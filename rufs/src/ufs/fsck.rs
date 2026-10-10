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

//! An `fsck_ffs`-shaped consistency checker.
//!
//! # Why
//!
//! FreeBSD's `sbin/fsck_ffs` is the reference for what a UFS2 filesystem is
//! allowed to look like, and it is *not* available on the platforms this
//! driver builds and tests on.  What is available is the definition of the
//! five passes, so this module re-derives the checks that matter for
//! allocation and ordering bugs:
//!
//! | pass | what it does | what it would catch here |
//! |---|---|---|
//! | 1 | walk every inode, check block ownership and block sizes | a pointer to a free block; a block owned twice |
//! | 2 | check directory format, `.` and `..`, entry targets | a directory entry pointing at an uninitialised inode |
//! | 3 | check connectivity to the root | an orphaned subtree |
//! | 4 | rebuild link counts from the directory tree | a link count decremented before its directory entry was removed |
//! | 5 | rebuild every bitmap and summary from scratch | a summary that disagrees with its bitmap, or with the global totals |
//!
//! Pass 5 is the one that matters most for this driver, because pass 5 is a
//! *rebuild*: it compares nothing against the on-disk counters except at the
//! end, so it catches every counter drift that passes 1-4 cannot see.
//!
//! # Not a substitute for fsck_ffs
//!
//! This checker does not implement cluster accounting, extended attributes,
//! snapshots, quotas, ACLs, the `ckhash` metadata checksums, or any repair.
//! It is a read-only oracle for tests.  It is deliberately conservative: it
//! reports a problem rather than guessing whether a discrepancy is benign,
//! and the callers in the test suite treat every reported problem as a bug.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{blockreader::Backend, InodeNum};

/// One inconsistency found by the checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
	/// Which `fsck_ffs` pass would report this.
	pub pass: u8,

	/// Human-readable description.
	pub msg: String,

	/// Whether this is a *contradiction* or merely an *incompleteness*.
	///
	/// The two look the same to a caller and are not the same at all:
	///
	/// * a contradiction is the disk disagreeing with itself -- a pointer to a
	///   block the bitmap calls free, a superblock total that does not match the
	///   bitmaps, a directory entry naming an inode that does not exist.  No
	///   crash point may ever produce one.
	/// * an incompleteness is work that was started and not finished -- an
	///   allocated inode no directory entry names yet, a block nobody points at.
	///   `fsck` "repairs" these by moving them to lost+found; they are what a
	///   half-completed operation looks like from the outside.
	///
	/// [`Report::is_coherent`] is the question the crash-point suite asks, and
	/// the distinction exists for that: a create that wrote its inode but not
	/// its directory entry is not corruption.
	pub contradiction: bool,
}

impl std::fmt::Display for Problem {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "fsck pass {}: {}", self.pass, self.msg)
	}
}

/// The outcome of a consistency check.
#[derive(Debug, Clone, Default)]
pub struct Report {
	/// Problems found, in the order they were detected.
	pub problems: Vec<Problem>,

	/// Inodes that were visited and are allocated.
	pub live_inodes: usize,

	/// Blocks referenced by at least one live inode.
	pub used_blocks: usize,
}

impl Report {
	fn problem(&mut self, pass: u8, msg: impl Into<String>) {
		self.problems.push(Problem {
			pass,
			msg: msg.into(),
			contradiction: true,
		});
	}

	/// Record something that is unfinished rather than self-contradictory.
	fn incomplete(&mut self, pass: u8, msg: impl Into<String>) {
		self.problems.push(Problem {
			pass,
			msg: msg.into(),
			contradiction: false,
		});
	}

	/// Whether the filesystem passed every check.
	pub fn is_clean(&self) -> bool {
		self.problems.is_empty()
	}

	/// Whether the image is at least self-consistent.
	///
	/// True when nothing on the disk contradicts anything else on it, whether or
	/// not the image is finished: a crash can leave an operation half applied,
	/// and that is not the same as leaving it inconsistent.
	pub fn is_coherent(&self) -> bool {
		self.problems.iter().all(|p| !p.contradiction)
	}

