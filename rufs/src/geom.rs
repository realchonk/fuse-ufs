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

//! UFS2 cylinder-group geometry.
//!
//! # Which structures are on disk?
//!
//! Everything in [`crate::data`] is a literal transcription of a UFS2 on-disk
//! structure and is encoded/decoded with `bincode-next`, so field order *is*
//! layout.  In particular [`Superblock`], [`CylGroup`] and [`Inode`] must not be
//! "improved" without changing the disk format.  Everything in this module is
//! runtime-only bookkeeping derived from those structures and is never
//! serialized.
//!
//! # Cylinder-group geometry
//!
//! A UFS2 filesystem is divided into `ncg` equally sized *cylinder groups*
//! (CGs).  Each CG holds `fpg` fragments of `fsize` bytes, addressed as a
//! single contiguous run of filesystem blocks starting at `cg * fpg`.  Within
//! a CG the layout is fixed by three superblock offsets, in increasing address
//! order:
//!
//! ```text
//!  cg_start(cg)                                              cg_end(cg)
//!      |                                                            |
//!      | super | cyl struct | inode blk | ... | METADATA zone | DATA zone |
//!      |     ^sblkno        ^iblkno            ^dblkno           ^dblkno+metaspace
//! ```
//!
//! * **Inode area** — `cg_inode_start .. cg_inode_end`, exactly
//!   `howmany(ipg, inopb)` blocks.  It comes first because `fs_iblkno` is the
//!   smallest of the three offsets and because inode lookup is the hottest
//!   path: `ino_to_fsba()` must be a shift/add away.
//! * **Metadata zone** — `cg_meta_start .. cg_data_start`.  Directories and
//!   indirect blocks live here.  It exists because those structures are
//!   read together with the CG superblock during `fsck` phase 1 and during
//!   filesystem repair, and because the first `metaspace` blocks of the data
//!   zone are the only place left that is guaranteed contiguous and free when
//!   a large file needs a second-level indirect block.
//! * **Data zone** — `cg_data_start .. cg_end`.  Ordinary file data prefers
//!   here.  It is the largest area, so spreading bulk data over it keeps the
//!   metadata zone available for the metadata that must be adjacent to the CG
//!   superblock.
//!
//! The metadata zone is a *preference*, not a fence: when it is exhausted,
//! allocation spills into ordinary data space.  See [`Superblock::cg_meta_start`]
//! and the fallback ladder in [`crate::policy`].
//!
//! # Relationship to FreeBSD
//!
//! FreeBSD spells these as the macros `cgstart()`, `cgbase()`, `cgimin()`,
//! `cgimax()`, `cgdmin()`, `cgmeta()` and `cgdata()` in `sys/ufs/ffs/fs.h`, and
//! `cgmetabase()` in `sys/ufs/ffs/ffs_subr.c`.  They were used here as a
//! behavioural specification only; the Rust versions below are written from
//! scratch with explicit names and checked by unit tests.

// This module is the documented UFS2 geometry surface: not every accessor is
// used by every build configuration (fragment- and cluster-allocation support
// are still being added), and each one is part of the on-disk-format contract
// that `data.rs` describes.
#![allow(dead_code)]

use crate::data::*;

/// A cylinder-group index.  Kept as a distinct newtype so that a CG number can
/// never be confused with a filesystem block number, which was a very easy
/// mistake to make when reading the `u64`-based FreeBSD code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CgNum(u32);

impl CgNum {
	/// Wrap a raw cylinder-group index.
	///
	/// No validation is performed: out-of-range indices are the caller's
	/// problem, and the geometry helpers below are pure arithmetic.
	pub const fn new(n: u32) -> Self {
		Self(n)
	}

	/// The raw index.
	pub const fn get(self) -> u32 {
		self.0
	}

	/// The index of the *next* cylinder group.  This is not a valid CG number
	/// (it is past the end of the filesystem) but it is the natural way to
	/// write an exclusive upper bound.
	pub const fn next(self) -> Self {
		Self(self.0 + 1)
	}

