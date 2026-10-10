// Copyright (c) 2026 Pedro Giffuni
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this
//    list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! Allocation *policy*: deciding where a new block or inode should go.
//!
//! # Policy is not allocation
//!
//! Everything in this module is a **pure function** of
//!
//! * the superblock ([`Superblock`]),
//! * a snapshot of the per-cylinder-group counters ([`CgSums`]),
//! * runtime hints ([`AllocationSummary`]), and
//! * a description of the request ([`BlockPrefInput`], [`DirPrefInput`]).
//!
//! Nothing in this module reads or writes an allocation bitmap, and nothing in
//! this module is allowed to fail: a policy always returns a *preference*,
//! possibly the uninformative one ("cylinder group 0, anywhere in it"), and it
//! is the allocator's job to satisfy that preference or to walk away from it.
//! Keeping the two apart is what makes the policy exhaustively unit-testable
//! without constructing a filesystem image, and it is the split FreeBSD
//! deliberately makes between `ffs_blkpref_ufs2()`/`ffs_dirpref()` (which
//! return a `daddr_t`/`ino_t` and touch nothing but in-core counters) and
//! `ffs_alloc()`/`ffs_hashalloc()`/`ffs_alloccgblk()` (which mutate the
//! bitmaps).  Conflating them is the classic way to end up with an allocator
//! whose fallback behaviour is impossible to reason about.
//!
//! # Reference
//!
//! FreeBSD's `sys/ufs/ffs/ffs_alloc.c` was used as a behavioural specification.
//! `pref_block()` re-derives `ffs_blkpref_ufs2()` and `pref_dir()` re-derives
//! `ffs_dirpref()`; the Rust code below is written independently, with
//! different types, explicit fallback return values and (where noted)
//! documented divergences.

use crate::{
	data::*,
	geom::{AllocationSummary, CgNum},
};

/// What a newly allocated block will be used for.
///
/// The role decides *which zone* of a cylinder group the block should come
/// from.  This matters because UFS2 splits every cylinder group into a
/// metadata zone (`cgmeta()..cgdata()`) and a data zone
/// (`cgdata()..cgend()`), and because the metadata zone is a *preference* that
/// is allowed to spill: nothing here reserves space, it only expresses what
/// would be nice.
///
/// The four roles are genuinely different and are *not* interchangeable:
///
/// * **File data** is bulk payload.  It goes in the data zone so that the
///   metadata zone stays small and so that large files do not evict metadata
///   from the blocks adjacent to the cylinder-group superblock.
/// * **Directory data** is read on almost every path lookup and written on
///   almost every mutating operation.  It goes in the metadata zone so that
///   `fsck` phase 2, which has just read the cylinder-group superblock, can
///   find the directory with a sequential read, and so that a directory's
///   blocks are not scattered across the whole filesystem.
/// * **Indirect blocks** are metadata *and* are the one structure whose
///   location is load-bearing for random access: see
///   [`BlockRole::Indirect`].
/// * **Extended-attribute blocks** are metadata that is addressed from the
///   inode's `extb[]` array and read as a contiguous run, so they follow the
///   same reasoning as directory data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRole {
	/// A block of ordinary file payload, reached directly or through an
	/// indirect pointer.
	FileData,

	/// A block belonging to a directory's data.
	DirectoryData,

	/// An indirect (block-pointer) block.
	///
	/// `first` distinguishes the *first* indirect block — the one pointed at
	/// by `indirect[0]`, covering logical blocks `UFS_NDADDR ..
	/// UFS_NDADDR + nindir` — from every later one.  The first indirect block
	/// gets special treatment: it is deliberately placed *inline*, right
	/// after the last direct block, because the files that need it are
	/// exactly the files whose first `UFS_NDADDR + nindir` blocks are already
	/// contiguous.  Later indirect blocks go to the metadata zone of the
	/// inode's cylinder group instead, because by the time a file reaches the
	/// second indirect level it is large enough that its metadata zone will
	/// already be busy and its data has spilled into other cylinder groups.
	Indirect { first: bool },

	/// A block of an inode's extended-attribute area.
	ExtendedAttribute,
}

// `is_metadata` is exercised by the tests below and by the soft-update layer
// that distinguishes "the block's contents are metadata" from "the block is a
// pointer target".
#[allow(dead_code)]
impl BlockRole {
	/// Whether this role's blocks are metadata, i.e. whether they belong in
	/// the metadata zone rather than the data zone.
	pub fn is_metadata(self) -> bool {
		matches!(
			self,
			Self::DirectoryData | Self::Indirect { .. } | Self::ExtendedAttribute
		)
	}
}

/// A preference for where a block should be allocated.
///
/// `blk == 0` means "no particular block; the allocator may choose anywhere
/// within `cg`".  This mirrors FreeBSD's convention of passing `bpref == 0`
/// into `ffs_alloccgblk()`, which then falls back to `cgbase + cg_rotor +
/// fs_frag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPref {
	/// The preferred cylinder group.
	pub cg: CgNum,

	/// The preferred filesystem block, or 0 for "anywhere in `cg`".
	pub blk: u64,
}

impl BlockPref {
	/// "Anywhere in cylinder group `cg`."
	pub fn anywhere(cg: CgNum) -> Self {
		Self { cg, blk: 0 }
	}

	/// This exact block.
	pub fn exact(cg: CgNum, blk: u64) -> Self {
		Self { cg, blk }
	}

	/// The CG's metadata zone, unanchored.
	pub fn meta(sb: &Superblock, cg: CgNum) -> Self {
		Self {
			cg,
			blk: sb.cg_meta_start(cg),
		}
	}

	/// The CG's data zone, unanchored.
	pub fn data(sb: &Superblock, cg: CgNum) -> Self {
		Self {
			cg,
			blk: sb.cg_data_start(cg),
		}
	}
}