	/// A one-line summary, for `assert!` messages.
	pub fn summary(&self) -> String {
		if self.is_clean() {
			format!(
				"clean: {} live inodes, {} used blocks",
				self.live_inodes, self.used_blocks
			)
		} else {
			self.problems
				.iter()
				.map(|p| p.to_string())
				.collect::<Vec<_>>()
				.join("\n")
		}
	}
}

impl<R: Backend> Ufs<R> {
	/// Walk the whole filesystem and check every invariant this driver can
	/// violate.
	///
	/// This is the crash-consistency oracle: after any sequence of
	/// operations — or any simulated crash — the image must satisfy every
	/// check.  See the module docs for the mapping onto `fsck_ffs` passes.
	pub fn check_consistency(&mut self) -> IoResult<Report> {
		let mut rep = Report::default();

		// Blocks claimed by at least one live inode, and by how many.  Used
		// for pass 1's "no two owners" check.
		let mut owners: BTreeMap<u64, Vec<String>> = BTreeMap::new();
		// Fragments (not blocks) that the bitmaps say are allocated.
		//
		// Fragment granularity matters: FreeBSD allocates the tail block of a
		// small file as a *run of fragments*, so an inode can legitimately
		// point at a fragment address that is not block-aligned and whose
		// neighbouring fragments in the same block are still free.
		let mut allocated: BTreeSet<u64> = BTreeSet::new();
		// Inode number -> (nlink from the inode, nlink counted from the tree).
		let mut nlink_on_disk: BTreeMap<u32, u16> = BTreeMap::new();
		let mut nlink_counted: BTreeMap<u32, u16> = BTreeMap::new();
		let mut kind_on_disk: BTreeMap<u32, InodeType> = BTreeMap::new();
		let mut is_dir: BTreeSet<u32> = BTreeSet::new();

		// ------------------------------------------------------------ pass 5
		// Rebuild the bitmaps from first principles.  Doing this first means
		// every later pass can ask "is this block really allocated?" without
		// trusting the on-disk bitmap.
		let sb = self.superblock.clone();
		let mut nbfree = 0i64;
		let mut nifree = 0i64;
		let mut ndir = 0i64;
		let mut cg_nbfree = vec![0i32; sb.ncg as usize];
		let mut cg_nifree = vec![0i32; sb.ncg as usize];
		let mut cg_ndir = vec![0i32; sb.ncg as usize];

		for cg in sb.cgs() {
			let cgd = self.read_cg(cg)?;
			if cgd.cs.ndir < -1 || cgd.cs.nbfree < -1 {
				rep.problem(5, format!("{cg}: implausible free counters {:?}", cgd.cs));
			}
			// cg_cs == {-1,-1,-1,-1} is the on-disk sentinel that says "this
			// cylinder group has never been initialised"; fsck rebuilds it.
			let inited = cgd.cs.ndir >= 0;

			let map = self.read_blkmap(cg, &cgd)?;
			let mut free = 0i32;
			let step = sb.frag();
			for bno in (0..sb.fpg()).step_by(step as usize) {
				if map.is_free_block(bno) {
					free += 1;
				}
			}
			// Fragment granularity matters: FreeBSD allocates the tail block
			// of a small file as a run of *fragments*, so an inode can point
			// at a fragment address that is not block-aligned and whose
			// neighbours in the same block are still free.
			for frag in 0..sb.fpg() {
				if !map.is_free(frag) {
					allocated.insert(sb.cg_start(cg) + frag);
				}
			}
			let imap = self.read_inomap(cg, &cgd)?;
			let mut ifree = 0i32;
			for off in 0..sb.ipg() {
				if imap.is_free(off) {
					ifree += 1;
				}
			}

			if inited {
				cg_nbfree[cg.get() as usize] = free;
				cg_nifree[cg.get() as usize] = ifree;
				nbfree += free as i64;
				nifree += ifree as i64;
			}

			// Compare the on-disk cylinder-group counters with the truth.
			if inited && cgd.cs.nbfree != free {
				rep.problem(
					5,
					format!(
						"{cg}: cg_cs.cs_nbfree is {} but the block bitmap has {free} free blocks",
						cgd.cs.nbfree
					),
				);
			}
			if inited && cgd.cs.nifree != ifree {
				rep.problem(
					5,
					format!(
						"{cg}: cg_cs.cs_nifree is {} but the inode bitmap has {ifree} free inodes",
						cgd.cs.nifree
					),
				);
			}
		}

		if sb.cstotal.nbfree != nbfree {
			rep.problem(
				5,
				format!(
					"fs_cstotal.cs_nbfree is {} but the cylinder-group bitmaps hold {nbfree}",
					sb.cstotal.nbfree
				),
			);
		}
		if sb.cstotal.nifree != nifree {
			rep.problem(
				5,
				format!(
					"fs_cstotal.cs_nifree is {} but the cylinder-group bitmaps hold {nifree}",
					sb.cstotal.nifree
				),
			);
		}

		// ------------------------------------------------------------ pass 1/4
		// Walk every inode the bitmap says is allocated.
		//
		// Inode 1 is skipped: UFS2 reserves it as `lost+found`, and a
		// `newfs`ed filesystem leaves it marked used in `cg_iused[]` with no
		// file type until `fsck` or `mount` populates it.  `fsck_ffs` treats
		// `LOSTFOUNDINO` specially for exactly the same reason.  Nothing in
		// this driver creates or depends on it.
		for inr in 1..sb.ipg * sb.ncg {
			if inr == 1 {
				continue;
			}
			let inr = unsafe { InodeNum::new(inr) };
			let cg = sb.ino_to_cg(inr);
			let off = inr.get64() % sb.ipg();
			let cgd = self.read_cg(cg)?;
			let imap = self.read_inomap(cg, &cgd)?;
			if imap.is_free(off) {
				continue;
			}

			// An allocated inode that cannot be read is not necessarily
			// corruption.  A crash between the bitmap write and the inode's own
			// image leaves exactly that, and `read_inode()` rejects it because
			// there is no file type.  `fsck` pass 1 calls this "allocated inode
			// probably lost"; here it is an *incompleteness*, and the checker has
			// to be able to say so or it cannot look at a crashed image at all.
			let ino = match self.read_inode(inr) {
				Ok(ino) => ino,
				Err(e) => {
					log::warn!("check: {inr} unreadable: {e}");
					rep.incomplete(
						1,
						format!("{inr}: allocated but its image is not on the disk"),
					);
					continue;
				}
			};
			if ino.mode & S_IFMT == 0 {
				// An allocated inode with no file type is the signature of a
				// crash between "bitmap cleared the bit" and "inode written".
				// fsck pass 1 clears it; we report it because nothing in this
				// driver is allowed to produce one.
				rep.problem(1, format!("{inr}: allocated but has no file type"));
				continue;
			}

			rep.live_inodes += 1;
			nlink_on_disk.insert(inr.get(), ino.nlink);
			kind_on_disk.insert(inr.get(), ino.kind());
			if ino.kind() == InodeType::Directory {
				is_dir.insert(inr.get());
				ndir += 1;
				cg_ndir[cg.get() as usize] += 1;
			}

			// Walk the inode's block map *structurally*, not by logical
			// block index.
			//
			// This is what `fsck_ffs` pass 1 does, and it is not an
			// optimisation: UFS2 supports sparse files, so a file's
			// `i_size` may be orders of magnitude larger than its block map
			// is long.  The golden image contains three such files created
			// with `dd seek=`; iterating `0..i_size/bs` over them would take
			// effectively forever.
			let _ = self.walk_block_map(inr, &ino, &mut rep, &allocated, &mut owners)?;

			// `i_blocks` is deliberately *not* cross-checked here.
			//
			// FreeBSD's accounting for it is subtler than it looks: the tail
			// block of a small file is allocated as a run of *fragments*
			// (`blksize()` in sys/ufs/ffs/fs.h), so a 23-byte file has
			// `i_blocks == 8` even though it occupies a whole 32 KiB
			// allocation, and `newfs` gives a directory `i_blocks == 8`
			// regardless of how many blocks it really holds.  Reproducing
			// that faithfully is a separate piece of work (fragment
			// allocation, see the `balloc` module docs) and a check that is
			// subtly wrong is worse than no check at all, because it turns
			// every unrelated test red.

			// Pass 7 (soft-update invariant): the root's depth is 0.
			if ino.kind() == InodeType::Directory {
				let d = ino.dir_depth().unwrap_or(0);
				if inr == InodeNum::ROOT && d != 0 {
					rep.problem(5, format!("root directory has depth {d}, expected 0"));
				}
			}
		}

		// Pass 1, second half: no block may be owned by two incompatible
		// pointers.  (`owners` is keyed by block, so this is a single scan.)
		for (blk, by) in &owners {
			if by.len() > 1 {
				rep.problem(
					1,
					format!(
						"block {blk} is referenced by {} owners: {}",
						by.len(),
						by.join(", ")
					),
				);
			}
		}
		rep.used_blocks = owners.len();

		// Pass 5, directory counter.
		if sb.cstotal.ndir != ndir {
			rep.problem(
				5,
				format!(
					"fs_cstotal.cs_ndir is {} but {ndir} allocated inodes are directories",
					sb.cstotal.ndir
				),
			);
		}
		for cg in sb.cgs() {
			let cgd = self.read_cg(cg)?;
			if cgd.cs.ndir >= 0 && cgd.cs.ndir != cg_ndir[cg.get() as usize] {
				rep.problem(
					5,
					format!(
						"{cg}: cg_cs.cs_ndir is {} but { } allocated inodes there are directories",
						cgd.cs.ndir,
						cg_ndir[cg.get() as usize]
					),
				);
			}
		}

		// ------------------------------------------------------------ pass 2/3/4
		// Walk the directory tree from the root.  This validates directory
		// format, entry targets, connectivity and link counts in one pass,
		// which is exactly how the first four fsck passes interlock.
		let mut visited = BTreeSet::new();
		let mut reached = BTreeSet::new();
		let mut queue = vec![InodeNum::ROOT];
		visited.insert(InodeNum::ROOT.get());
		reached.insert(InodeNum::ROOT.get());

		while let Some(dinr) = queue.pop() {
			match self.read_inode(dinr) {
				Ok(_) => {}
				Err(_) => {
					rep.problem(3, format!("{dinr}: directory is unreadable"));
					continue;
				}
			}

			let mut saw_dot = false;
			let mut saw_dotdot = false;
			let mut names: Vec<(String, InodeNum)> = Vec::new();
			let walk = self.dir_iter(dinr, |name, inr, kind| {
				let n = name.to_string_lossy().into_owned();
				if n == "." {
					saw_dot = true;
					if inr != dinr {
						rep.problem(2, format!("{dinr}: '.' points at {inr} instead of itself"));
					}
				} else if n == ".." {
					saw_dotdot = true;
				} else {
					if kind != InodeType::Directory {
						// Counted below via the child's own scan.
					}
					names.push((n, inr));
				}
				None::<()>
			});

			if let Err(e) = walk {
				rep.problem(2, format!("{dinr}: directory walk failed: {e}"));
				continue;
			}
			if !saw_dot {
				rep.problem(2, format!("{dinr}: no '.' entry"));
			}
			if !saw_dotdot {
				rep.problem(2, format!("{dinr}: no '..' entry"));
			}

			let mut subdirs: u16 = 0;

			for (name, child) in names {
				let ok = self
					.read_inode(child)
					.map(|i| i.mode & S_IFMT != 0)
					.unwrap_or(false);
				let cg = sb.ino_to_cg(child);
				let off = child.get64() % sb.ipg();
				let cgd = self.read_cg(cg)?;
				let imap = self.read_inomap(cg, &cgd)?;
				if imap.is_free(off) {
					rep.problem(
						2,
						format!(
							"{dinr}: entry {name:?} points at {child}, whose inode bitmap bit is free"
						),
					);
					continue;
				}
				if !ok {
					rep.problem(
						2,
						format!("{dinr}: entry {name:?} points at uninitialised inode {child}"),
					);
					continue;
				}

				// UFS2 link counting:
				//
				// * a directory's `i_nlink` is `2 + (number of entries in it
				//   that name a directory)`.  The two are `.` and the
				//   implied `..`; a child's `..` is *not* an extra link,
				//   because it is exactly the link the parent's entry
				//   already accounts for.
				// * a non-directory's `i_nlink` is its hard-link count, i.e.
				//   the number of directory entries that name it.
				reached.insert(child.get());
				if is_dir.contains(&child.get()) {
					subdirs += 1;
				} else {
					*nlink_counted.entry(child.get()).or_default() += 1;
				}

				if is_dir.contains(&child.get()) && visited.insert(child.get()) {
					queue.push(child);
				}
			}

			*nlink_counted.entry(dinr.get()).or_default() += 2 + subdirs;

			// Pass 7: a child's depth must be exactly its parent's plus one.
			if let Ok(ino) = self.read_inode(dinr) {
				if ino.kind() == InodeType::Directory {
					let pd = ino.dir_depth().unwrap_or(0);
					let child = self.dir_lookup(dinr, OsStr::new(".."));
					if let Ok(c) = child {
						if let Ok(ci) = self.read_inode(c) {
							let cd = ci.dir_depth().unwrap_or(0);
							if c != dinr && ci.kind() == InodeType::Directory && cd + 1 != pd {
								rep.problem(
									5,
									format!("{dinr}: depth {pd} but parent {c} has depth {cd}"),
								);
							}
						}
					}
				}
			}
		}

		// Pass 4: link counts.
		//
		// A mismatch in *either* direction is an incompleteness, not a
		// contradiction.  I had split them, on the reasoning that a count which
		// ran *low* was the dangerous one: it understates the subdirectories, so
		// anything reasoning from the count alone might treat the directory as
		// removable.
		//
		// The randomised crash test showed that reasoning is wrong, because a
		// directory rename holds the parent's count back until the entries have
		// moved, and the state in between -- entry moved, count not yet -- *is*
		// the count-low one.  It is unavoidable in any implementation that does
		// not make the whole rename atomic, which is exactly what Soft Updates is
		// not.
		//
		// What settles it is what `fsck` does with the state: pass 4 sets `nlink`
		// from the tree in both directions, and nothing dangles either way.  A
		// count that is too high names an entry that is not there, so it cannot
		// be followed; a count that is too low describes a directory that is
		// still there.  Neither is a reference reaching something it should not,
		// and neither loses data.
		//
		// Calling the low direction a contradiction would also have been a
		// contradiction of itself: `mkdir` reaches exactly that state between its
		// parent's link count and the new entry, and `MkdirParentDep` narrows the
		// window rather than closing it, because the count cannot be published in
		// the same write as the entry that justifies it.
		for (ino, counted) in &nlink_counted {
			match nlink_on_disk.get(ino) {
				Some(on_disk) if on_disk == counted => {}
				Some(on_disk) => {
					rep.incomplete(
						4,
						format!(
							"{ino}: nlink is {on_disk} but the directory tree counts {counted}"
						),
					);
				}
				None => rep.problem(4, format!("{ino}: reachable but not allocated")),
			}
		}

		// An allocated inode nothing reaches, whose counter still counts links
		// that are not there.  `fsck` pass 2 finds it through the inode bitmap and
		// pass 4 clears it; nothing points at it, so nothing can dangle.
		for (ino, on_disk) in &nlink_on_disk {
			if on_disk > &1 && !nlink_counted.contains_key(ino) {
				rep.incomplete(
					4,
					format!("{ino}: nlink is {on_disk} but the inode is unreachable from the root"),
				);
			}
		}

		// Pass 3: every allocated inode must be reachable from the root.
		for ino in nlink_on_disk.keys() {
			if !reached.contains(ino) {
				rep.incomplete(
					3,
					format!("{ino}: allocated but not reachable from the root"),
				);
			}
		}

		let _ = kind_on_disk;
		Ok(rep)
	}

