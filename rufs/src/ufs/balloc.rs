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

//! Block allocation: the *implementation* half of the policy split.
//!
//! [`crate::policy`] decides *where*; this module decides *whether it is
//! possible* and performs the bookkeeping.  Nothing here has an opinion about
//! locality: given a [`BlockPref`] it tries that cylinder group, that block,
//! then that cylinder group's rotor, then falls back through the cylinder
//! overflow search, and finally fails with `ENOSPC`.
//!
//! # The cylinder-overflow search
//!
//! FreeBSD's `ffs_hashalloc()` implements a three-step fallback, and this
//! module reproduces it exactly because the *order* matters for
//! near-full filesystems:
//!
//! 1. the preferred cylinder group,
//! 2. a quadratic rehash — `+1, +2, +4, +8, …` cylinder groups, wrapping —
//!    which touches distant parts of the disk before nearby parts,
//! 3. a brute-force sweep of every cylinder group from `preferred + 2`.
//!
//! The quadratic step is not an optimisation.  On a filesystem that is 99%
//! full, the cylinder groups nearest the preference are the ones most likely
//! to be full too, so a linear nearby-first search would rescan the same dead
//! cylinder groups on every single allocation.  Rehashing outwards finds the
//! few surviving cylinder groups after `log2(ncg)` probes instead of `ncg`.
//!
//! # Bitmap representation
//!
//! The UFS2 block bitmap (`cg_blksfree`) has one bit per *fragment*, packed
//! `fs_frag` bits to a byte, and a bit set means *free*.  A whole block is a
//! run of `fs_frag` consecutive fragments, which is always byte-aligned, so
//! whole-block allocation reduces to "find a byte whose `fs_frag` bits are all
//! set".
//!
//! [`BlkMap`] holds that byte array in memory for the duration of one
//! allocation.  The alternative — reading and writing individual map bytes
//! through the decoder inside a scan loop — turns a single allocation into
//! potentially `fpg` seeks.
//!
//! # Divergence from FreeBSD: whole blocks only
//!
//! FreeBSD allocates a run of `fs_frag` fragments when the request cannot be
//! satisfied as a fragment (`ffs_alloccg()`), and tracks the tail as free
//! fragments in `cg_nffree`/`cg_frsum`.  This implementation always allocates
//! and frees *whole blocks* and never touches `cg_nffree` or `cg_frsum`.
//!
//! The reason is not laziness: FreeBSD's fragment path relies on
//! `ffs_fragextend()`, which grows an existing fragment allocation *in place*
//! by clearing more bits of the same block.  Without that, a file whose tail
//! grows from one fragment to two would be given a *second* block by
//! `inode_write_block()` (it reuses the resolved block rather than
//! reallocating), leaking the first.  Whole-block allocation keeps
//! allocate/free exactly symmetric — which is the invariant that actually
//! protects `fsck` phase 5 — at the cost of wasting up to `fs_bsize - 1`
//! bytes in the last block of each file.  Restoring fragment allocation
//! requires in-place extension and is explicitly a follow-up.

use super::*;
use crate::{
	data::*,
	err,
	geom::CgNum,
	policy::{pref_block, BlockPref, BlockPrefInput, BlockRole},
	softdep::DepId,
};

/// Number of bits in a map byte.
const NBBY: u64 = 8;

/// `howmany(n, NBBY)`.
const fn bytes_for(n: u64) -> u64 {
	n.div_ceil(NBBY)
}

/// The on-disk block bitmap of one cylinder group, held in memory.
///
/// Bits are per *fragment*; a set bit means the fragment is free.  Only
/// `fs_fpg` fragments are addressable, so trailing bits in the last byte are
/// masked off by the constructor's `frags` bound in [`BlkMap::find`].
pub struct BlkMap {
	bytes: Vec<u8>,

	/// `fs_frag`: fragments per filesystem block, and map bits per byte.
	frag: u64,

	/// `fs_fpg`: addressable fragments in this cylinder group.
	fpg: u64,
}

// `new`, `frags` and `find_frag_run` are the fragment-granular half of the
// bitmap API.  They are exercised by the unit tests below and are the entry
// points that fragment allocation (see the module docs) will build on.
#[allow(dead_code)]
impl BlkMap {
	/// Build an empty (everything free) map.  Only used by tests; production
	/// maps are read from the cylinder-group superblock.
	pub fn new(frag: u64, fpg: u64) -> Self {
		Self {
			bytes: vec![0xffu8; bytes_for(fpg) as usize],
			frag,
			fpg,
		}
	}

	/// Wrap an existing map image.
	pub fn from_bytes(bytes: Vec<u8>, frag: u64, fpg: u64) -> Self {
		debug_assert_eq!(bytes.len() as u64, bytes_for(fpg));
		Self { bytes, frag, fpg }
	}

	/// The raw map image.
	pub fn as_bytes(&self) -> &[u8] {
		&self.bytes
	}

	/// Number of fragments in the map.
	pub fn frags(&self) -> u64 {
		self.fpg
	}

	/// Whether fragment `bno` is free.
	///
	/// # Bit layout
	///
	/// UFS2 stores *eight* fragment bits per byte regardless of `fs_frag`.
	/// This is `blkmap()`'s
	/// `(map[loc / NBBY] >> (loc % NBBY)) & (0xff >> (NBBY - fs_frag))` in
	/// `sys/ufs/ffs/fs.h`: the mask only looks meaningful for *block-aligned*
	/// locations, which is the only way `blkmap()` is ever called, and it is
	/// what makes the map exactly `howmany(fs_fpg, NBBY)` bytes long.
	///
	/// Packing `fs_frag` bits per byte instead would need a map twice as long
	/// for `fs_frag == 4` and eight times as long for `fs_frag == 1`, i.e. it
	/// would run off the end of the space `newfs` reserved.  (This layout was
	/// previously wrong in this crate; the golden images use `fs_frag == 8`,
	/// which is why the bug was invisible until the packing was exercised
	/// with a smaller fragment size.)
	///
	/// `fs_frag` therefore controls only how many *consecutive* fragments form
	/// a whole block, not how they are packed.
	pub fn is_free(&self, bno: u64) -> bool {
		debug_assert!(bno < self.fpg);
		let byte = (bno / NBBY) as usize;
		let bit = bno % NBBY;
		self.bytes[byte] & (1 << bit) != 0
	}

	/// Set or clear the free bit of fragment `bno`.
	pub fn set_free(&mut self, bno: u64, free: bool) {
		debug_assert!(bno < self.fpg);
		let byte = (bno / NBBY) as usize;
		let bit = bno % NBBY;
		let mask = 1u8 << bit;
		if free {
			self.bytes[byte] |= mask;
		} else {
			self.bytes[byte] &= !mask;
		}
	}