/// Everything [`pref_block`] needs to know about one block request.
///
/// This is a snapshot rather than a borrow of the inode on purpose: the policy
/// must be callable with stale or partially-initialised state (see
/// [`BlockRole::Indirect`], where `last_direct` is read from an inode whose
/// `indirect[0]` has not been allocated yet), and a value type makes that
/// explicit and testable.
#[derive(Debug, Clone, Copy)]
pub struct BlockPrefInput<'a> {
	/// What the block is for.
	pub role: BlockRole,

	/// The inode the block belongs to.  Its cylinder group anchors the
	/// preference: keeping a file's blocks near its inode is the single
	/// biggest locality win in a rotational-geometry filesystem.
	pub inr: InodeNum,

	/// Logical block number being allocated.
	pub lbn: u64,

	/// Filesystem block of the *previous* logical block, or 0 if that block is
	/// a hole (or is itself being allocated).  A hole breaks the run, and the
	/// policy then has to pick a fresh starting point.
	pub prev: u64,

	/// `direct[UFS_NDADDR - 1]` of the inode.
	pub last_direct: u64,

	/// `indirect[0]` of the inode, or 0 if the first indirect block does not
	/// exist yet.
	pub first_indirect: u64,

	/// Per-cylinder-group free-block counters, needed by the
	/// "find a cylinder group with more than the average number of free
	/// blocks" step.
	pub cgs: &'a CgSums,
}

/// Index of the most significant set bit, counted from 1.
///
/// This is the `fls()` of the traditional BSD bit-scan family: `fls(1) == 1`,
/// `fls(2) == 2`, `fls(3) == 2`.  `0` maps to `0` so that callers can detect
/// the degenerate case instead of shifting by `-1`.
pub const fn fls(n: u64) -> u32 {
	64 - n.leading_zeros()
}

/// FreeBSD's `ffs_blkpref_ufs2()`, independently re-derived.
///
/// # The algorithm
///
/// The file is divided into *sections*: the `UFS_NDADDR` direct blocks are
/// section 0, the `nindir` blocks behind the first indirect are section 1, and
/// each subsequent indirect level starts a new section.  The policy has three
/// distinct behaviours, chosen in this order:
///
/// 1. **Indirect blocks.**  Prefer the metadata zone of the inode's cylinder
///    group.  The *first* indirect block instead prefers to sit immediately
///    after the last direct block, which keeps the common case (a file that
///    just grew past 12 blocks) fully contiguous.
///
/// 2. **First block behind the first indirect.**  If the first indirect block
///    was itself allocated in the data area, prefer to put the first data
///    block behind it immediately, so the whole of section 1 is contiguous.
///
/// 3. **Everything else.**  If the previous logical block is allocated *and* we
///    have not yet allocated `fs_maxbpg` blocks in this section, prefer
///    `prev + fs_frag`, i.e. contiguity.  Otherwise — at the start of the
///    file, after a hole, or once a section is full — pick a new starting
///    point:
///
///    * a directory's block goes to the metadata zone of the inode's CG;
///    * a file still inside section 1 goes to the data zone of the inode's CG;
///    * anything further out goes to the data zone of *some* cylinder group
///      with at least the average number of free blocks, searching forward
///      from `inocg + lbn / fs_maxbpg` (or from the CG after `prev`'s) and
///      wrapping.
///
/// Returning `BlockPref { cg: 0, blk: 0 }` means "no preference at all"; the
/// cylinder-overflow search in the allocator then takes over.
///
/// # Crash safety
///
/// This function only chooses an address.  The address is worthless until the
/// block's bitmap bits have been cleared *and* its contents have been
/// initialised; see [`crate::softdep::NewBlockDep`].
pub fn pref_block(sb: &Superblock, inp: BlockPrefInput<'_>) -> BlockPref {
	let inocg = sb.ino_to_cg(inp.inr);
	let ncg = sb.ncg as u64;
	let frag = sb.frag();

	// (1) Indirect blocks.
	if let BlockRole::Indirect { first } = inp.role {
		let mut blk = sb.cg_meta_start(inocg);
		// The first indirect block follows the direct blocks when they exist:
		// a file that needs one has just filled all 12 direct slots, so those
		// are contiguous and the natural place for the pointer table is right
		// after them.
		if first && inp.lbn < UFS_NDADDR as u64 + sb.nindir() && inp.last_direct != 0 {
			blk = inp.last_direct + frag;
			if sb.blk_to_cg(blk).get() as u64 >= ncg {
				// Would run off the end of the filesystem; fall back to the
				// metadata zone below.
				blk = sb.cg_meta_start(inocg);
			}
		}
		return BlockPref::exact(inocg, blk);
	}

	// (2) The first data block behind the first indirect block.
	if inp.lbn == UFS_NDADDR as u64 {
		let x1 = inp.first_indirect;
		// Only if the indirect block landed in the data area *and* in the
		// inode's own cylinder group can "the next block" be meaningful.
		if x1 >= sb.cg_data_start(inocg) && x1 < sb.cg_base(inocg.next()) {
			let cand = x1 + frag;
			if sb.blk_to_cg(cand).get() as u64 >= ncg {
				return BlockPref::anywhere(CgNum::new(0));
			}
			return BlockPref::exact(inocg, cand);
		}
	}

	// (3) Ordinary data.
	let prev = inp.prev;
	let maxbpg = sb.fs_maxbpg as u64;
	let new_section = maxbpg == 0 || inp.lbn.is_multiple_of(maxbpg) || prev == 0;

	if !new_section {
		// Contiguous run inside the current section.
		let cand = prev + frag;
		if sb.blk_to_cg(cand).get() as u64 >= ncg {
			return BlockPref::anywhere(CgNum::new(0));
		}
		return BlockPref::exact(inocg, cand);
	}

	// Directory data is metadata: keep it in the metadata zone of the inode's
	// own cylinder group, both for locality and so that `fsck` phase 2 finds
	// it where it expects.
	if inp.role == BlockRole::DirectoryData {
		return BlockPref::meta(sb, inocg);
	}

	// Extended attributes are addressed as a run from the inode, so they get
	// the same treatment as directory data.
	if inp.role == BlockRole::ExtendedAttribute {
		return BlockPref::meta(sb, inocg);
	}

	// Sections 0 and 1 live in the inode's cylinder group, in the data area.
	if inp.lbn < UFS_NDADDR as u64 + sb.nindir() {
		return BlockPref::data(sb, inocg);
	}

	// Later sections: spread out, but only into a cylinder group that is not
	// worse than average, so that a large file cannot pin the filesystem
	// around its own inode's cylinder group.
	let startcg = if prev == 0 {
		(inocg.get() as u64 + inp.lbn / maxbpg) % ncg
	} else {
		(sb.blk_to_cg(prev).get() as u64 + 1) % ncg
	};

	let avgbfree = sb.cstotal.nbfree.max(0) as u64 / ncg;
	for cg in startcg..ncg {
		if inp.cgs.nbfree(cg) >= avgbfree as i32 {
			return BlockPref::data(sb, CgNum::new(cg as u32));
		}
	}
	for cg in 0..startcg {
		if inp.cgs.nbfree(cg) >= avgbfree as i32 {
			return BlockPref::data(sb, CgNum::new(cg as u32));
		}
	}

	// Nothing is at or above average (a nearly-full filesystem).  Leave the
	// choice entirely to the cylinder-overflow search.
	BlockPref::anywhere(CgNum::new(0))
}