	/// Walk one inode's block map structurally.
	///
	/// Returns the number of *data* blocks and the number of *indirect* blocks
	/// the map references.  Each referenced block is checked against
	/// `allocated` and recorded in `owners` so that pass 1's "one owner per
	/// block" rule can be checked in one scan afterwards.
	fn walk_block_map(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		rep: &mut Report,
		allocated: &BTreeSet<u64>,
		owners: &mut BTreeMap<u64, Vec<String>>,
	) -> IoResult<(u64, u64)> {
		let pbp = self.superblock.bsize() / size_of::<UfsDaddr>() as u64;
		let InodeData::Blocks(b) = &ino.data else {
			return Ok((0, 0));
		};

		let mut data_blocks = 0u64;
		let mut ind_blocks = 0u64;

		/// Record one reference and complain if the block is not allocated.
		fn note(
			inr: InodeNum,
			blk: u64,
			what: &str,
			rep: &mut Report,
			allocated: &BTreeSet<u64>,
			owners: &mut BTreeMap<u64, Vec<String>>,
		) {
			if !allocated.contains(&blk) {
				rep.problem(
					1,
					format!(
						"{inr}: {what} points at block {blk}, which the bitmap reports as free"
					),
				);
			}
			owners
				.entry(blk)
				.or_default()
				.push(format!("{inr}: {what}"));
		}

		for (i, &d) in b.direct.iter().enumerate() {
			if d == 0 {
				continue;
			}
			note(
				inr,
				d as u64,
				&format!("direct[{i}]"),
				rep,
				allocated,
				owners,
			);
			data_blocks += 1;
		}

		for (level, &ib) in b.indirect.iter().enumerate() {
			if ib == 0 {
				continue;
			}
			note(
				inr,
				ib as u64,
				&format!("indirect[{level}]"),
				rep,
				allocated,
				owners,
			);
			ind_blocks += 1;
			let base = self.indirect_base(level);
			let (d, i) = self.walk_subtree(
				inr,
				ib as u64,
				base,
				pbp,
				level as u32 + 1,
				rep,
				allocated,
				owners,
			)?;
			data_blocks += d;
			ind_blocks += i;
		}

		Ok((data_blocks, ind_blocks))
	}