	/// The index of the *previous* cylinder group.  Only meaningful for
	/// `cg > 0`; wrapping is intentional so that CG0's "previous" is the last
	/// CG, which keeps the modular arithmetic in the allocation policies
	/// branch-free.
	pub const fn prev(self) -> Self {
		Self(self.0.wrapping_sub(1))
	}
}

impl From<u32> for CgNum {
	fn from(n: u32) -> Self {
		Self(n)
	}
}

impl TryFrom<u64> for CgNum {
	type Error = <u32 as TryFrom<u64>>::Error;

	/// Reject cylinder-group indices that do not fit in `u32`.
	///
	/// The infallible `From` silently truncated past `u32::MAX`, remapping an
	/// out-of-range index to a different cylinder group — the exact address
	/// confusion the `CgNum` newtype exists to prevent.  Conversions from
	/// values that could exceed the on-disk `u16`/`u32` CG range must be
	/// fallible.
	fn try_from(n: u64) -> Result<Self, Self::Error> {
		u32::try_from(n).map(Self)
	}
}

impl From<CgNum> for u32 {
	fn from(cg: CgNum) -> Self {
		cg.0
	}
}

impl std::fmt::Display for CgNum {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "CG{}", self.0)
	}
}

impl Superblock {
	/// Iterate over every cylinder-group index, in order.
	pub fn cgs(&self) -> impl Iterator<Item = CgNum> + '_ {
		(0..self.ncg).map(CgNum::new)
	}

	/// Number of fragments in a cylinder group (`fs_fpg`).
	pub fn fpg(&self) -> u64 {
		self.fpg as u64
	}

	/// Number of fragments per filesystem block (`fs_frag`).
	pub fn frag(&self) -> u64 {
		self.frag as u64
	}

	/// Size of a filesystem block, the unit of the block allocator (`fs_bsize`).
	pub fn bsize(&self) -> u64 {
		self.bsize as u64
	}

	/// Size of a fragment, the unit of the block bitmap (`fs_fsize`).
	pub fn fsize(&self) -> u64 {
		self.fsize as u64
	}

	/// Number of inodes per cylinder group (`fs_ipg`).
	pub fn ipg(&self) -> u64 {
		self.ipg as u64
	}

	/// Number of inodes per *block* (`fs_inopb`, a.k.a. `INOPB`).
	pub fn inopb(&self) -> u64 {
		self.inopb as u64
	}

	/// Number of block pointers per indirect block (`fs_nindir`).
	pub fn nindir(&self) -> u64 {
		self.nindir as u64
	}

	/// Size of the per-CG summary area (`fs_cssize`), in bytes.
	pub fn cssize(&self) -> u64 {
		self.cssize as u64
	}

	/// Number of blocks at the start of each cylinder group's data zone that
	/// are reserved for metadata (`fs_metaspace`).
	///
	/// This is where later (second and third level) indirect blocks go.  The
	/// reservation is deliberately located in the data zone rather than in
	/// `cgmeta()`: by the time a file is large enough to need a double-indirect
	/// block, its metadata zone is already mostly consumed, and the blocks must
	/// nevertheless be findable without reading every CG header.
	pub fn metaspace(&self) -> u64 {
		self.metaspace as u64
	}

	/// Number of inode blocks in one cylinder group, `howmany(fs_ipg, INOPB)`.
	pub fn niblk(&self) -> u64 {
		self.ipg().div_ceil(self.inopb())
	}

	/// First filesystem block of cylinder group `cg` (`cgstart`).
	///
	/// UFS2 removed the FFS1 skew, so this is simply `cg * fpg`; UFS1's
	/// `fs_old_cgoffset`/`fs_old_cgmask` remapping is intentionally not
	/// supported.
	pub fn cg_start(&self, cg: CgNum) -> u64 {
		self.fpg() * cg.get() as u64
	}

	/// Alias for [`Self::cg_start`]; the first *addressable* block of the CG.
	pub fn cg_base(&self, cg: CgNum) -> u64 {
		self.cg_start(cg)
	}

	/// One past the last filesystem block of cylinder group `cg` (`cgend`).
	///
	/// Equal to `cg_start(cg.next())`; i.e. cylinder groups tile the whole
	/// filesystem with no gaps.
	pub fn cg_end(&self, cg: CgNum) -> u64 {
		self.cg_start(cg.next())
	}

	/// First block of the inode area of cylinder group `cg` (`cgimin`).
	pub fn cg_inode_start(&self, cg: CgNum) -> u64 {
		self.cg_start(cg) + self.iblkno as u64
	}

	/// One past the last block of the inode area of cylinder group `cg`
	/// (`cgimax`).
	///
	/// `cg_inode_start() + howmany(fs_ipg, INOPB)`; note this is *not* the
	/// start of the metadata zone, because UFS2 leaves a gap between the two.
	pub fn cg_inode_end(&self, cg: CgNum) -> u64 {
		self.cg_inode_start(cg) + self.niblk()
	}

	/// Byte offset of the cylinder-group superblock of `cg` within its block.
	///
	/// UFS2 keeps up to four copies of the primary superblock inside block
	/// `sblkno`; the mirror offset is applied by [`Self::super_mirror`].
	pub fn cg_super(&self, cg: CgNum) -> u64 {
		self.cg_start(cg) + self.sblkno as u64
	}

	/// Byte offset of the cylinder-group summary struct of `cg` within its
	/// block (`cblkno`).
	pub fn cg_struct(&self, cg: CgNum) -> u64 {
		self.cg_start(cg) + self.cblkno as u64
	}

	/// First block of the metadata zone of cylinder group `cg` (`cgmeta`).
	///
	/// This is `cgdmin`, i.e. `fs_dblkno` blocks into the CG.  Everything from
	/// here to [`Self::cg_data_start`] is metadata: directories and indirect
	/// blocks are allocated here first so that `fsck` phase 1 can find them
	/// while walking the CG, and so that they are not interleaved with bulk
	/// file data.
	pub fn cg_meta_start(&self, cg: CgNum) -> u64 {
		self.cg_start(cg) + self.dblkno as u64
	}

	/// First block of the data zone of cylinder group `cg` (`cgdata`).
	///
	/// `cgmeta() + fs_metaspace`.  Bulk file data is allocated from here.
	pub fn cg_data_start(&self, cg: CgNum) -> u64 {
		self.cg_meta_start(cg) + self.metaspace()
	}

	/// Number of metadata-zone blocks in `cg`, i.e. the size of the reserved
	/// metadata area.
	///
	/// Saturated at 0 rather than underflowing if a corrupt superblock claims
	/// `metaspace` larger than the CG itself.
	pub fn cg_meta_len(&self, cg: CgNum) -> u64 {
		self.cg_data_start(cg)
			.saturating_sub(self.cg_meta_start(cg))
	}

	/// Byte offset of the inode-blocks summary area (`csaddr`).
	pub fn cg_sums(&self) -> u64 {
		self.csaddr as u64
	}

	/// Cylinder group that owns a filesystem block (`dtog`).
	pub fn blk_to_cg(&self, blk: u64) -> CgNum {
		CgNum::new((blk / self.fpg()) as u32)
	}

	/// Offset of a filesystem block within its cylinder group (`dtogd`).
	pub fn blk_to_cgoff(&self, blk: u64) -> u64 {
		blk % self.fpg()
	}

	/// Cylinder-group-relative offset of a filesystem block, rounded down to
	/// the enclosing fragment (`blknum`).
	pub fn blk_to_frag(&self, blk: u64) -> u64 {
		blk & !(self.frag() - 1)
	}

	/// Fragment index within the enclosing fragment-group (`fragnum`).
	pub fn blk_to_fragnum(&self, blk: u64) -> u64 {
		blk & (self.frag() - 1)
	}

	/// Index of the first inode number of cylinder group `cg`.
	pub fn cg_inode_base(&self, cg: CgNum) -> u32 {
		cg.get() * self.ipg
	}

	/// Byte offset, within its block, of the on-disk inode `inr`.
	///
	/// This is `ino_to_fsba() * fs_fsize + ino_to_fsbo() * UFS_INOSZ`: the inode
	/// area is a dense array of fixed-size records, so the byte offset is
	/// simply `index * UFS_INOSZ` from the start of the inode area.  The two
	/// halves of the expression agree because `inopb * UFS_INOSZ == fs_bsize`
	/// and `fs_bsize == fs_frag * fs_fsize`.
	pub fn ino_to_fso(&self, inr: InodeNum) -> u64 {
		self.ino_to_fsba(inr) * self.fsize() + self.ino_to_fsbo(inr) * UFS_INOSZ as u64
	}

	/// Filesystem block holding the on-disk inode `inr`.
	///
	/// Note the unit: like FreeBSD's `ino_to_fsba()`, the result is a
	/// **fragment** address, not a `fs_bsize` block address, because
	/// `fs_iblkno` is a fragment offset and one inode block spans
	/// `fs_frag` fragments.  [`Self::ino_to_fso`] is the byte offset that
	/// [`Read`]/[`Write`] want.
	pub fn ino_to_fsba(&self, inr: InodeNum) -> u64 {
		let cg = self.ino_to_cg(inr);
		self.cg_inode_start(cg) + self.blks_to_frags(inr.get64() % self.ipg() / self.inopb())
	}

	/// Index of `inr` within its inode block.
	pub fn ino_to_fsbo(&self, inr: InodeNum) -> u64 {
		inr.get64() % self.inopb()
	}

	/// Cylinder group owning inode `inr` (`ino_to_cg`).
	pub fn ino_to_cg(&self, inr: InodeNum) -> CgNum {
		CgNum::new((inr.get64() / self.ipg()) as u32)
	}

	/// `(cylinder group, index within the group)` of inode `inr`.
	pub fn ino_in_cg(&self, inr: InodeNum) -> (CgNum, u64) {
		let cg = self.ino_to_cg(inr);
		(cg, inr.get64() % self.ipg())
	}

	/// Byte offset of the CylGroup struct of `cg` in filesystem bytes.
	///
	/// Unlike the `*_within_its_block` helpers above, this is an absolute byte
	/// offset into the image, ready to hand to a `Read`/`Write`.
	pub fn cg_addr(&self, cg: CgNum) -> u64 {
		self.cg_struct(cg) * self.fsize()
	}

	/// Byte offset of the primary superblock mirror `n` in the image.
	///
	/// UFS2 stores the superblock twice (plus an optional journal) and only
	/// updates every other write so that a torn superblock still has one valid
	/// copy.
	pub fn super_mirror(n: u32) -> u64 {
		(SBLOCK_UFS2 as u64) + (n as u64 % 2) * SBLOCKSIZE as u64
	}

	/// Round `n` up to a whole number of fragments.
	///
	/// Derived from `fs_fsize` rather than from the shift fields so that a
	/// corrupt shift cannot silently produce a wrong answer.
	///
	/// # Deliberate divergence
	///
	/// FreeBSD's macro of the same name is `(n + fs_qfmask) & fs_fmask` with
	/// `fs_qfmask == ~fs_fmask`, which evaluates to `n % fs_fsize`.  It is an
	/// inclusive-end helper used on already-biased arguments, not a rounding
	/// operation; mirroring it under the same name would be actively
	/// misleading, so this is a true round-up and the FreeBSD form is not
	/// reproduced.
	pub fn frag_roundup(&self, n: u64) -> u64 {
		n.div_ceil(self.fsize()) * self.fsize()
	}

	/// Round `n` up to a whole number of blocks (`fs_bsize`).
	pub fn blk_roundup(&self, n: u64) -> u64 {
		n.div_ceil(self.bsize()) * self.bsize()
	}

	/// Convert a byte count to the number of fragments it spans
	/// (`numfrags`).
	pub fn numfrags(&self, size: u64) -> u64 {
		size >> self.fshift as u32
	}

	/// Convert a fragment count to a byte count (`lfragtosize`).
	pub fn frag_to_size(&self, frags: u64) -> u64 {
		frags << self.fshift as u32
	}

	/// Convert a block count to a byte count (`lblktosize`).
	pub fn blk_to_size(&self, blocks: u64) -> u64 {
		blocks << self.bshift as u32
	}

	/// Convert a byte count to a whole number of blocks (`lblkno`).
	pub fn blkno(&self, off: u64) -> u64 {
		off >> self.bshift as u32
	}

	/// Number of blocks occupied by a fragment range (`fragstoblks`).
	pub fn frags_to_blks(&self, frags: u64) -> u64 {
		frags >> self.fragshift as u32
	}

	/// Number of fragments occupied by a block range (`blkstofrags`).
	pub fn blks_to_frags(&self, blocks: u64) -> u64 {
		blocks << self.fragshift as u32
	}

	/// Fraction of the filesystem that should be kept free, in percent
	/// (`fs_minfree`).
	pub fn minfree_percent(&self) -> u64 {
		self.minfree as u64
	}

	/// Number of fragments that must remain free across the whole filesystem,
	/// given `fs_minfree`.
	pub fn minfree(&self) -> u64 {
		self.blks_to_frags(self.cstotal.nbfree.max(0) as u64)
	}
}