	/// Whether every fragment of the block starting at fragment `bno` is
	/// free.
	pub fn is_free_block(&self, bno: u64) -> bool {
		(0..self.frag).all(|i| self.is_free(bno + i))
	}

	/// Mark the whole block starting at fragment `bno` free or allocated.
	pub fn set_free_block(&mut self, bno: u64, free: bool) {
		for i in 0..self.frag {
			self.set_free(bno + i, free);
		}
	}

	/// Whether any fragment of the block starting at `bno` is in use.
	pub fn is_used_block(&self, bno: u64) -> bool {
		!self.is_free_block(bno)
	}

	/// Find `allocsiz` consecutive free fragments starting at or after
	/// fragment `start`, wrapping once around the cylinder group.
	///
	/// Returns the fragment number of the run, or `None` if there is no such
	/// run.  This is the equivalent of FreeBSD's `ffs_mapsearch()`; the
	/// difference is that FreeBSD searches for the *bit pattern* of a run
	/// inside a single map byte (so it cannot find a run that straddles two
	/// bytes), whereas this walks fragments and can.
	pub fn find(&self, start: u64, allocsiz: u64) -> Option<u64> {
		assert!(allocsiz >= 1 && allocsiz <= self.frag);
		let start = start % self.fpg;
		// Only block-aligned starts can hold a whole block's worth of
		// fragments.
		let step = self.frag;
		let mut bno = start / step * step;

		// Two sweeps: [bno, fpg) then [0, bno).
		for _ in 0..2 {
			while bno + self.frag <= self.fpg {
				if (0..allocsiz).all(|i| self.is_free(bno + i)) {
					return Some(bno);
				}
				bno += step;
			}
			bno = 0;
		}
		None
	}

	/// Find the *first* free fragment at or after `start`, wrapping.  Used for
	/// sub-block fragment runs.
	pub fn find_frag_run(&self, start: u64, allocsiz: u64) -> Option<u64> {
		assert!(allocsiz >= 1 && allocsiz <= self.frag);
		let mut bno = start % self.fpg;
		for _ in 0..2 {
			while bno + allocsiz <= self.fpg {
				if (0..allocsiz).all(|i| self.is_free(bno + i)) {
					return Some(bno);
				}
				bno += 1;
			}
			bno = 0;
		}
		None
	}
}

/// The inode bitmap of one cylinder group, held in memory.
///
/// Same layout as [`BlkMap`] but with one bit per inode, always packed 8 to a
/// byte (`fs_inopb` and the bitmap are independent: UFS2 pads the inode *area*
/// to whole blocks but packs the bitmap eight inodes to a byte).
pub struct InoMap {
	bytes:  Vec<u8>,
	ipg:    u64,
	irotor: u64,
}

impl InoMap {
	/// Wrap an existing map image.
	pub fn from_bytes(bytes: Vec<u8>, ipg: u64, irotor: u64) -> Self {
		debug_assert_eq!(bytes.len() as u64, bytes_for(ipg));
		Self { bytes, ipg, irotor }
	}

	/// The raw map image.
	pub fn as_bytes(&self) -> &[u8] {
		&self.bytes
	}

	/// Whether inode `off` (an offset within this cylinder group) is free.
	pub fn is_free(&self, off: u64) -> bool {
		debug_assert!(off < self.ipg);
		self.bytes[(off / NBBY) as usize] & (1 << (off % NBBY)) == 0
	}

	/// Mark inode `off` used or free.
	pub fn set_used(&mut self, off: u64, used: bool) {
		debug_assert!(off < self.ipg);
		let byte = (off / NBBY) as usize;
		let mask = 1u8 << (off % NBBY);
		if used {
			self.bytes[byte] |= mask;
		} else {
			self.bytes[byte] &= !mask;
		}
	}

	/// Find a free inode, preferring the exact one asked for.
	///
	/// This reproduces FreeBSD's `ffs_nodealloccg()` order: the requested
	/// offset if it happens to be free, then a forward sweep from
	/// `cg_irotor` to the end of the cylinder group, then a wrap back to the
	/// start.  Keeping the rotor roughly monotonic is what stops every
	/// allocation from hitting the same inode and repeatedly invalidating a
	/// single inode-block's page cache entry.
	pub fn find(&mut self, prefer: Option<u64>) -> Option<u64> {
		if let Some(p) = prefer {
			if p < self.ipg && self.is_free(p) {
				self.irotor = p;
				return Some(p);
			}
		}

		let start = self.irotor.min(self.ipg);
		for pass in 0..2 {
			let (lo, hi) = if pass == 0 {
				(start, self.ipg)
			} else {
				(0, start)
			};
			let mut off = lo;
			while off < hi {
				let byte = (off / NBBY) as usize;
				let b = self.bytes[byte];
				if b != 0xff {
					// The first clear bit at or after `off`.  It has to be
					// located inside the byte rather than with
					// `trailing_zeros()`, because the rotor may sit *on* a
					// used inode.
					let base = off - (off % NBBY);
					for bit in (off - base)..NBBY {
						if b & (1 << bit) == 0 {
							let inr = base + bit;
							if inr < self.ipg {
								self.irotor = inr;
								return Some(inr);
							}
							break;
						}
					}
				}
				// Skip the whole byte: it is either full, or its remaining
				// clear bits are all at or beyond fs_ipg.
				off = (off / NBBY + 1) * NBBY;
			}
		}
		None
	}
}

impl<R: Backend> Ufs<R> {
	// ------------------------------------------------------------ CG accessors

	/// Read a cylinder-group superblock.
	///
	/// Goes through the metadata cache, so a cylinder group whose struct is
	/// dirty but not yet persistent reads back as the allocator last wrote it.
	/// Every mutation below reaches the device only via
	/// [`Ufs::sync_metadata`], so this cache-backed read is what keeps the
	/// running filesystem and the disk from disagreeing about `cs_nbfree`.
	pub(super) fn read_cg(&mut self, cg: CgNum) -> IoResult<CylGroup> {
		self.metadata_read(self.superblock.cg_addr(cg))
	}