/// A snapshot of the per-cylinder-group allocation counters.
///
/// These come from the on-disk `cg_cs` of each cylinder-group superblock, but
/// they are gathered into a flat slice here because the policies need to scan
/// *all* cylinder groups and reading a 32 KiB cylinder-group superblock per
/// comparison would be absurd.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgSums {
	ndir:   Vec<i32>,
	nbfree: Vec<i32>,
	nifree: Vec<i32>,
}

#[allow(dead_code)]
impl CgSums {
	/// Build from per-CG `csum`s, indexed by cylinder group.
	pub fn new(sums: &[Csum]) -> Self {
		Self {
			ndir:   sums.iter().map(|c| c.ndir).collect(),
			nbfree: sums.iter().map(|c| c.nbfree).collect(),
			nifree: sums.iter().map(|c| c.nifree).collect(),
		}
	}

	/// Number of cylinder groups covered.
	pub fn len(&self) -> usize {
		self.nbfree.len()
	}

	/// Whether this snapshot covers no cylinder groups.
	pub fn is_empty(&self) -> bool {
		self.nbfree.is_empty()
	}

	/// Number of directories in cylinder group `cg`.
	pub fn ndir(&self, cg: u64) -> i32 {
		self.ndir.get(cg as usize).copied().unwrap_or(0)
	}

	/// Number of free blocks in cylinder group `cg`.
	pub fn nbfree(&self, cg: u64) -> i32 {
		self.nbfree.get(cg as usize).copied().unwrap_or(0)
	}

	/// Number of free inodes in cylinder group `cg`.
	pub fn nifree(&self, cg: u64) -> i32 {
		self.nifree.get(cg as usize).copied().unwrap_or(0)
	}

	/// Replace the counters of cylinder group `cg`.
	///
	/// Called from the single choke point that writes a cylinder-group
	/// superblock, so the cache cannot drift from the disk.
	pub fn set(&mut self, cg: CgNum, csum: Csum) {
		let i = cg.get() as usize;
		if i >= self.nbfree.len() {
			return;
		}
		self.ndir[i] = csum.ndir;
		self.nbfree[i] = csum.nbfree;
		self.nifree[i] = csum.nifree;
	}
}

/// Where a new inode should be placed: the inputs to [`pref_inode`].
#[derive(Debug, Clone, Copy)]
pub struct InodePrefInput<'a> {
	/// The parent directory, if this inode is being created inside one.
	/// `None` means "no parent" (root, or an inode being allocated by
	/// `fsck`/recovery), which makes the policy fall back to a
	/// filesystem-wide search.
	pub parent: Option<ParentInfo>,

	/// Whether the new inode is itself a directory.  Only directories need
	/// the dirpref placement below; ordinary files follow their parent's
	/// cylinder group, which is what keeps a directory and its contents
	/// together.
	pub is_dir: bool,

	/// Per-cylinder-group counters.
	pub cgs: &'a CgSums,

	/// Runtime-only allocation bookkeeping.
	pub alloc: &'a AllocationSummary,
}

/// What is known about the parent directory of a new inode.
#[derive(Debug, Clone, Copy)]
pub struct ParentInfo {
	/// The parent's inode number.
	pub inr: InodeNum,

	/// The parent's depth below the root directory; the root itself is 0.
	///
	/// This is the on-disk `i_dirdepth`, which for UFS2 shares the `di_ignored`
	/// word with the soft-update journal's "next unlinked inode" pointer.  A
	/// non-directory inode always reads 0 there.
	pub depth: u32,

	/// The parent's link count.
	///
	/// For a directory this counts `.`, `..` and one entry per child
	/// directory, so it is a good proxy for "how many directories have already
	/// been created directly inside me".
	pub nlink: u16,
}