/// Runtime-only allocation bookkeeping.
///
/// # Why this is not part of [`Superblock`]
///
/// `fs_contigdirs` and the allocation rotors are *hints*: they describe where
/// the running kernel would like the next allocation to go, and their value is
/// irrelevant to a filesystem that has been cleanly unmounted (it is rebuilt
/// from the bitmaps on the next mount).  FreeBSD keeps them in the in-core
/// `struct fs` while serializing them out, which is exactly why the on-disk
/// `struct fs` still has kernel pointers in `fs_ocsp`/`fs_si`.  This
/// implementation keeps them strictly in memory so that (a) the on-disk image
/// is byte-identical to what `newfs`/`fsck` would produce, and (b) a
/// read-only mount never has to invent values for fields it cannot write.
///
/// The one field that *is* on disk is the cylinder-group `cg_rotor`,
/// `cg_frotor` and `cg_irotor`, because they are part of `struct cg` and are
/// maintained by `fsck` as well as by the allocator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationSummary {
	/// Number of directories created back-to-back in each cylinder group
	/// without an intervening non-directory allocation.
	///
	/// Saturates at 255.  A *regular file* allocation decrements it, which is
	/// what makes the counter mean "how starved is this CG of file content
	/// relative to its directory count".  See [`crate::policy::DirPref`].
	///
	/// Not persisted: see the type-level docs.
	pub contig_dirs: Vec<u8>,

	/// Cylinder group the last successful block allocation came from.
	///
	/// FreeBSD stores this in `fs_cgrotor`, which *is* a serialized superblock
	/// field; we keep it here and mirror it into the superblock when the
	/// superblock is written, so that a `fs_avgfilesize`-style heuristic
	/// resumes where it left off after a crash.
	pub cg_rotor: u32,
}