	/// Stage a cylinder-group superblock and keep the summary cache in sync.
	///
	/// Every mutation of a cylinder group *must* go through here; the cached
	/// [`CgSums`] that the allocation policies read is derived from exactly
	/// these structures, so funnelling the writes is what keeps the cache
	/// honest.
	///
	/// This does **not** persist anything.  It serializes the struct and copies
	/// it into the cached block, which is now dirty; whether that block may
	/// reach the device is [`Ufs::sync_metadata`]'s decision.  That is the
	/// difference between this and the `encode_at` it replaces, and it is what
	/// makes a cylinder-group bitmap update a *step* in a dependency chain
	/// rather than an immediate disk write.
	pub(super) fn write_cg(&mut self, cg: CgNum, cgd: &CylGroup) -> IoResult<()> {
		log::trace!("write_cg({cg}): cs={:?}", cgd.cs);
		self.metadata_write(self.superblock.cg_addr(cg), cgd)?;
		self.cg_sums.set(cg, cgd.cs);
		Ok(())
	}

	/// Read the block bitmap of a cylinder group into memory.
	///
	/// Through the cache, because the bitmap lives in the *same* `fs_bsize`
	/// block as the cylinder-group struct that [`Ufs::write_cg`] stages.  A
	/// read that went straight to the device would miss an unflushed bitmap
	/// change, and — worse — a later write-back of the cached block would
	/// silently roll the bitmap back to whatever it was when the block was
	/// fetched.  Two writers to one block is exactly the situation the
	/// metadata cache exists to eliminate.
	pub(super) fn read_blkmap(&mut self, cg: CgNum, cgd: &CylGroup) -> IoResult<BlkMap> {
		let addr = self.superblock.cg_addr(cg) + cgd.freeoff as u64;
		let map = self.metadata_read_at(addr, bytes_for(self.superblock.fpg()) as usize)?;
		debug_assert_eq!(map.len() as u64, bytes_for(self.superblock.fpg()));
		Ok(BlkMap::from_bytes(
			map,
			self.superblock.frag(),
			self.superblock.fpg(),
		))
	}

	/// Stage the block bitmap of a cylinder group.
	///
	/// The inverse of [`Self::read_blkmap`], and the same cache block: the
	/// bitmap is written back with the cylinder-group struct it belongs to, or
	/// not at all.
	pub(super) fn write_blkmap(&mut self, cg: CgNum, cgd: &CylGroup, map: &BlkMap) -> IoResult<()> {
		let addr = self.superblock.cg_addr(cg) + cgd.freeoff as u64;
		self.metadata_write_at(addr, map.as_bytes())
	}

	/// Read the inode bitmap of a cylinder group into memory.
	///
	/// See [`Self::read_blkmap`] for why this reads through the cache.
	pub(super) fn read_inomap(&mut self, cg: CgNum, cgd: &CylGroup) -> IoResult<InoMap> {
		let addr = self.superblock.cg_addr(cg) + cgd.iusedoff as u64;
		let map = self.metadata_read_at(addr, bytes_for(self.superblock.ipg()) as usize)?;
		debug_assert_eq!(map.len() as u64, bytes_for(self.superblock.ipg()));
		Ok(InoMap::from_bytes(
			map,
			self.superblock.ipg(),
			cgd.irotor as u64,
		))
	}

	/// Stage the inode bitmap of a cylinder group.
	pub(super) fn write_inomap(&mut self, cg: CgNum, cgd: &CylGroup, map: &InoMap) -> IoResult<()> {
		let addr = self.superblock.cg_addr(cg) + cgd.iusedoff as u64;
		self.metadata_write_at(addr, map.as_bytes())
	}

	// ------------------------------------------------------------------ policy

	/// Ask the placement policy where a block for `inr` should go.
	pub(super) fn block_pref(
		&self,
		role: BlockRole,
		inr: InodeNum,
		lbn: u64,
		prev: u64,
		last_direct: u64,
		first_indirect: u64,
	) -> BlockPref {
		pref_block(
			&self.superblock,
			BlockPrefInput {
				role,
				inr,
				lbn,
				prev,
				last_direct,
				first_indirect,
				cgs: &self.cg_sums,
			},
		)
	}

	// ------------------------------------------------------------- allocation

	/// Try to allocate one whole block in `cg`, honouring `pref`.
	///
	/// Reproduces FreeBSD's `ffs_alloccgblk()`:
	///
	/// 1. if the preference names a block in *this* cylinder group, try it;
	/// 2. if the preference names a block in a *different* cylinder group,
	///    translate it into the equivalent position in the same zone of this
	///    cylinder group — this is why a preference is expressed as an
	///    absolute address yet is still meaningful after the cylinder-overflow
	///    search has moved to another cylinder group;
	/// 3. if there was no preference at all, start at `cg_rotor`;
	/// 4. otherwise scan forward from the chosen position, wrapping, and
	///    finally fall back to the map search.
	fn alloc_cg_block(&mut self, cg: CgNum, pref: BlockPref) -> IoResult<Option<NonZeroU64>> {
		let mut cgd = self.read_cg(cg)?;
		if cgd.cs.nbfree <= 0 {
			return Ok(None);
		}

		// Everything derived from the superblock is computed up front so that
		// the immutable borrow ends before the map is read.
		let (start, data_off, fpg, frag) = {
			let sb = &self.superblock;
			let frag = sb.frag();
			let data_off = sb.blk_to_cgoff(sb.cg_data_start(cg));

			// Nothing below the metadata zone is ever handed out.  The region
			// between the start of the cylinder group and `cgmeta()` holds
			// the disk label, the boot blocks, the backup superblock copy,
			// the cylinder-group struct and the inode blocks.
			//
			// `newfs` marks most of that used in the block bitmap, but not
			// all of it: in the *last* cylinder group of a `newfs`-created
			// image the first few boot fragments are left free, and
			// `cg_rotor` starts at zero.  Following FreeBSD's
			// `cgbase + cg_rotor + fs_frag` start point literally would then
			// allocate a boot fragment and destroy the ability to boot the
			// filesystem.  Clamping is strictly safer and costs at most
			// `fs_dblkno` fragments per cylinder group, which the bitmap
			// already counts as free.
			let floor = sb.blk_to_cgoff(sb.cg_meta_start(cg));

			// Where to start looking, as a cylinder-group-relative fragment.
			let start: u64 = if pref.blk == 0 {
				// cg_rotor is a CG-relative fragment offset; step past it so
				// that successive allocations in a full cylinder group keep
				// moving instead of retrying the same block.
				(cgd.rotor as u64 + frag).max(floor)
			} else if sb.blk_to_cg(pref.blk) != cg {
				// Translate a foreign preference into the equivalent zone
				// here: "the metadata zone" and "the data zone" mean the same
				// thing in every cylinder group, which is what makes an
				// absolute-address preference still be meaningful after the
				// cylinder-overflow search has moved on.
				if pref.blk < sb.cg_data_start(sb.blk_to_cg(pref.blk)) {
					sb.blk_to_cgoff(sb.cg_meta_start(cg))
				} else {
					data_off
				}
			} else {
				sb.blk_to_cgoff(pref.blk)
			};

			(sb.blk_to_frag(start), data_off, sb.fpg(), frag)
		};

		let mut map = self.read_blkmap(cg, &cgd)?;

		// (1) The requested block itself.
		if start + frag <= fpg && map.is_free_block(start) {
			map.set_free_block(start, false);
			return self.finish_alloc(cg, &mut cgd, &map, start, None).map(Some);
		}

		// (2) The map search, anchored at the preference so that the rotor
		// keeps sweeping forward rather than restarting each time.
		let Some(bno) = map.find(start, frag) else {
			return Ok(None);
		};

		// cg_rotor is only meaningful for allocations out of the data zone;
		// moving it while servicing the metadata zone would defeat the data
		// zone's locality.
		let rotor = if bno >= data_off {
			Some(bno as u32)
		} else {
			None
		};
		map.set_free_block(bno, false);
		self.finish_alloc(cg, &mut cgd, &map, bno, rotor).map(Some)
	}