/// FreeBSD's `ffs_dirpref()` — a "dirpref change" in FreeBSD's own words —
/// independently re-derived.
///
/// # Why directories need a policy at all
///
/// Ordinary files are placed next to their parent directory, which is good
/// enough: a file's blocks are only read through its parent anyway.  Directories
/// are different — they are the *only* inode whose location is chosen before
/// any of its contents exist, so if every new directory landed in the cylinder
/// group of its parent, a workload like
///
/// ```text
/// mkdir -p a/b/c/d/e/.../z
/// ```
///
/// would put the entire tree in one cylinder group and leave the rest of the
/// filesystem as one long contiguous file-data region.  The dirpref scheme
/// this by making the *distance* between a directory and its parent shrink as
/// the tree gets deeper.
///
/// # The algorithm
///
/// Let `depth` be the parent's depth and `curcg` its cylinder group.
///
/// * Pick a **search range** of `ncg / 2^depth` cylinder groups centred on
///   `curcg`.  A child of the root therefore searches the whole filesystem, a
///   child of a first-level directory searches half of it, and once `depth`
///   reaches `log2(ncg)` the range collapses to 1 and every descendant lands
///   in its parent's cylinder group.  That collapse *is* the clustering: deep
///   trees are expected to be accessed as a unit.
///
/// * Pick a **point** in that range by walking the van der Corput / bit
///   reversal sequence indexed by `nlink - 1`, i.e. `1/2, 1/4, 3/4, 1/8, 3/8,
///   5/8, 7/8, …`.  This is why a burst of `mkdir`s fans out symmetrically
///   instead of marching in one direction.
///
/// * Accept a cylinder group only if it satisfies four limits, all derived
///   from filesystem-wide averages so that they adapt to how full the
///   filesystem is:
///
///   | limit | meaning |
///   |---|---|
///   | `ndir < avgndir + 2^depth` | do not let one CG hoard directories |
///   | `nifree >= avgifree - avgifree/4` | leave the CG room for files |
///   | `nbfree >= avgbfree - avgbfree/4` | leave the CG room for file data |
///   | `contigdirs < maxcontigdirs` | stop a *run* of directories in one CG |
///
///   `maxcontigdirs` is the number of directories whose data would fit in the
///   CG's free space at the observed average directory size, capped by
///   `ipg / avgfpdir`.  Creating a directory costs its own inode and the
///   inodes of its (future) entries; a CG that is already the parent of
///   `maxcontigdirs` directories has, by construction, had its free space
///   reserved for them.
///
/// * Search forward from the preferred CG to the end, then from the start to
///   the preferred CG.  Scanning *forward first* is deliberate: it means a
///   nearly-full filesystem touches each CG at most twice per search instead
///   of repeatedly re-examining the full CGs nearest the preferred one.
///
/// * If nothing satisfies the limits, fall back to "any CG with at least the
///   average number of free inodes", and finally to CG 0.  Placing a directory
///   somewhere imperfect is better than failing `mkdir` with `ENOSPC`.
///
/// # Divergence from FreeBSD
///
/// * FreeBSD uses the in-core `i_effnlink`; we use the on-disk `i_nlink`,
///   which for a directory is what `i_effnlink` is derived from anyway
///   (`i_effnlink` is also bumped by `ufs_backgroundinode` for unlinked
///   snapshots, which we do not implement).
/// * FreeBSD computes `range = ncg >> depth` with a C shift; we saturate at 63
///   so that a corruptly deep `i_dirdepth` cannot produce a shift overflow.
/// * The `depth == 0 && parent != root` legacy special case is preserved.
///
/// # Crash safety
///
/// Returning a cylinder group is only half the job; the returned inode must be
/// marked used in `cg_iused[]` *before* any directory entry may reference it.
/// See [`crate::softdep::DirAddDep`].
pub fn pref_inode(sb: &Superblock, inp: InodePrefInput<'_>) -> CgNum {
	let ncg = sb.ncg as u64;
	if ncg == 0 {
		return CgNum::new(0);
	}

	let parent = match inp.parent {
		Some(p) => p,
		// No parent to anchor on: spread directories across the filesystem
		// from CG 0, and put everything else in CG 0 as well.
		None => return CgNum::new(0),
	};

	// Ordinary files follow their parent, so that a directory and the files
	// directly inside it share a cylinder group.
	if !inp.is_dir {
		return sb.ino_to_cg(parent.inr);
	}

	let curcg = sb.ino_to_cg(parent.inr).get() as u64;
	let depth = parent.depth;

	let avgifree = sb.cstotal.nifree.max(0) as u64 / ncg;
	let avgbfree = sb.cstotal.nbfree.max(0) as u64 / ncg;
	let avgndir = sb.cstotal.ndir.max(0) as u64 / ncg;

	// Search range, halved once per level of depth.
	let shift = depth.min(63);
	let range = ncg >> shift;
	let half = range / 2;

	let start = (curcg + ncg - half) % ncg;
	let end = (curcg + half) % ncg;

	// Point in the range from the bit-reversal sequence.  numdirs is the
	// parent's link count minus one: a directory with `.`, `..` and no
	// subdirectories has nlink 2, so its first child is placed at the middle
	// of the range.
	let numdirs = (parent.nlink as u64).saturating_sub(1).max(1);
	let power = fls(numdirs) as u64;
	let mask = 1u64 << (power - 1);
	let numerator = (numdirs & !mask) * 2 + 1;
	let denominator = 1u64 << power;

	let mut prefcg = (start + range * numerator / denominator) % ncg;

	// A filesystem that does not track directory depths (i_dirdepth == 0 on a
	// non-root parent, which happens on images made by an old newfs or
	// written by a driver that never implemented depth) gets the historical
	// "stick with the parent" behaviour.
	if depth == 0 && parent.inr != InodeNum::ROOT {
		prefcg = curcg;
	}

	let _ = end; // `end` documents the range; the search below is prefcg-relative.

	let ipg = sb.ipg();
	let maxndir = (avgndir + (1u64 << shift)).min(ipg);
	let minifree = (avgifree - avgifree / 4).max(1) as i32;
	let minbfree = (avgbfree - avgbfree / 4).max(1) as i32;

	// How many directories' worth of data still fits in an average CG, given
	// how many directories that CG already holds.  This is the estimate of
	// "space already promised to directories" that `maxcontigdirs` protects
	// against over-committing.
	let cgsize = sb.fsize() * sb.fpg();
	let avgbfree_bytes = avgbfree * sb.bsize();
	let mut dirsize = sb.avgfilesize as u64 * sb.avgfpdir as u64;
	let curdirsize = if avgndir != 0 && cgsize > avgbfree_bytes {
		(cgsize - avgbfree_bytes) / avgndir
	} else {
		0
	};
	if dirsize < curdirsize {
		dirsize = curdirsize;
	}
	let mut maxcontigdirs = if dirsize == 0 {
		0
	} else {
		avgbfree_bytes.checked_div(dirsize).unwrap_or(0).min(255)
	};
	if sb.avgfpdir > 0 {
		maxcontigdirs = maxcontigdirs.min(ipg / sb.avgfpdir as u64);
	}
	let maxcontigdirs = maxcontigdirs.max(1) as u32;

	let ok = |cg: u64, cgs: &CgSums, alloc: &AllocationSummary| -> bool {
		cgs.ndir(cg) < maxndir as i32 &&
			cgs.nifree(cg) >= minifree &&
			cgs.nbfree(cg) >= minbfree &&
			alloc.contig_dirs.get(cg as usize).copied().unwrap_or(0) < maxcontigdirs as u8
	};

	// Forward from the preferred CG, then wrap.  See the note in the module
	// docs: forward-first keeps a full filesystem from re-scanning the same
	// full cylinder groups on every request.
	for cg in prefcg..ncg {
		if ok(cg, inp.cgs, inp.alloc) {
			return CgNum::new(cg as u32);
		}
	}
	for cg in 0..prefcg {
		if ok(cg, inp.cgs, inp.alloc) {
			return CgNum::new(cg as u32);
		}
	}

	// Space deficit: any CG with at least the average number of free inodes.
	let avgifree = avgifree as i32;
	for cg in prefcg..ncg {
		if inp.cgs.nifree(cg) >= avgifree {
			return CgNum::new(cg as u32);
		}
	}
	for cg in 0..prefcg {
		if inp.cgs.nifree(cg) >= avgifree {
			return CgNum::new(cg as u32);
		}
	}

	CgNum::new(prefcg as u32)
}