impl AllocationSummary {
	/// Build an empty summary for a filesystem with `ncg` cylinder groups.
	pub fn new(ncg: u32) -> Self {
		Self {
			contig_dirs: vec![0; ncg as usize],
			cg_rotor:    0,
		}
	}

	/// Number of cylinder groups this summary covers.
	pub fn len(&self) -> usize {
		self.contig_dirs.len()
	}

	/// Whether this summary covers no cylinder groups at all.
	pub fn is_empty(&self) -> bool {
		self.contig_dirs.is_empty()
	}

	/// Record that an inode was allocated in `cg`.
	///
	/// A directory allocation increments `contig_dirs[cg]` (saturating), any
	/// other inode allocation decrements it.  This is the signal
	/// [`crate::policy::DirPref`] uses to stop a run of `mkdir`s from piling
	/// every new directory into the same cylinder group.
	pub fn note_inode_alloc(&mut self, cg: CgNum, is_dir: bool) {
		let slot = match self.contig_dirs.get_mut(cg.get() as usize) {
			Some(s) => s,
			None => return,
		};
		if is_dir {
			*slot = slot.saturating_add(1);
		} else {
			*slot = slot.saturating_sub(1);
		}
	}
}

/// Synthetic superblocks for unit tests that must not depend on a real image.
#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	/// A small but *realistic* UFS2 geometry, matching the parameters
	/// `newfs -b 32768 -f 4096` produces for a 4 MiB filesystem with a
	/// 4 KiB superblock: 4 CGs, 264 fragments each, 256 inodes each, and a
	/// metadata reservation of 8 blocks at the head of each data zone.
	///
	/// The field values were read out of `resources/ufs-little.img.zst`, so
	/// anything derived from this superblock is validated against a real
	/// `newfs(8)` output rather than against my imagination.
	pub(crate) fn superblock_for_tests() -> Superblock {
		Superblock {
			firstfield:       0,
			unused_1:         0,
			sblkno:           24,
			cblkno:           32,
			iblkno:           40,
			dblkno:           56,
			old_cgoffset:     0,
			old_cgmask:       0,
			old_time:         0,
			old_size:         0,
			old_dsize:        0,
			ncg:              4,
			bsize:            32768,
			fsize:            4096,
			frag:             8,
			minfree:          8,
			old_rotdelay:     0,
			old_rps:          0,
			bmask:            32767,
			fmask:            4095,
			bshift:           15,
			fshift:           12,
			fs_maxcontig:     16,
			fs_maxbpg:        128,
			fragshift:        3,
			fsbtodb:          3,
			sbsize:           4096,
			spare1:           [0; 2],
			nindir:           4096,
			inopb:            128,
			old_nspf:         0,
			optim:            0,
			old_npsect:       0,
			old_interleave:   0,
			old_trackskew:    0,
			id:               [0; 2],
			old_csaddr:       0,
			cssize:           4096,
			cgsize:           4096,
			spare2:           0,
			old_nsect:        0,
			old_spc:          0,
			old_ncyl:         0,
			old_cpg:          0,
			ipg:              256,
			fpg:              264,
			old_cstotal:      Csum {
				ndir:   0,
				nbfree: 0,
				nifree: 0,
				nffree: 0,
			},
			fmod:             0,
			clean:            1,
			ronly:            0,
			old_flags:        0,
			fsmnt:            [0; MAXMNTLEN],
			volname:          [0; MAXVOLLEN],
			swuid:            0,
			pad:              0,
			cgrotor:          0,
			ocsp:             [0; NOCSPTRS],
			si:               0,
			old_cpc:          0,
			maxbsize:         0,
			unrefs:           0,
			providersize:     0,
			metaspace:        8,
			sparecon64:       [0; 13],
			sblockactualloc:  0,
			sblockloc:        0,
			cstotal:          CsumTotal {
				ndir:        0,
				nbfree:      0,
				nifree:      0,
				nffree:      0,
				numclusters: 0,
				spare:       [0; 3],
			},
			time:             0,
			size:             0,
			dsize:            0,
			csaddr:           0,
			pendingblocks:    0,
			pendinginodes:    0,
			snapinum:         [0; FSMAXSNAP],
			avgfilesize:      0,
			avgfpdir:         0,
			save_cgsize:      0,
			mtime:            0,
			sujfree:          0,
			sparecon32:       [0; 21],
			ckhash:           0,
			metackhash:       0,
			flags:            0,
			contigsumsize:    0,
			maxsymlinklen:    0,
			old_inodefmt:     0,
			maxfilesize:      0,
			qbmask:           -32768,
			qfmask:           -4096,
			state:            0,
			old_postblformat: 0,
			old_nrpos:        0,
			spare5:           [0; 2],
			magic:            FS_UFS2_MAGIC,
		}
	}
}