	/// Commit an allocation: write the map and the cylinder group, decrement
	/// the free-block counters in both the cylinder group and the superblock.
	fn finish_alloc(
		&mut self,
		cg: CgNum,
		cgd: &mut CylGroup,
		map: &BlkMap,
		bno: u64,
		rotor: Option<u32>,
	) -> IoResult<NonZeroU64> {
		let cg_start = self.superblock.cg_start(cg);
		self.write_blkmap(cg, cgd, map)?;
		if let Some(r) = rotor {
			cgd.rotor = r;
		}
		cgd.cs.nbfree -= 1;
		self.write_cg(cg, cgd)?;
		self.update_sb(|sb| sb.cstotal.nbfree -= 1)?;

		// Block 0 is never allocatable: it would alias the boot blocks, and
		// UFS2 uses 0 as the "no block" sentinel in every on-disk pointer.
		let blkno = NonZeroU64::new(cg_start + bno).expect("allocator returned block 0");
		Ok(blkno)
	}

	/// FreeBSD's `ffs_hashalloc()`: preferred cylinder group, quadratic
	/// rehash, brute force.
	fn hash_alloc_block(&mut self, pref: BlockPref) -> IoResult<Option<NonZeroU64>> {
		let ncg = self.superblock.ncg as u64;
		if ncg == 0 {
			return Ok(None);
		}

		// 1: the preferred cylinder group.
		if let Some(b) = self.alloc_cg_block(pref.cg, pref)? {
			return Ok(Some(b));
		}

		// 2: quadratic rehash.  `i` doubles, so this visits cylinder groups at
		// distances 1, 2, 4, 8, ... from the preference.
		let mut cg = pref.cg.get() as u64;
		let mut i = 1u64;
		while i < ncg {
			cg = (cg + i) % ncg;
			if let Some(b) = self.alloc_cg_block(
				CgNum::new(cg as u32),
				BlockPref::anywhere(CgNum::new(cg as u32)),
			)? {
				return Ok(Some(b));
			}
			i *= 2;
		}

		// 3: brute force, starting after the two cylinder groups already
		// tried (the preferred one and the `+1` from the rehash).
		let icg = pref.cg.get() as u64;
		for k in 2..ncg {
			let c = CgNum::new(((icg + k) % ncg) as u32);
			if let Some(b) = self.alloc_cg_block(c, BlockPref::anywhere(c))? {
				return Ok(Some(b));
			}
		}

		Ok(None)
	}

	/// Allocate one filesystem block for `inr`, following the UFS2 block
	/// placement policy.
	///
	/// The returned block is *reserved* (its bitmap bits are cleared and the
	/// counters decremented) but its contents are undefined.  Callers must
	/// initialise it before any pointer to it may become persistent; see
	/// [`crate::softdep::NewBlockDep`].
	pub(super) fn blk_alloc_for(
		&mut self,
		role: BlockRole,
		inr: InodeNum,
		lbn: u64,
		prev: u64,
		last_direct: u64,
		first_indirect: u64,
	) -> IoResult<NonZeroU64> {
		self.assert_rw()?;
		let pref = self.block_pref(role, inr, lbn, prev, last_direct, first_indirect);
		match self.hash_alloc_block(pref)? {
			Some(b) => {
				// The allocation exists in the in-memory bitmap and in the cached
				// cylinder-group struct, and nowhere else.  Registering the
				// dependency here -- at the one place every allocation goes
				// through -- is what makes "a pointer to this block may not be
				// trusted yet" expressible at all.
				self.register_new_block(b.get());
				Ok(b)
			}
			None => Err(err!(ENOSPC)),
		}
	}

	/// Register a newly allocated block with the dependency engine.
	///
	/// Created by [`Self::blk_alloc_for`], the only path by which a block
	/// becomes allocated.  The dependency says two things about the block:
	/// its allocation bitmap must reach the disk, and its initialised contents
	/// must reach the disk, before any pointer to it may become persistent.
	///
	/// Note what is *not* claimed here: that either of them has happened.  The
	/// bitmap bit has been set in memory and the block has not been written;
	/// the two facts are recorded when the corresponding bytes actually reach
	/// the device, and until then nothing may point at this block.
	///
	/// Protects invariants 2, 3 and 4 in `docs/ufs2-invariants.md`.
	pub(super) fn register_new_block(&mut self, blk: u64) -> DepId {
		let cg = self.superblock.blk_to_cg(blk);
		let id = self.softdep.new_block(blk, cg);
		log::trace!("register_new_block({blk}) in {cg}: {id:?}");
		id
	}

	/// The allocation dependency for `blk`, if it is still outstanding.
	pub(super) fn allocation_of(&self, blk: u64) -> Option<DepId> {
		self.softdep.dep_for_block(blk)
	}

	/// Record that the initialised contents of `blk` have reached the device.
	///
	/// Distinct from having *changed* the block.  A live image that has been
	/// modified is still a lie as far as the disk is concerned, and treating
	/// the two as the same is precisely the bug that makes an uninitialised
	/// block reachable through a persistent pointer.
	///
	/// A block with no outstanding dependency is silently not noted: it was
	/// already allocated before this operation, so its contents being on the
	/// disk says nothing about anything pending.
	pub(super) fn note_block_contents(&mut self, blk: u64) -> IoResult<()> {
		if let Some(id) = self.allocation_of(blk) {
			log::trace!("note_block_contents({blk}): {id:?}");
			self.softdep.note_contents_written(id)?;
		}
		Ok(())
	}