#[cfg(test)]
mod t {
	use super::*;

	/// Build a synthetic filesystem.  Four cylinder groups is too few to
	/// exercise the interesting parts of the dirpref policy, so tests below
	/// scale it up; the geometry is otherwise the same as a `newfs -b 32768
	/// -f 4096` image.
	fn sb(ncg: u32) -> Superblock {
		let mut sb = crate::geom::tests::superblock_for_tests();
		sb.ncg = ncg;
		sb.fpg = 264;
		sb.ipg = 256;
		sb.metaspace = 8;
		sb.avgfilesize = 16384;
		sb.avgfpdir = 64;
		sb.cstotal.nifree = 200 * ncg as i64;
		sb.cstotal.nbfree = 100 * ncg as i64;
		sb.cstotal.ndir = 8 * ncg as i64;
		sb
	}

	fn sums(ncg: u32, f: impl Fn(u64) -> Csum) -> CgSums {
		let v: Vec<Csum> = (0..ncg as u64).map(f).collect();
		CgSums::new(&v)
	}

	fn uniform(ncg: u32) -> CgSums {
		sums(ncg, |_| {
			Csum {
				ndir:   8,
				nbfree: 100,
				nifree: 200,
				nffree: 0,
			}
		})
	}

	fn pref_of(
		sb: &Superblock,
		role: BlockRole,
		inr: u32,
		lbn: u64,
		prev: u64,
		cgs: &CgSums,
	) -> BlockPref {
		pref_block(
			sb,
			BlockPrefInput {
				role,
				inr: unsafe { InodeNum::new(inr) },
				lbn,
				prev,
				last_direct: 0,
				first_indirect: 0,
				cgs,
			},
		)
	}

	// ---------------------------------------------------------------- blocks

	/// Direct file data goes in the data zone of the inode's cylinder group.
	#[test]
	fn file_data_prefers_inode_cg_data_zone() {
		let sb = sb(8);
		let cgs = uniform(8);
		for lbn in 0..UFS_NDADDR as u64 {
			let inr = unsafe { InodeNum::new(300) }; // CG 1, since ipg == 256
			let p = pref_of(&sb, BlockRole::FileData, inr.get(), lbn, 0, &cgs);
			assert_eq!(p.cg, CgNum::new(1));
			assert_eq!(p.blk, sb.cg_data_start(CgNum::new(1)));
		}
	}

	/// Directory data goes in the *metadata* zone of the inode's cylinder
	/// group — never in the data zone, never in another CG.
	#[test]
	fn dir_data_prefers_inode_cg_meta_zone() {
		let sb = sb(8);
		let cgs = uniform(8);
		for lbn in [0u64, 1, 500] {
			let p = pref_of(&sb, BlockRole::DirectoryData, 300, lbn, 0, &cgs);
			assert_eq!(p.cg, CgNum::new(1));
			assert_eq!(p.blk, sb.cg_meta_start(CgNum::new(1)));
			assert!(p.blk < sb.cg_data_start(CgNum::new(1)));
		}
	}

	/// Extended-attribute blocks are metadata too.
	#[test]
	fn extattr_prefers_meta_zone() {
		let sb = sb(8);
		let cgs = uniform(8);
		let p = pref_of(&sb, BlockRole::ExtendedAttribute, 300, 0, 0, &cgs);
		assert_eq!(p.blk, sb.cg_meta_start(CgNum::new(1)));
	}