#[cfg(test)]
mod t {
	use super::{tests::superblock_for_tests, *};

	fn sb() -> Superblock {
		superblock_for_tests()
	}

	/// Every cylinder-group geometry assertion, checked for the first, a
	/// middle and the final cylinder group of a 4-CG filesystem.
	///
	/// The point of testing all three is that `cg_start(0) == 0` hides
	/// off-by-one errors in the first group, and that a loop that assumes
	/// `cg_end(cg) < size` silently breaks for the last group.
	#[test]
	fn cg_layout_first_middle_last() {
		let sb = sb();

		// The CG area tiles the filesystem with no gaps and no overlap.
		for cg in sb.cgs() {
			assert_eq!(sb.cg_start(CgNum::new(0)), 0);
			assert_eq!(sb.cg_start(cg), sb.fpg() * cg.get() as u64);
			assert_eq!(sb.cg_end(cg), sb.cg_start(cg.next()));
			if cg.get() > 0 {
				assert_eq!(sb.cg_start(cg), sb.cg_end(cg.prev()));
			}
		}
		assert_eq!(sb.cg_end(CgNum::new(3)), 4 * 264);

		for cg in [CgNum::new(0), CgNum::new(2), CgNum::new(3)] {
			let start = sb.cg_start(cg);

			// Fixed structures come first, in the documented order.
			assert!(sb.cg_super(cg) > start);
			assert!(sb.cg_struct(cg) > sb.cg_super(cg));
			assert!(sb.cg_inode_start(cg) > sb.cg_struct(cg));

			// The inode area is exactly howmany(ipg, inopb) blocks.
			assert_eq!(sb.cg_inode_start(cg), start + 40);
			assert_eq!(sb.cg_inode_end(cg), sb.cg_inode_start(cg) + 2);
			assert_eq!(sb.niblk(), 2);

			// Metadata zone, then the data zone.
			assert_eq!(sb.cg_meta_start(cg), start + 56);
			assert_eq!(sb.cg_data_start(cg), start + 64);
			assert_eq!(sb.cg_meta_len(cg), 8);
			assert_eq!(sb.metaspace(), 8);

			// Everything is inside the CG, and the zones are ordered.
			assert!(sb.cg_inode_end(cg) <= sb.cg_meta_start(cg));
			assert!(sb.cg_meta_start(cg) <= sb.cg_data_start(cg));
			assert!(sb.cg_data_start(cg) <= sb.cg_end(cg));
		}
	}