	/// Register a newly allocated inode with the dependency engine.
	///
	/// Created by [`Self::hash_alloc_inode`], the one path by which an inode
	/// becomes allocated.  The dependency says the inode's *bitmap bit* and its
	/// *image* must both reach the disk before anything may name it; see
	/// [`crate::softdep::InodeDep`] for the crash that prevents.
	pub(super) fn register_new_inode(&mut self, inr: InodeNum) -> DepId {
		let cg = self.superblock.ino_to_cg(inr);
		let id = self.softdep.new_inode(inr, cg);
		log::trace!("register_new_inode({inr}) in {cg}: {id:?}");
		id
	}

	/// The allocation dependency for `inr`, if it is still outstanding.
	///
	/// Test-only, like the other `Ufs` internals: the engine looks this up by
	/// inode itself, and a caller that could ask would be able to conclude
	/// things about ordering that it has no business concluding.
	#[cfg(test)]
	pub(super) fn inode_allocation_of(&self, inr: InodeNum) -> Option<DepId> {
		self.softdep
			.new_inodes()
			.find(|d| d.inr() == inr)
			.map(|d| d.id())
	}

	/// Allocate one filesystem block and zero it.
	///
	/// Zeroing matters for two different reasons.  For a freshly allocated
	/// *file data* block it saves a read.  For a freshly allocated *indirect*
	/// block it is mandatory: the rest of the block must read as "no pointer
	/// here", and while `0` is the correct on-disk value for a hole, a block
	/// that still holds stale data from its previous life would be
	/// indistinguishable from a real pointer after a crash.
	pub(super) fn blk_alloc_zeroed_for(
		&mut self,
		role: BlockRole,
		inr: InodeNum,
		lbn: u64,
		prev: u64,
		last_direct: u64,
		first_indirect: u64,
	) -> IoResult<NonZeroU64> {
		let blkno = self.blk_alloc_for(role, inr, lbn, prev, last_direct, first_indirect)?;
		// A whole `fs_bsize`, not one `fs_fsize`.  An indirect block holds
		// `fs_bsize / 8` pointers, so zeroing only the first fragment would
		// leave the rest of the freshly allocated block holding whatever was
		// there before, and `inode_free_l1()` would later walk those stale
		// "pointers" and free blocks that belong to somebody else.
		let bs = self.superblock.bsize() as usize;
		//
		// Through the metadata cache, because this block is about to become an
		// indirect block.  Filling it directly would be wrong twice over: it
		// would bypass the cache for a block the cache will own from here on,
		// and if the block were still resident from a previous life, the next
		// write-back would restore the old pointers and re-arm references to
		// blocks that have since been freed.
		self.metadata_fill_at(blkno.get() * self.superblock.fsize(), 0u8, bs)?;
		Ok(blkno)
	}

	/// Free a whole filesystem block.
	///
	/// `size` must be a whole number of blocks; see the module-level note on
	/// why fragment-granular freeing is deliberately not implemented.
	///
	/// # `container`, and why the free is not immediate
	///
	/// `container` is the cache block of the structure the pointer to this block
	/// has just been removed from -- the inode for a direct block or a
	/// first-level indirect block, the enclosing indirect block for anything
	/// deeper.  Its *identity* is not what matters; whether its current contents
	/// have reached the disk is.
	///
	/// Clearing the allocation bit before that would hand a block that the disk
	/// still has a pointer to to the next allocation, and the old file would go
	/// on reading somebody else's data.  So the whole cylinder-group block --
	/// struct, counters and both bitmaps -- is gated until the container has been
	/// written.  The whole block rather than the one bit, because the bit and
	/// `cs_nbfree` have to move together: a crash between them would leave the
	/// bitmap saying free and the counter saying used.
	///
	/// A container that is already clean needs no gate: the pointer was removed
	/// by something that did not dirty it, so the disk has not changed and there
	/// is nothing to wait for.
	///
	/// Protects invariants 1, 2 and 3 in `docs/ufs2-invariants.md`.
	/// `container` is threaded but not acted on; see the note below.
	pub(super) fn blk_free(&mut self, bno: u64, size: u64, container: u64) -> IoResult<()> {
		log::trace!("blk_free(bno={bno}, size={size}, container={container});");
		self.assert_rw()?;

		if bno == 0 {
			return Ok(());
		}
		let (cg, off) = self.free_target(bno, size)?;

		// Validate before queueing, so a double free is caught whether it
		// arrives now or after the first free has run.  Both checks are needed
		// and they are not redundant: the bitmap catches a genuinely free block,
		// and the queue catches a block whose free is already waiting -- which
		// the bitmap cannot see, because a pending free deliberately leaves it
		// reading as allocated.
		let cgd = self.read_cg(cg)?;
		let map = self.read_blkmap(cg, &cgd)?;
		if !map.is_used_block(off) {
			log::error!(
				"blk_free({bno}): freeing a free block in {cg} at fragment {off}; \
				 filesystem is corrupt"
			);
			return Err(err!(EINVAL));
		}
		if self
			.deferred
			.contains(&crate::softdep::OpKey::FreeBlock(bno))
		{
			log::error!(
				"blk_free({bno}): a free of this block is already pending; the \
				 filesystem is corrupt"
			);
			return Err(err!(EINVAL));
		}

		// If the structure that used to point here is already on the disk, the
		// pointer cannot survive a crash and the free can happen now.  Otherwise
		// it has to wait: clearing the bit now would make the block reusable
		// while a pointer to it is still persistent.
		if self.softdep.container_is_persisted(container) {
			return self.blk_free_now(bno, size, cg);
		}

		log::trace!("blk_free({bno}): deferred until {container} is persisted");
		let fresh = self.deferred.push(crate::softdep::DeferredOp::FreeBlock {
			blk: bno,
			size,
			container,
		});
		debug_assert!(fresh, "the duplicate check above should have caught this");
		Ok(())
	}

	/// Check a free's shape and say which cylinder group and offset it lands on.
	fn free_target(&self, bno: u64, size: u64) -> IoResult<(CgNum, u64)> {
		let sb = &self.superblock;
		let bsize = sb.bsize();
		assert_ne!(size, 0);
		assert_eq!(
			size % sb.fsize(),
			0,
			"blk_free: size must be a multiple of fs_fsize"
		);
		assert!(
			size == bsize,
			"blk_free: only whole-block frees are supported (size={size}, bsize={bsize})"
		);
		Ok((sb.blk_to_cg(bno), sb.blk_to_frag(sb.blk_to_cgoff(bno))))
	}