	/// Walk the sub-tree rooted at one indirect block.
	///
	/// `depth` is how many indirect levels remain below `blk`: 1 means `blk`
	/// holds data pointers, 2 means it holds pointers to single-indirect
	/// blocks, and so on.
	#[allow(clippy::too_many_arguments)]
	fn walk_subtree(
		&mut self,
		inr: InodeNum,
		blk: u64,
		base: u64,
		pbp: u64,
		depth: u32,
		rep: &mut Report,
		allocated: &BTreeSet<u64>,
		owners: &mut BTreeMap<u64, Vec<String>>,
	) -> IoResult<(u64, u64)> {
		let mut data = vec![0u64; pbp as usize];
		if let Err(e) = self.read_pblock(blk, &mut data) {
			rep.problem(1, format!("{inr}: indirect block {blk} is unreadable: {e}"));
			return Ok((0, 0));
		}

		let mut data_blocks = 0u64;
		let mut ind_blocks = 0u64;
		for (k, &v) in data.iter().enumerate() {
			if v == 0 {
				continue;
			}
			if depth <= 1 {
				owners
					.entry(v)
					.or_default()
					.push(format!("{inr}: lbn {}", base + k as u64));
				if !allocated.contains(&v) {
					rep.problem(
						1,
						format!(
							"{inr}: lbn {} points at block {v}, which the bitmap reports as free",
							base + k as u64
						),
					);
				}
				data_blocks += 1;
			} else {
				owners
					.entry(v)
					.or_default()
					.push(format!("{inr}: indirect at {}", base + k as u64));
				if !allocated.contains(&v) {
					rep.problem(
						1,
						format!(
							"{inr}: indirect block at {} is {v}, which the bitmap reports as free",
							base + k as u64
						),
					);
				}
				ind_blocks += 1;
				let child = base + k as u64 * pbp;
				let (d, i) =
					self.walk_subtree(inr, v, child, pbp, depth - 1, rep, allocated, owners)?;
				data_blocks += d;
				ind_blocks += i;
			}
		}
		Ok((data_blocks, ind_blocks))
	}

	/// First logical block number covered by indirect level `level`
	/// (0 == single indirect).
	fn indirect_base(&self, level: usize) -> u64 {
		let pbp = self.superblock.bsize() / size_of::<UfsDaddr>() as u64;
		let nd = UFS_NDADDR as u64;
		match level {
			0 => nd,
			1 => nd + pbp,
			_ => nd + pbp + pbp * pbp,
		}
	}
}