	/// `blk_to_cg`/`blk_to_cgoff` must be exact inverses of `cg_start`.
	#[test]
	fn blk_cg_roundtrip() {
		let sb = sb();
		for cg in sb.cgs() {
			for off in [0u64, 1, 63, 128, sb.fpg() - 1] {
				let blk = sb.cg_start(cg) + off;
				assert_eq!(sb.blk_to_cg(blk), cg);
				assert_eq!(sb.blk_to_cgoff(blk), off);
			}
		}
		// The first block of CG n belongs to CG n, not CG n-1, for every n.
		for cg in sb.cgs() {
			assert_eq!(sb.blk_to_cg(sb.cg_start(cg)), cg);
		}
	}

	/// Fragment arithmetic: rounding and the block/fragment boundary must
	/// agree with the shifts stored in the superblock.
	#[test]
	fn frag_arithmetic() {
		let sb = sb();
		assert_eq!(sb.frag_roundup(0), 0);
		assert_eq!(sb.frag_roundup(1), sb.fsize());
		assert_eq!(sb.frag_roundup(sb.fsize()), sb.fsize());
		assert_eq!(sb.frag_roundup(sb.fsize() + 1), 2 * sb.fsize());
		assert_eq!(sb.blk_roundup(0), 0);
		assert_eq!(sb.blk_roundup(1), sb.bsize());
		assert_eq!(sb.blk_roundup(sb.bsize()), sb.bsize());
		assert_eq!(sb.blk_roundup(sb.bsize() + 1), 2 * sb.bsize());
		assert_eq!(sb.numfrags(sb.bsize()), sb.frag());
		assert_eq!(sb.frags_to_blks(sb.frag()), 1);
		assert_eq!(sb.blks_to_frags(1), sb.frag());
		assert_eq!(sb.frag_to_size(sb.frag()), sb.bsize());
		assert_eq!(sb.blk_to_size(1), sb.bsize());
		assert_eq!(sb.blkno(sb.bsize()), 1);
		assert_eq!(sb.blkno(sb.bsize() - 1), 0);

		// blk_to_frag rounds down to the enclosing fragment-group.
		assert_eq!(sb.blk_to_frag(0), 0);
		assert_eq!(sb.blk_to_frag(7), 0);
		assert_eq!(sb.blk_to_frag(8), 8);
		assert_eq!(sb.blk_to_fragnum(8), 0);
		assert_eq!(sb.blk_to_fragnum(11), 3);
	}