	/// Release a block's allocation bit: the bit, `cs_nbfree` and
	/// `fs_cstotal.cs_nbfree`, together.
	///
	/// All three move in one place because a crash between them is exactly what
	/// `check_consistency()` reports as "cg_cs.cs_nbfree is N but the
	/// cylinder-group bitmaps hold M".  Calling this twice for one block is what
	/// `blk_free` refuses to arrange, and `blk_free_now` checks again rather
	/// than trusting the caller.
	pub(super) fn blk_free_now(&mut self, bno: u64, size: u64, cg: CgNum) -> IoResult<()> {
		let off = {
			let sb = &self.superblock;
			sb.blk_to_frag(sb.blk_to_cgoff(bno))
		};
		let _ = size;
		let mut cgd = self.read_cg(cg)?;
		let mut map = self.read_blkmap(cg, &cgd)?;

		if !map.is_used_block(off) {
			log::error!(
				"blk_free_now({bno}): block is not allocated in {cg} at fragment {off}; \
				 refusing to decrement the counters twice"
			);
			return Err(err!(EINVAL));
		}

		map.set_free_block(off, true);
		self.write_blkmap(cg, &cgd, &map)?;
		cgd.cs.nbfree += 1;
		self.write_cg(cg, &cgd)?;
		self.update_sb(|sb| sb.cstotal.nbfree += 1)?;
		log::trace!("blk_free_now({bno}): applied");
		Ok(())
	}

	/// Inode bitmap helper: allocate an inode in `cg`.
	///
	/// `prefer` is the offset within the cylinder group that the policy asked
	/// for, if any.
	pub(super) fn alloc_cg_inode(
		&mut self,
		cg: CgNum,
		prefer: Option<u64>,
	) -> IoResult<Option<u64>> {
		let mut cgd = self.read_cg(cg)?;
		if cgd.cs.nifree <= 0 {
			return Ok(None);
		}
		let ipg = self.superblock.ipg();
		let mut map = self.read_inomap(cg, &cgd)?;
		let Some(off) = map.find(prefer) else {
			return Ok(None);
		};
		map.set_used(off, true);
		self.write_inomap(cg, &cgd, &map)?;
		cgd.irotor = off as u32;
		cgd.cs.nifree -= 1;
		if (cgd.cs.ndir, cgd.cs.nbfree, cgd.cs.nifree, cgd.cs.nffree) == (-1, -1, -1, -1) {
			log::warn!("{cg}: csum looks like the fs_metaspace sentinel");
		}
		self.write_cg(cg, &cgd)?;
		self.update_sb(|sb| sb.cstotal.nifree -= 1)?;
		let _ = ipg;
		Ok(Some(off))
	}

	/// FreeBSD's `ffs_hashalloc()` for inodes.
	pub(super) fn hash_alloc_inode(
		&mut self,
		pref: CgNum,
		prefer_off: Option<u64>,
	) -> IoResult<Option<InodeNum>> {
		let ncg = self.superblock.ncg as u64;
		if ncg == 0 {
			return Ok(None);
		}

		if let Some(off) = self.alloc_cg_inode(pref, prefer_off)? {
			let inr = unsafe { InodeNum::new(self.superblock.cg_inode_base(pref) + off as u32) };
			self.register_new_inode(inr);
			return Ok(Some(inr));
		}

		let mut cg = pref.get() as u64;
		let mut i = 1u64;
		while i < ncg {
			cg = (cg + i) % ncg;
			let c = CgNum::new(cg as u32);
			if let Some(off) = self.alloc_cg_inode(c, None)? {
				let inr = unsafe { InodeNum::new(self.superblock.cg_inode_base(c) + off as u32) };
				self.register_new_inode(inr);
				return Ok(Some(inr));
			}
			i *= 2;
		}

		let icg = pref.get() as u64;
		for k in 2..ncg {
			let c = CgNum::new(((icg + k) % ncg) as u32);
			if let Some(off) = self.alloc_cg_inode(c, None)? {
				let inr = unsafe { InodeNum::new(self.superblock.cg_inode_base(c) + off as u32) };
				self.register_new_inode(inr);
				return Ok(Some(inr));
			}
		}

		Ok(None)
	}

	/// Release an inode: clear the inode bitmap bit and adjust the counters.
	///
	/// Ordering note: the bitmap bit is cleared *last*.  `fsck` phase 5
	/// rebuilds the bitmap from the inodes that are still referenced; a
	/// bitmap that says "free" while the inode is still reachable is repaired,
	/// whereas a bitmap that says "used" for an inode with `nlink == 0` is
	/// repaired too but loses the inode's contents first.  See
	/// [`crate::softdep::FreeInodeDep`].
	/// Release an inode, once nothing persistent can still name it.
	///
	/// The inode is already zeroed in the buffer by the time this is called --
	/// `inode_free()` clears it before it gets here -- but the inode *bitmap bit*
	/// is what says the inode number is in use, and it is what stops the number
	/// being handed out again.  Releasing it before the zeroed image is on the
	/// disk would let a crash leave a directory entry pointing at an inode whose
	/// fields are zero and whose bit is clear: `fsck` pass 2 resolves that by
	/// clearing the entry, discarding a file whose data was never lost.
	///
	/// If the inode's block is already on the disk the release happens now.  If
	/// it is not, it is queued, because the zeroed image is itself held back
	/// until the directory entry is gone and that may not have happened yet.
	pub(super) fn free_cg_inode(&mut self, inr: InodeNum, gen: u32, was_dir: bool) -> IoResult<()> {
		self.assert_rw()?;
		let (cg, off) = self.superblock.ino_in_cg(inr);

		// Validate before queueing, as `blk_free` does: the bitmap catches a
		// genuinely free inode, and the queue catches one whose release is
		// already waiting -- which the bitmap cannot see, for the same reason.
		let cgd = self.read_cg(cg)?;
		if self.read_inomap(cg, &cgd)?.is_free(off) {
			log::error!("free_cg_inode({inr}): double free in {cg} at offset {off}");
			return Err(err!(EINVAL));
		}
		if self
			.deferred
			.contains(&crate::softdep::OpKey::FreeInode(inr, gen))
		{
			log::error!("free_cg_inode({inr}): this inode's release is already pending");
			return Err(err!(EINVAL));
		}

		let container = self.metadata_blk(self.superblock.ino_to_fsba(inr));
		if self.softdep.container_is_persisted(container) {
			return self.free_cg_inode_now(inr, gen, was_dir);
		}

		log::trace!("free_cg_inode({inr}): deferred until {container} is persistent");
		let fresh = self.deferred.push(crate::softdep::DeferredOp::FreeInode {
			inr,
			gen,
			was_dir,
			container,
		});
		debug_assert!(fresh, "the duplicate check above should have caught this");
		Ok(())
	}