	/// A *later* indirect block prefers the inode CG's metadata zone even when
	/// the file has spilled into other cylinder groups.
	#[test]
	fn later_indirect_prefers_meta_zone() {
		let sb = sb(8);
		let cgs = uniform(8);
		let inr = unsafe { InodeNum::new(300) };
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role: BlockRole::Indirect { first: false },
				inr,
				lbn: 100_000,
				prev: 0,
				last_direct: 0,
				first_indirect: 0,
				cgs: &cgs,
			},
		);
		assert_eq!(p.cg, CgNum::new(1));
		assert_eq!(p.blk, sb.cg_meta_start(CgNum::new(1)));
	}

	/// The *first* indirect block is placed immediately after the last direct
	/// block, keeping section 1 contiguous with section 0.
	#[test]
	fn first_indirect_follows_last_direct() {
		let sb = sb(8);
		let cgs = uniform(8);
		let last_direct = sb.cg_data_start(CgNum::new(1)) + 11 * sb.frag();
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role: BlockRole::Indirect { first: true },
				inr: unsafe { InodeNum::new(300) },
				lbn: UFS_NDADDR as u64,
				prev: 0,
				last_direct,
				first_indirect: 0,
				cgs: &cgs,
			},
		);
		assert_eq!(p.cg, CgNum::new(1));
		assert_eq!(p.blk, last_direct + sb.frag());
	}

	/// If the last direct block is the last fragment of the filesystem, the
	/// "right after it" preference would run off the end; the policy must fall
	/// back to the metadata zone instead of producing an out-of-range block.
	#[test]
	fn first_indirect_at_end_of_fs_falls_back() {
		let sb = sb(2);
		let cgs = uniform(2);
		let end = sb.cg_end(CgNum::new(1)) - sb.frag();
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role:           BlockRole::Indirect { first: true },
				inr:            unsafe { InodeNum::new(256) },
				lbn:            UFS_NDADDR as u64,
				prev:           0,
				last_direct:    end,
				first_indirect: 0,
				cgs:            &cgs,
			},
		);
		assert_eq!(p.blk, sb.cg_meta_start(CgNum::new(1)));
		assert!(p.blk < sb.cg_end(CgNum::new(1)));
	}

	/// The first data block behind an inline first-indirect block goes right
	/// after it; if the indirect block lives in the metadata zone (or in
	/// another CG) there is nothing to continue from.
	#[test]
	fn first_indirect_data_follows_indirect_only_in_data_zone() {
		let sb = sb(8);
		let cgs = uniform(8);
		let inocg = CgNum::new(1);
		let x1 = sb.cg_data_start(inocg) + 3 * sb.frag();

		let p = pref_block(
			&sb,
			BlockPrefInput {
				role:           BlockRole::FileData,
				inr:            unsafe { InodeNum::new(300) },
				lbn:            UFS_NDADDR as u64,
				prev:           0,
				last_direct:    0,
				first_indirect: x1,
				cgs:            &cgs,
			},
		);
		assert_eq!(p.cg, inocg);
		assert_eq!(p.blk, x1 + sb.frag());

		// An indirect block in the metadata zone: no inline continuation.
		let x1 = sb.cg_meta_start(inocg);
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role:           BlockRole::FileData,
				inr:            unsafe { InodeNum::new(300) },
				lbn:            UFS_NDADDR as u64,
				prev:           0,
				last_direct:    0,
				first_indirect: x1,
				cgs:            &cgs,
			},
		);
		assert_eq!(p.cg, inocg);
		assert_eq!(p.blk, sb.cg_data_start(inocg));

		// An indirect block in a *different* cylinder group: also no
		// continuation.
		let x1 = sb.cg_data_start(CgNum::new(4)) + 3 * sb.frag();
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role:           BlockRole::FileData,
				inr:            unsafe { InodeNum::new(300) },
				lbn:            UFS_NDADDR as u64,
				prev:           0,
				last_direct:    0,
				first_indirect: x1,
				cgs:            &cgs,
			},
		);
		assert_eq!(p.cg, inocg);
		assert_eq!(p.blk, sb.cg_data_start(inocg));
	}

	/// Inside a section, allocation is contiguous.
	#[test]
	fn contiguous_run_is_preserved() {
		let sb = sb(8);
		let cgs = uniform(8);
		let maxbpg = sb.fs_maxbpg as u64;
		let prev = sb.cg_data_start(CgNum::new(2)) + 40 * sb.frag();
		// lbn == 1 is not a section boundary and prev != 0.
		let p = pref_of(&sb, BlockRole::FileData, 512, 1, prev, &cgs);
		assert_eq!(p.cg, CgNum::new(2));
		assert_eq!(p.blk, prev + sb.frag());

		// Hitting a section boundary restarts the search even with a prev.
		let p = pref_of(&sb, BlockRole::FileData, 512, maxbpg, prev, &cgs);
		assert_eq!(p.cg, CgNum::new(2));
		assert_eq!(p.blk, sb.cg_data_start(CgNum::new(2)));

		// A hole restarts the search too.
		let p = pref_of(&sb, BlockRole::FileData, 512, 1, 0, &cgs);
		assert_eq!(p.blk, sb.cg_data_start(CgNum::new(2)));
	}

	/// Beyond the first two sections the policy leaves the inode's CG and
	/// picks a cylinder group with at least the average number of free
	/// blocks, scanning forward and wrapping.
	#[test]
	fn later_sections_spread_to_better_cg() {
		let sb = sb(8);
		// CG0 is full, CG1..3 average, CG4 empty, CG5 average, CG6 average.
		let cgs = sums(8, |cg| {
			Csum {
				ndir:   8,
				nbfree: match cg {
					0 => 0,
					4 => 500,
					_ => 100,
				},
				nifree: 200,
				nffree: 0,
			}
		});
		let lbn = UFS_NDADDR as u64 + sb.nindir();
		// Inode 256 is in CG 1.
		let p = pref_of(&sb, BlockRole::FileData, 256, lbn, 0, &cgs);
		// startcg = (inocg + lbn / fs_maxbpg) % ncg
		let expected = (1 + lbn / sb.fs_maxbpg as u64) % 8;
		assert_eq!(p.cg, CgNum::new(expected as u32));
		assert_ne!(p.cg, CgNum::new(0), "CG 0 is full and must be skipped");
		assert_eq!(p.blk, sb.cg_data_start(p.cg));

		// If the sweep starts *at* the full CG 0 it must move on, not stall.
		let sb = {
			let mut sb = sb;
			sb.cstotal.nbfree = 100 * 8;
			sb
		};
		let p = pref_of(&sb, BlockRole::FileData, 256 - 256, lbn, 0, &cgs);
		assert_ne!(p.cg, CgNum::new(0));
	}

	/// When no cylinder group is at or above average, the policy reports "no
	/// preference" and leaves it to the cylinder-overflow search.
	#[test]
	fn later_sections_give_up_cleanly() {
		let sb = sb(4);
		let cgs = sums(4, |_| {
			Csum {
				ndir:   0,
				nbfree: 1,
				nifree: 0,
				nffree: 0,
			}
		});
		let lbn = UFS_NDADDR as u64 + sb.nindir();
		let p = pref_of(&sb, BlockRole::FileData, 256, lbn, 0, &cgs);
		assert_eq!(p, BlockPref::anywhere(CgNum::new(0)));
	}

	/// A preference that would run past the end of the filesystem degrades to
	/// "no preference" rather than naming an out-of-range block.
	#[test]
	fn preference_past_end_of_fs_is_dropped() {
		let sb = sb(2);
		let cgs = uniform(2);
		let last = sb.cg_end(CgNum::new(1)) - sb.frag();
		let p = pref_of(&sb, BlockRole::FileData, 256, 1, last, &cgs);
		assert_eq!(p, BlockPref::anywhere(CgNum::new(0)));

		let x1 = last;
		let p = pref_block(
			&sb,
			BlockPrefInput {
				role:           BlockRole::FileData,
				inr:            unsafe { InodeNum::new(256) },
				lbn:            UFS_NDADDR as u64,
				prev:           0,
				last_direct:    0,
				first_indirect: x1,
				cgs:            &cgs,
			},
		);
		assert_eq!(p, BlockPref::anywhere(CgNum::new(0)));
	}

	#[test]
	fn roles_classify_metadata() {
		assert!(!BlockRole::FileData.is_metadata());
		assert!(BlockRole::DirectoryData.is_metadata());
		assert!(BlockRole::Indirect { first: true }.is_metadata());
		assert!(BlockRole::ExtendedAttribute.is_metadata());
	}

	#[test]
	fn fls_is_one_based() {
		assert_eq!(fls(0), 0);
		assert_eq!(fls(1), 1);
		assert_eq!(fls(2), 2);
		assert_eq!(fls(3), 2);
		assert_eq!(fls(4), 3);
		assert_eq!(fls(7), 3);
		assert_eq!(fls(8), 4);
		assert_eq!(fls(u64::MAX), 64);
	}

	// ------------------------------------------------------------ directories

	/// The cylinder group the dirpref *point* computation selects, before any
	/// the acceptance limits are applied.  Recomputed here so that the
	/// ndir-limit test can assert that the preferred CG is really being
	/// skipped rather than merely never chosen.
	fn prefcg_for(sb: &Superblock, parent_inr: u32, nlink: u16) -> u32 {
		let curcg = sb.ino_to_cg(unsafe { InodeNum::new(parent_inr) }).get() as u64;
		let ncg = sb.ncg as u64;
		let range = ncg;
		let half = range / 2;
		let start = (curcg + ncg - half) % ncg;
		let numdirs = (nlink as u64).saturating_sub(1).max(1);
		let power = fls(numdirs) as u64;
		let mask = 1u64 << (power - 1);
		let numerator = (numdirs & !mask) * 2 + 1;
		((start + range * numerator / (1u64 << power)) % ncg) as u32
	}

	fn dir_pref(
		sb: &Superblock,
		parent_inr: u32,
		depth: u32,
		nlink: u16,
		cgs: &CgSums,
		alloc: &AllocationSummary,
	) -> CgNum {
		pref_inode(
			sb,
			InodePrefInput {
				parent: Some(ParentInfo {
					inr: unsafe { InodeNum::new(parent_inr) },
					depth,
					nlink,
				}),
				is_dir: true,
				cgs,
				alloc,
			},
		)
	}

	/// Non-directories go to the parent's cylinder group, always.
	#[test]
	fn files_follow_parent_cg() {
		let sb = sb(16);
		let cgs = sums(16, |_| {
			Csum {
				ndir:   999,
				nbfree: 0,
				nifree: 0,
				nffree: 0,
			}
		});
		let alloc = AllocationSummary::new(16);
		let cg = pref_inode(
			&sb,
			InodePrefInput {
				parent: Some(ParentInfo {
					inr:   unsafe { InodeNum::new(1000) }, // CG 3
					depth: 5,
					nlink: 2,
				}),
				is_dir: false,
				cgs:    &cgs,
				alloc:  &alloc,
			},
		);
		assert_eq!(cg, CgNum::new(3));
	}

	/// The root directory spreads its children across the whole filesystem.
	///
	/// With 16 CGs and depth 0 the search range is `[-8, +8]` around CG 0,
	/// i.e. the whole filesystem.  The preferred point walks the bit-reversal
	/// sequence `1/2, 1/4, 3/4, 1/8, 3/8, 5/8, 7/8` as the root's link count
	/// grows, which is the "shallow directories are distributed broadly"
	/// property.
	#[test]
	fn root_children_are_spread_by_the_bitreversal_sequence() {
		let sb = sb(16);
		let cgs = uniform(16);
		let alloc = AllocationSummary::new(16);

		// nlink 2..8 -> numdirs 1..7 -> fractions 1/2, 1/4, 3/4, 1/8, 3/8,
		// 5/8, 7/8 of a 16-CG range centred on CG 0.
		let want = [0u32, 12, 4, 10, 14, 2, 6];
		for (i, &cg) in want.iter().enumerate() {
			let nlink = 2 + i as u16;
			assert_eq!(
				dir_pref(&sb, InodeNum::ROOT.get(), 0, nlink, &cgs, &alloc),
				CgNum::new(cg),
				"root child {i} (nlink {nlink})"
			);
		}

		// ...and all seven land in seven *distinct* cylinder groups.
		let used: std::collections::BTreeSet<u32> = (2..=8)
			.map(|nl| dir_pref(&sb, InodeNum::ROOT.get(), 0, nl, &cgs, &alloc).get())
			.collect();
		assert_eq!(used.len(), 7);
	}

	/// As depth increases the search range shrinks, so children drift towards
	/// the parent; once `2^depth >= ncg` they land in the parent's CG.
	#[test]
	fn deeper_directories_cluster_on_their_parent() {
		let sb = sb(16);
		let cgs = uniform(16);
		let alloc = AllocationSummary::new(16);

		// Parent in CG 5.
		let parent = 5 * 256;
		// depth 1: range 8, centred on 5 -> [1..9], first child at the middle
		// of the range, i.e. 5 itself.
		assert_eq!(dir_pref(&sb, parent, 1, 2, &cgs, &alloc), CgNum::new(5));
		// depth 2: range 4 -> [3..7] -> middle 5.
		assert_eq!(dir_pref(&sb, parent, 2, 2, &cgs, &alloc), CgNum::new(5));
		// depth 4: range 1 -> [5..5].
		assert_eq!(dir_pref(&sb, parent, 4, 2, &cgs, &alloc), CgNum::new(5));
		// depth >= log2(ncg): the range has collapsed entirely.
		assert_eq!(dir_pref(&sb, parent, 10, 2, &cgs, &alloc), CgNum::new(5));

		// ...but at depth 1 a *second* child moves a quarter of the (halved)
		// range away, so the siblings are not all stacked.
		assert_ne!(
			dir_pref(&sb, parent, 1, 2, &cgs, &alloc),
			dir_pref(&sb, parent, 1, 3, &cgs, &alloc)
		);
	}

	/// A non-root parent at depth 0 means the filesystem does not track
	/// directory depth; the policy then keeps the historical "same CG" rule.
	#[test]
	fn untracked_depth_falls_back_to_parent_cg() {
		let sb = sb(16);
		let cgs = uniform(16);
		let alloc = AllocationSummary::new(16);
		assert_eq!(dir_pref(&sb, 7 * 256, 0, 5, &cgs, &alloc), CgNum::new(7));
		// ...but the root at depth 0 still spreads (nlink 5 -> 1/8 of the
		// range -> CG 10).
		assert_eq!(
			dir_pref(&sb, InodeNum::ROOT.get(), 0, 5, &cgs, &alloc),
			CgNum::new(10)
		);
	}

	/// `contig_dirs` is a run counter: a burst of `mkdir`s into one CG is
	/// eventually refused, forcing the next directory elsewhere.
	#[test]
	fn contigdirs_throttles_directory_runs() {
		let sb = sb(8);
		let cgs = uniform(8);
		let mut alloc = AllocationSummary::new(8);

		// This geometry yields maxcontigdirs == 3 (see the derivation in
		// pref_inode): avgbfree*bsize / (avgfilesize*avgfpdir) == 3, capped by
		// ipg/avgfpdir == 4.
		let limit = 3;

		let mut run = 0u32;
		let mut maxrun = 0u32;
		let mut last: Option<u32> = None;
		let mut seen = std::collections::BTreeSet::new();
		for _ in 0..8 * 3 {
			let cg = dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc);
			seen.insert(cg.get());
			if Some(cg.get()) == last {
				run += 1;
			} else {
				run = 1;
				last = Some(cg.get());
			}
			maxrun = maxrun.max(run);
			alloc.note_inode_alloc(cg, true);
		}
		assert!(
			maxrun <= limit,
			"directory run of {maxrun} exceeded {limit}"
		);
		// And the run limiter must actually be doing something: without it a
		// uniform filesystem would happily stack directories.
		assert!(seen.len() >= 4, "only {} cylinder groups used", seen.len());
	}

	/// Interleaving a file creation resets `contig_dirs`, so a
	/// mkdir/create/mkdir pattern is not throttled by the directory counter.
	#[test]
	fn file_allocations_reset_contigdirs() {
		let sb = sb(8);
		let cgs = uniform(8);
		let mut alloc = AllocationSummary::new(8);
		let cg = dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc);
		alloc.note_inode_alloc(cg, true);
		alloc.note_inode_alloc(cg, false);
		assert_eq!(alloc.contig_dirs[cg.get() as usize], 0);
	}

	/// A cylinder group that is already over-full of directories is skipped
	/// even when it satisfies the free-space limits.
	#[test]
	fn ndir_limit_is_enforced() {
		let sb = sb(8);
		// avgndir == 8, so maxndir == 8 + 2^0 == 9 at depth 0 for the root.
		// CG 0 is exactly at the limit; every other CG is one under it.
		let cgs = sums(8, |cg| {
			Csum {
				ndir:   if cg == 0 { 9 } else { 8 },
				nbfree: 100,
				nifree: 200,
				nffree: 0,
			}
		});
		let alloc = AllocationSummary::new(8);
		// The dirpref *point* really is CG 0 here, so a correct implementation
		// has to skip it and take the next cylinder group instead.
		assert_eq!(prefcg_for(&sb, InodeNum::ROOT.get(), 2), 0);
		assert_eq!(
			dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc),
			CgNum::new(1)
		);
	}

	/// The space-deficit backstop is used when the free-space limits exclude
	/// every cylinder group: any CG with at least the average number of free
	/// inodes is acceptable.
	#[test]
	fn space_deficit_backstop() {
		let sb = sb(8);
		// Everything is full of directories and short of both free inodes and
		// free blocks relative to the limits, but CG 6 still meets the plain
		// average.
		let cgs = sums(8, |cg| {
			Csum {
				ndir:   1000,
				nbfree: 100,
				nifree: if cg == 6 { 200 } else { 1 },
				nffree: 0,
			}
		});
		// avgifree here is cstotal.nifree/ncg; force it below CG6's count by
		// lowering the totals.
		let mut sb = sb;
		sb.cstotal.nifree = 8 * 8;
		sb.cstotal.nbfree = 8 * 8;
		sb.cstotal.ndir = 8 * 8;
		let alloc = AllocationSummary::new(8);
		let cg = dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc);
		assert_eq!(cg, CgNum::new(6));
	}

	/// Everything exhausted: the policy still returns a usable CG.
	#[test]
	fn total_deficit_still_returns_a_cg() {
		let sb = sb(4);
		let cgs = sums(4, |_| {
			Csum {
				ndir:   1000,
				nbfree: 0,
				nifree: 0,
				nffree: 0,
			}
		});
		let alloc = AllocationSummary::new(4);
		let cg = dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc);
		assert!(cg.get() < 4);
	}

	/// No parent at all (recovery / fsck-style allocation) yields CG 0.
	#[test]
	fn no_parent_yields_cg0() {
		let sb = sb(4);
		let cgs = uniform(4);
		let alloc = AllocationSummary::new(4);
		let cg = pref_inode(
			&sb,
			InodePrefInput {
				parent: None,
				is_dir: true,
				cgs:    &cgs,
				alloc:  &alloc,
			},
		);
		assert_eq!(cg, CgNum::new(0));
	}

	/// A pathologically deep `i_dirdepth` must not panic.
	#[test]
	fn absurd_depth_does_not_panic() {
		let sb = sb(4);
		let cgs = uniform(4);
		let alloc = AllocationSummary::new(4);
		for depth in [31u32, 32, 63, 64, u32::MAX] {
			let cg = dir_pref(&sb, 2 * 256, depth, 2, &cgs, &alloc);
			assert!(cg.get() < 4, "depth {depth} produced {cg}");
		}
	}

	/// A single-cylinder-group filesystem must still work.
	#[test]
	fn single_cg_filesystem() {
		let sb = sb(1);
		let cgs = uniform(1);
		let alloc = AllocationSummary::new(1);
		assert_eq!(
			dir_pref(&sb, InodeNum::ROOT.get(), 0, 2, &cgs, &alloc),
			CgNum::new(0)
		);
		let p = pref_of(&sb, BlockRole::FileData, 2, 0, 0, &cgs);
		assert_eq!(p.cg, CgNum::new(0));
	}
}