	/// Inode addresses must be dense within the inode area, and the inode's
	/// cylinder group must match `ino_to_cg`.
	#[test]
	fn ino_addressing() {
		let sb = sb();
		for cg in [CgNum::new(0), CgNum::new(2), CgNum::new(3)] {
			let base = sb.cg_inode_start(cg) * sb.fsize();
			for off in [0u64, 1, 127, 128, 255] {
				let inr = unsafe { InodeNum::new(sb.cg_inode_base(cg) + off as u32) };
				assert_eq!(sb.ino_to_cg(inr), cg);
				assert_eq!(sb.ino_to_fsbo(inr), off % sb.inopb());
				let fso = sb.ino_to_fso(inr);
				assert!(fso >= base, "inode {inr} escaped its inode area");
				assert!(fso < base + sb.niblk() * sb.bsize());
				// The inode area is a *dense* array: no padding between
				// records, only whole-block granularity.
				assert_eq!((fso - base) / UFS_INOSZ as u64, off);
			}
		}

		// ino_to_fso must advance by exactly UFS_INOSZ between neighbours
		// *inside* one inode block...
		let a = unsafe { InodeNum::new(300) };
		let b = unsafe { InodeNum::new(301) };
		assert_eq!(sb.ino_to_fso(b) - sb.ino_to_fso(a), UFS_INOSZ as u64);

		// ...and move to the next inode block at an inopb boundary.  Inodes
		// 127 and 128 are the last and first of two different inode blocks,
		// one bsize apart.
		let c = unsafe { InodeNum::new(127) };
		let d = unsafe { InodeNum::new(128) };
		assert_eq!(
			sb.ino_to_fsba(d) - sb.ino_to_fsba(c),
			sb.blks_to_frags(1),
			"one inode block spans fs_frag fragments"
		);
		assert_eq!(sb.ino_to_fsba(d) - sb.ino_to_fsba(c), sb.frag());
		assert_eq!(sb.ino_to_fsbo(c), sb.inopb() - 1);
		assert_eq!(sb.ino_to_fsbo(d), 0);
		// Inode 127 ends exactly at the end of its inode block.
		assert_eq!(
			sb.ino_to_fso(c) + UFS_INOSZ as u64,
			sb.ino_to_fsba(d) * sb.fsize()
		);

		// inode numbers are dense over the whole CG inode area.
		let first = unsafe { InodeNum::new(256) }; // CG1, inode 0
		let last = unsafe { InodeNum::new(511) }; // CG1, inode 255
		let area_start = sb.cg_inode_start(sb.ino_to_cg(first)) * sb.fsize();
		let area_end = area_start + sb.ipg() * UFS_INOSZ as u64;
		assert_eq!(sb.ino_to_fso(first), area_start);
		assert_eq!(sb.ino_to_fso(last) + UFS_INOSZ as u64, area_end);
	}