	/// The whole of an inode's release: the bitmap bit, `cs_nifree`,
	/// `fs_cstotal.cs_nifree`, and for a directory `cs_ndir` and
	/// `fs_cstotal.cs_ndir`.
	///
	/// They move together because they describe one set of live inodes, and
	/// `check_consistency()` compares each against the bitmap.  `gen` is checked
	/// before anything is written: the inode is zero by now, so without it a
	/// release queued for one inode could be applied to whatever later took the
	/// same number.
	pub(super) fn free_cg_inode_now(
		&mut self,
		inr: InodeNum,
		gen: u32,
		was_dir: bool,
	) -> IoResult<()> {
		let (cg, off) = self.superblock.ino_in_cg(inr);

		// The inode is zero by now, so `di_gen` cannot be read back.  If it is
		// readable *and* carries a different generation, the number has been
		// reallocated since the release was queued, and releasing the bit now
		// would free somebody else's inode.
		//
		// A zero generation means the inode was cleared, which is the ordinary
		// case and says nothing.
		if let Ok(ino) = self.read_inode(inr) {
			if ino.gen != 0 && ino.gen != gen {
				log::error!(
					"free_cg_inode_now({inr}): generation is {} but the queued \
					 release was for generation {gen}; the inode number has been \
					 reallocated and must not be freed",
					ino.gen
				);
				return Err(err!(EINVAL));
			}
		}

		let mut cgd = self.read_cg(cg)?;
		let mut map = self.read_inomap(cg, &cgd)?;

		if map.is_free(off) {
			log::error!(
				"free_cg_inode_now({inr}): already free in {cg} at offset {off}; \
				 refusing to decrement the counters twice"
			);
			return Err(err!(EINVAL));
		}

		map.set_used(off, false);
		self.write_inomap(cg, &cgd, &map)?;
		cgd.cs.nifree += 1;
		if was_dir {
			cgd.cs.ndir -= 1;
		}
		self.write_cg(cg, &cgd)?;
		self.update_sb(|sb| {
			sb.cstotal.nifree += 1;
			if was_dir {
				sb.cstotal.ndir -= 1;
			}
		})?;
		log::trace!("free_cg_inode_now({inr}): applied");
		Ok(())
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::geom::tests::superblock_for_tests;

	#[allow(dead_code)]
	fn sb() -> Superblock {
		superblock_for_tests()
	}

	// ------------------------------------------------------------ BlkMap

	/// A fresh map has every fragment free and every block free.
	#[test]
	fn map_starts_all_free() {
		let mut m = BlkMap::new(8, 64);
		assert_eq!(m.frags(), 64);
		assert!(m.is_free(0));
		assert!(m.is_free(63));
		assert!(m.is_free_block(0));
		assert!(m.is_free_block(56));
		// Offsets within a block share the block's state.
		m.set_free(3, false);
		assert!(!m.is_free(3));
		assert!(!m.is_free_block(0));
		assert!(m.is_used_block(0));
		assert!(m.is_free_block(8));
	}

	/// Setting a whole block must not disturb its neighbours.
	#[test]
	fn block_bits_are_independent() {
		let mut m = BlkMap::new(8, 64);
		m.set_free_block(8, false);
		assert!(!m.is_free_block(8));
		assert!(m.is_free_block(0));
		assert!(m.is_free_block(16));
		for i in 0..8 {
			assert!(!m.is_free(8 + i));
			assert!(m.is_free(i));
			assert!(m.is_free(16 + i));
		}
	}

	/// The map is eight fragments per byte even when `fs_frag < 8`, because
	/// that is the size `newfs` reserved (`howmany(fs_fpg, NBBY)`).
	#[test]
	fn map_packs_eight_fragments_per_byte() {
		let fpg = 63u64;
		let m = BlkMap::new(4, fpg);
		// ceil(63/8) = 8 bytes, not ceil(63/4) = 16.
		assert_eq!(m.as_bytes().len() as u64, bytes_for(fpg));
		assert_eq!(m.as_bytes().len(), 8);

		// Every fragment number below fpg is a real bit, and a fresh map has
		// them all free.
		let mut m = BlkMap::new(4, fpg);
		for b in 0..fpg {
			assert!(m.is_free(b), "fragment {b}");
		}
		// A whole block is `fs_frag` consecutive fragments starting at a
		// multiple of `fs_frag`; with fs_frag == 4 that is two blocks per
		// map byte.
		m.set_free_block(4, false);
		assert!(!m.is_free_block(4));
		assert!(m.is_free_block(0));
		assert!(m.is_free_block(8));
		// With fs_frag == 4 the block *run* advances by four fragments, not
		// by eight, so the search hops over block 4 in one step.
		assert_eq!(m.find(0, 4), Some(0));
		assert_eq!(m.find(4, 4), Some(8));
	}

	/// The whole-block search must return block-aligned runs only, scan
	/// forward from the preference, and wrap.
	#[test]
	fn map_search_is_aligned_and_wraps() {
		let mut m = BlkMap::new(8, 64);
		m.set_free_block(0, false);
		assert_eq!(m.find(0, 8), Some(8));

		// Freeing from 32 onwards: a search starting at 40 wraps to 8.
		for b in [32usize, 40, 48, 56] {
			m.set_free_block(b as u64, false);
		}
		assert_eq!(m.find(40, 8), Some(8));

		// A search starting past the last candidate but below fpg still
		// wraps rather than returning None.
		assert_eq!(m.find(60, 8), Some(8));
	}

	/// A full map has nothing to find; an empty map finds block 0.
	#[test]
	fn map_search_extremes() {
		let mut m = BlkMap::new(8, 64);
		assert_eq!(m.find(0, 8), Some(0));
		for b in 0..8u64 {
			m.set_free_block(b * 8, false);
		}
		assert_eq!(m.find(0, 8), None);
		assert_eq!(m.find(56, 8), None);
	}

	/// The search must respect the preference when that block is free.
	#[test]
	fn map_search_prefers_the_anchor() {
		let mut m = BlkMap::new(8, 64);
		assert_eq!(m.find(5 * 8, 8), Some(5 * 8));
		m.set_free_block(5 * 8, false);
		assert_eq!(m.find(5 * 8, 8), Some(6 * 8));
	}

	/// A fragment run may start anywhere, including in the middle of a map
	/// byte.
	///
	/// This is the deliberate improvement over FreeBSD's `ffs_mapsearch()`,
	/// which only inspects bit patterns inside a single map byte and therefore
	/// cannot find a run that straddles two bytes.
	#[test]
	fn frag_run_may_start_mid_byte() {
		let mut m = BlkMap::new(8, 64);
		// Occupy 0..6 so that only 6 and 7 of byte 0 are free.
		for f in 0..6 {
			m.set_free(f, false);
		}
		assert_eq!(m.find_frag_run(6, 2), Some(6));
		assert_eq!(m.find_frag_run(7, 2), Some(7));
		// ...but the whole-block search skips it: a block needs a run of
		// `fs_frag` fragments starting on a block boundary.
		assert_eq!(m.find(0, 8), Some(8));

		// Taking 7 away too leaves no run in byte 0 at all, so the search
		// continues into the next byte.
		m.set_free(7, false);
		assert_eq!(m.find_frag_run(6, 2), Some(8));
	}

	/// `find_frag_run` still refuses to start a run that would cross the end
	/// of the map, and wraps instead.
	#[test]
	fn frag_run_does_not_overrun() {
		let m = BlkMap::new(8, 64);
		assert_eq!(m.find_frag_run(63, 2), Some(0));
		// fpg is not a multiple of fs_frag, so the tail of the map is short.
		// With only fragments 60..62 free, a run of three fits but a run of
		// four does not, and neither may start past the end.
		let mut m = BlkMap::new(4, 63);
		for f in 0..60 {
			m.set_free(f, false);
		}
		assert_eq!(m.find_frag_run(0, 3), Some(60));
		assert_eq!(m.find_frag_run(0, 4), None);
		// A run that would start at the last real fragment and run off the
		// end of the map is not offered; the search wraps instead.
		assert_eq!(m.find_frag_run(62, 2), Some(60));
	}

	/// Round-tripping a map through its byte image must be lossless.
	#[test]
	fn map_roundtrip_through_bytes() {
		let mut m = BlkMap::new(8, 128);
		m.set_free_block(0, false);
		m.set_free_block(64, false);
		m.set_free(70, false);
		let m2 = BlkMap::from_bytes(m.as_bytes().to_vec(), 8, 128);
		assert!(!m2.is_free_block(0));
		assert!(!m2.is_free_block(64));
		assert!(!m2.is_free(70));
		assert!(m2.is_free_block(8));
	}

	// ------------------------------------------------------------ InoMap

	/// A fresh inode map hands out the requested inode, then the rotor.
	///
	/// `find` only *searches*; marking an inode used is the caller's job,
	/// exactly as in `alloc_cg_inode`.
	#[test]
	fn inomap_prefers_request_then_rotor() {
		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(256) as usize], 256, 0);

		assert_eq!(m.find(Some(7)), Some(7));
		m.set_used(7, true);
		// 7 is taken; asking for it again falls through to the sweep from the
		// rotor, which sits at 7.
		assert_eq!(m.find(Some(7)), Some(8));

		// Asking for an out-of-range inode is ignored, not honoured.
		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(256) as usize], 256, 0);
		assert_eq!(m.find(Some(9999)), Some(0));
	}

	/// The inode search returns nothing when every inode is used.
	#[test]
	fn inomap_full() {
		let mut m = InoMap::from_bytes(vec![0xffu8; bytes_for(16) as usize], 16, 0);
		assert_eq!(m.find(None), None);
	}

	/// The inode search wraps past the end of the cylinder group.
	#[test]
	fn inomap_wraps() {
		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(16) as usize], 16, 15);
		m.set_used(15, true);
		// The rotor sits at 15, which is used, so the sweep wraps to the start.
		assert_eq!(m.find(None), Some(0));
	}

	/// The search must never return an inode at or beyond `ipg`, even when
	/// the last map byte has spare bits.
	#[test]
	fn inomap_respects_ipg() {
		let ipg = 12u64;
		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(ipg) as usize], ipg, 0);
		for off in 0..ipg {
			m.set_used(off, true);
		}
		assert_eq!(m.find(None), None);

		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(ipg) as usize], ipg, 0);
		for off in 0..ipg - 1 {
			m.set_used(off, true);
		}
		assert_eq!(m.find(None), Some(ipg - 1));
		// Mark it used too; the search must notice the trailing spare bits are
		// not real inodes.
		m.set_used(ipg - 1, true);
		assert_eq!(m.find(None), None);
	}

	/// set_used/is_free must round-trip for every bit position.
	#[test]
	fn inomap_bits_roundtrip() {
		let mut m = InoMap::from_bytes(vec![0u8; bytes_for(32) as usize], 32, 0);
		for off in 0..32u64 {
			assert!(m.is_free(off));
			m.set_used(off, true);
			assert!(!m.is_free(off), "bit {off}");
			m.set_used(off, false);
			assert!(m.is_free(off), "bit {off}");
		}
	}

	// ------------------------------------------------------- hash_alloc order

	/// The cylinder-overflow search must consult cylinder groups in the order
	/// preferred, +1, +2, +4, ..., then brute force.
	///
	/// This is a pure-model test of the ordering, which is the part that is
	/// easy to get wrong and impossible to see in an end-to-end test on a
	/// nearly-empty filesystem.
	#[test]
	fn hashalloc_visits_cgs_in_documented_order() {
		let ncg = 16u32;
		let pref = CgNum::new(3);
		let mut order = Vec::new();

		// 1. preferred
		order.push(pref.get());

		// 2. quadratic rehash
		let mut cg = pref.get() as u64;
		let mut i = 1u64;
		while i < ncg as u64 {
			cg = (cg + i) % ncg as u64;
			order.push(cg as u32);
			i *= 2;
		}

		// 3. brute force from preferred+2
		for k in 2..ncg as u64 {
			order.push(((pref.get() as u64 + k) % ncg as u64) as u32);
		}

		assert_eq!(order[0], 3);
		// +1, +2, +4, +8 from CG 3, all modulo 16.
		assert_eq!(&order[1..5], &[4, 6, 10, 2]);
		assert_eq!(order.len() as u64, 1 + 4 + (ncg as u64 - 2));

		// Every cylinder group is reachable.  Some are visited twice (the
		// quadratic rehash lands on a group the brute-force sweep also
		// covers); that is harmless, it is just one wasted probe of a
		// cylinder group already known to be full.
		let seen: std::collections::BTreeSet<u32> = order.iter().copied().collect();
		assert_eq!(seen.len(), ncg as usize);
	}
}