	/// `cg_addr` is what the on-disk CG structs are read from; verify it lands
	/// on `cblkno` fragments into the CG and differs per CG.
	#[test]
	fn cg_addr_is_absolute() {
		let sb = sb();
		assert_eq!(sb.cg_addr(CgNum::new(0)), 32 * 4096);
		assert_eq!(sb.cg_addr(CgNum::new(1)), (264 + 32) * 4096);
		assert_ne!(sb.cg_addr(CgNum::new(0)), sb.cg_addr(CgNum::new(1)));
		assert_eq!(sb.cg_sums(), sb.csaddr as u64);
	}

	#[test]
	fn contigdirs_saturates_and_falls_back() {
		let mut a = AllocationSummary::new(2);
		assert_eq!(a.len(), 2);
		assert!(!a.is_empty());

		// Directories increment, saturating at 255.
		for _ in 0..300 {
			a.note_inode_alloc(CgNum::new(0), true);
		}
		assert_eq!(a.contig_dirs[0], 255);
		assert_eq!(a.contig_dirs[1], 0);

		// Regular files decrement, saturating at 0.
		for _ in 0..300 {
			a.note_inode_alloc(CgNum::new(0), false);
		}
		assert_eq!(a.contig_dirs[0], 0);

		// Out-of-range CGs are ignored rather than panicking.
		a.note_inode_alloc(CgNum::new(9), true);
	}
}
