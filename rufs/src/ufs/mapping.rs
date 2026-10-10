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

//! Canonical UFS logical-to-physical block mapping.
//!
//! # Why this module
//!
//! "Where does byte *X* of this file live?" is a UFS question, and answering
//! it correctly requires understanding the inode's block map, the indirect
//! tree, UFS2 fragments and the sparse-file convention.  Three different
//! callers need that answer — FUSE `bmap`, `SEEK_DATA`/`SEEK_HOLE` and Linux
//! `FIEMAP` — and none of them should have to know any of that.  So it is
//! answered here, once, and the platform-specific parts stay in the FUSE
//! backend.
//!
//! The dependency direction is the one the rest of the crate follows:
//!
//! ```text
//!   FUSE BMAP / LSEEK / FIEMAP   <- platform ABI, knows nothing about UFS
//!            |
//!   Ufs::inode_map_offset         <- this module
//!            |
//!   live inode + indirect blocks  <- read through the metadata cache
//! ```
//!
//! Nothing here mentions `fiemap`, `SEEK_DATA` or any Linux constant, and
//! nothing in the UFS core should ever.
//!
//! # Live metadata, not the last image on the disk
//!
//! Every read this module performs goes through the metadata cache, so a
//! mapping query answers from the filesystem's *current* state even when
//! Soft Updates is holding part of that state back from the medium.  A file
//! whose fourth block was allocated a moment ago and whose inode has not been
//! written back maps that block, and mapping it does not force it out.  See
//! [`super::meta`] for why the cache is below this rather than above it.
//!
//! # Units
//!
//! | quantity | unit |
//! |---|---|
//! | `BlockMapping::Data.block` | a UFS block, i.e. a **fragment** address |
//! | its byte address | `block * fs_fsize` |
//! | `BlockMapping::Data.length` | bytes of the file covered by this mapping |
//! | the offsets passed in | bytes into the file |
//!
//! The fragment-address convention is UFS's, not a choice made here: `fs_fpg`
//! is a fragment count and `fs_iblkno` is a fragment offset, so every block
//! number on disk — direct pointers, indirect-block entries, `cgbase` sums —
//! is a fragment address.  A caller that wants a byte address multiplies by
//! `fs_fsize`; a caller that wants a device block index for FUSE `bmap`
//! divides that by the block size the *caller* asked about, which is not
//! necessarily `fs_fsize`.  That conversion belongs in the FUSE backend,
//! where the block size is known.
//!
//! # Sparse files
//!
//! A zero pointer means "no block", and the file reads as zeroes there.  That
//! is a hole, and it is not data: an allocated block whose bytes happen to be
//! zero is still [`BlockMapping::Data`].  The two are only ever told apart by
//! the pointer, never by the contents and never by `i_size`.  FreeBSD's
//! `ufs_bmap()` makes the same distinction, and reports `-1` for a block
//! inside the file that has no pointer.

use std::num::NonZeroU64;

use super::*;
use crate::{data::InodeData, BlockRun};

/// Where the bytes of a file offset live.
///
/// Every variant answers the same question for a *byte offset into the file*:
/// starting at that offset, what is stored there, and for how many bytes is
/// the answer unchanged?  `length` is what makes the answer usable for more
/// than a single block: a mapping that says `Data { block, 32768 }` lets a
/// caller coalesce without asking again, and one that says `Hole { 4096 }`
/// is a claim about 4096 bytes at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMapping {
	/// The offset is at or past `i_size`.
	///
	/// A distinct answer rather than a hole, because "there is nothing here
	/// because the file ended" and "there is nothing here because it was never
	/// allocated" lead to different answers for `SEEK_DATA`: the first is
	/// `ENXIO`, the second is a hole that a later offset may leave.
	Eof,

	/// Inside the file, with no block allocated.
	///
	/// Reading these bytes yields zeroes.
	Hole {
		/// Bytes, starting at the queried offset, covered by this answer.
		length: u64,
	},

	/// Inside the file and backed by a block.
	Data {
		/// The UFS block holding the data: a fragment address.
		block: u64,

		/// Bytes, starting at the queried offset, covered by this answer.
		///
		/// Less than `fs_bsize` only for the last block of a file, whose
		/// remaining `fs_bsize - i_size % fs_bsize` bytes UFS2 accounts for as a
		/// run of `fs_fsize` fragments.  Callers must not assume `fs_bsize`.
		length: u64,
	},
}

impl BlockMapping {
	/// Whether this mapping is backed by an allocated block.
	pub fn is_data(&self) -> bool {
		matches!(self, Self::Data { .. })
	}

	/// The bytes this mapping covers, or `0` at end of file.
	pub fn length(&self) -> u64 {
		match self {
			Self::Eof => 0,
			Self::Hole { length } | Self::Data { length, .. } => *length,
		}
	}

	/// The UFS block this mapping names, if it names one.
	pub fn block(&self) -> Option<u64> {
		match self {
			Self::Data { block, .. } => Some(*block),
			_ => None,
		}
	}
}

impl<R: Backend> Ufs<R> {
	/// Map the file offset `offset` of `inr` to where its bytes live.
	///
	/// The canonical answer, and the primitive every other mapping operation
	/// is built from.  See the module documentation for units and for why this
	/// reads the live image.
	///
	/// # Errors
	///
	/// `EIO` if the inode's size says the bytes exist but the geometry cannot
	/// place them, which means the inode and the superblock disagree; `EINVAL`
	/// if `inr` names an inode with no file type.
	pub fn inode_map_offset(&mut self, inr: InodeNum, offset: u64) -> IoResult<BlockMapping> {
		let ino = self.read_inode(inr)?;
		self.inode_map_offset_in(inr, &ino, offset)
	}

	/// [`Self::inode_map_offset`] against an already-loaded inode.
	///
	/// Separate so that the block-run iterator can hold one inode for its whole
	/// traversal instead of re-reading it per unit, and so that a caller that
	/// has just fetched an inode does not fetch it twice.
	pub(super) fn inode_map_offset_in(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		offset: u64,
	) -> IoResult<BlockMapping> {
		if offset >= ino.size {
			return Ok(BlockMapping::Eof);
		}

		// An inode without a block map has nothing to map.  The only kinds that
		// reach here with a non-zero size and no block map are symlinks whose
		// target fits in `UFS_SLLEN` bytes and is stored inline: there is no
		// block, and there is nothing for a block-mapped query to report.
		if !matches!(ino.data, InodeData::Blocks(_)) {
			return Ok(BlockMapping::Eof);
		}

		// `inode_locate` is `inode_find_block` without the panic: this function
		// is reachable with an arbitrary caller-supplied offset, and answering
		// "end of file" is a result, not a bug.
		let Some(info) = self.inode_locate(inr, ino, offset) else {
			log::error!(
				"inode_map_offset({inr}, {offset}): i_size {} is beyond the geometry",
				ino.size
			);
			return Err(err!(EIO));
		};

		// The mapping stops at whichever comes first: the end of the block, or
		// the end of the file.
		let length = (info.size - info.off).min(ino.size - offset);

		match self.inode_resolve_block(inr, ino, info.blkidx)? {
			Some(block) => {
				Ok(BlockMapping::Data {
					block: block.get(),
					length,
				})
			}
			None => Ok(BlockMapping::Hole { length }),
		}
	}

	/// Map the logical file block `lbn` of `inr`, in `fs_bsize` units.
	///
	/// A thin convenience over [`Self::inode_map_offset`] for callers whose
	/// unit of interest is a logical block rather than a byte — FUSE `bmap`
	/// being the obvious one.  Note that this is *not* the same as asking
	/// about `lbn * fs_bsize` when the file's last block is a fragment: the
	/// byte-offset form is the general one and this is the shorthand for a
	/// whole-block query.
	pub fn inode_map_block(&mut self, inr: InodeNum, lbn: u64) -> IoResult<BlockMapping> {
		let offset = lbn
			.checked_mul(self.superblock.bsize())
			.ok_or(err!(EOVERFLOW))?;
		self.inode_map_offset(inr, offset)
	}

	/// The filesystem's block size, `fs_bsize`.
	///
	/// The unit the buffer cache and the metadata blocks work in, and the unit
	/// the kernel reports as the superblock's `s_blocksize` for a FUSE mount.
	/// A mapping consumer needs it to convert a UFS block address into a device
	/// byte offset.
	pub fn bsize(&self) -> u64 {
		self.superblock.bsize()
	}

	/// The filesystem's fragment size, `fs_fsize`.
	///
	/// **Every** block number on a UFS2 disk -- direct pointers, indirect-block
	/// entries, the base of a cylinder group -- is a fragment address, so this
	/// is the multiplier that turns one into a byte offset.  Exposed because
	/// every mapping consumer needs it and getting it wrong is silent.
	pub fn fsize(&self) -> u64 {
		self.superblock.fsize()
	}

	/// The number of logical `fs_bsize` blocks a file occupies, including a
	/// final partial one.
	///
	/// `ceil(i_size / fs_bsize)`.  This is the *upper bound* on what
	/// [`Self::inode_map_block`] will be asked about; it says nothing about
	/// which of those blocks are allocated.
	pub fn inode_block_count(&mut self, inr: InodeNum) -> IoResult<u64> {
		let ino = self.read_inode(inr)?;
		Ok(ino.size.div_ceil(self.superblock.bsize()))
	}
}

impl<R: Backend> Ufs<R> {
	/// The physical block behind one logical file block.
	///
	/// The shape FreeBSD's `ufs_bmap()` reports, and the direct counterpart of
	/// [`BlockMapping`] for a caller that wants a plain number: a hole is
	/// `None`, which is the `-1` that `ufs_bmap()` returns.
	///
	/// # Why `blocksize` is a parameter
	///
	/// The caller supplies the block size, and it is *not* necessarily
	/// `fs_bsize`.  FUSE `bmap` gets it from the kernel's page size, and a
	/// caller that assumes the two agree will map the wrong block.  The
	/// resulting block is still a UFS block (a fragment address), because that
	/// is what this filesystem addresses in; turning it into a device byte
	/// offset is the caller's conversion, and belongs on the platform side of
	/// the boundary.
	///
	/// # Errors
	///
	/// `ENXIO` when `lbn` is past the end of the file.  That is deliberately
	/// distinct from `Ok(None)`: "there is no such block" and "that block is
	/// unallocated" are different answers, and only a caller that can tell them
	/// apart can map "hole" onto the FUSE ABI correctly.
	pub fn inode_bmap(&mut self, inr: InodeNum, lbn: u64, blocksize: u64) -> IoResult<Option<u64>> {
		let ino = self.read_inode(inr)?;
		if blocksize == 0 || lbn >= ino.size.div_ceil(blocksize) {
			return Err(err!(ENXIO));
		}
		Ok(self
			.inode_resolve_block(inr, &ino, lbn)?
			.map(NonZeroU64::get))
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::{testutil, InodeNum, InodeType};

	/// Create an empty regular file in the root directory.
	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// Both byte orders, so a byte-order assumption in the pointer maths cannot
	/// pass on one image and fail on the other.
	const IMAGES: [&str; 2] = ["ufs-little", "ufs-big"];

	/// An empty file maps nothing, and says so as end of file rather than as a
	/// hole — the two get different answers from `SEEK_DATA`.
	#[test]
	fn empty_file_is_end_of_file() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let inr = create(&mut ug, "zzz-empty");
			assert_eq!(
				ug.inode_map_offset(inr, 0).unwrap(),
				BlockMapping::Eof,
				"{name}"
			);
			assert_eq!(ug.inode_block_count(inr).unwrap(), 0, "{name}");
		}
	}

	/// Every byte of an allocated block is data, and the whole block is
	/// covered by one answer.
	#[test]
	fn a_written_block_is_data() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let inr = create(&mut ug, "zzz-data");
			ug.inode_write(inr, 0, &[0xa5u8; 32768]).unwrap();

			let bs = ug.superblock.bsize();
			let m = ug.inode_map_offset(inr, 0).unwrap();
			let Some(block) = m.block() else {
				panic!("{name}: offset 0 of a written file is not data");
			};
			assert!(block > 0, "{name}: block zero is never an allocation");
			assert_eq!(m.length(), bs, "{name}");

			// The same answer from inside the block, with the length clipped.
			let mid = ug.inode_map_offset(inr, bs / 2).unwrap();
			assert_eq!(mid.block(), Some(block), "{name}");
			assert_eq!(mid.length(), bs / 2, "{name}");
		}
	}

	/// A block of allocated zeroes is data.  Only a zero *pointer* is a hole,
	/// and conflating the two would make every freshly truncated file look
	/// sparse.
	#[test]
	fn an_allocated_block_of_zeroes_is_data() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-zeroes");
		ug.inode_write(inr, 0, &[0u8; 32768]).unwrap();
		let m = ug.inode_map_offset(inr, 0).unwrap();
		assert!(m.is_data(), "a zero-filled but allocated block is data");
	}

	/// The last block of a file is a fragment run: the mapping must report the
	/// file's remaining bytes, not a whole `fs_bsize`, because that is all the
	/// block was allocated for.
	#[test]
	fn the_last_block_is_shorter_than_a_filesystem_block() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-tail");
		let bs = ug.superblock.bsize();
		// One whole block plus a quarter of one.
		ug.inode_write(inr, 0, &vec![0u8; (bs + bs / 4) as usize])
			.unwrap();

		assert_eq!(
			ug.inode_map_offset(inr, bs).unwrap().length(),
			bs / 4,
			"the tail mapping must be clipped to i_size"
		);
		assert_eq!(
			ug.inode_map_offset(inr, bs + bs / 4).unwrap(),
			BlockMapping::Eof
		);
		// And it is still data.
		assert!(ug.inode_map_offset(inr, bs).unwrap().is_data());
	}

	/// A hole is a missing pointer, never an allocation decision: a file
	/// truncated to zero and re-extended through its old blocks is sparse, and
	/// the mapping must say so.  Building one by hand is the only way to get a
	/// genuine hole, because this implementation's `inode_write` allocates
	/// every block it writes.
	#[test]
	fn a_null_pointer_is_a_hole() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-sparse");
		let bs = ug.superblock.bsize();
		// Write two blocks, then clear the first pointer behind the
		// filesystem's back and stage the inode.
		ug.inode_write(inr, 0, &vec![0x11u8; (2 * bs) as usize])
			.unwrap();
		let mut ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &mut ino.data else {
			panic!("no block map");
		};
		let freed = b.direct[0] as u64;
		b.direct[0] = 0;
		ug.write_inode(inr, &ino).unwrap();

		assert_eq!(
			ug.inode_map_offset(inr, 0).unwrap(),
			BlockMapping::Hole { length: bs },
			"a cleared direct pointer is a hole"
		);
		assert!(
			ug.inode_map_offset(inr, bs).unwrap().is_data(),
			"the second block is untouched"
		);
		assert_eq!(ug.inode_block_count(inr).unwrap(), 2);

		// Restoring the pointer restores the mapping, which is what proves the
		// hole came from the pointer and not from a stale cached inode.
		let mut ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &mut ino.data else {
			panic!();
		};
		b.direct[0] = freed as i64;
		ug.write_inode(inr, &ino).unwrap();
		assert_eq!(
			ug.inode_map_offset(inr, 0).unwrap().block(),
			Some(freed),
			"the mapping follows the live inode"
		);
	}

	/// Blocks reached through an indirect block map, at every level, on both
	/// byte orders.  The assertion is that the mapping agrees with the block map
	/// — the placement *policy* is tested in `alloctest`, and duplicating it here
	/// would only add a second thing to keep in sync.
	#[test]
	fn indirect_blocks_map() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let inr = create(&mut ug, "zzz-ind");
			let bs = ug.superblock.bsize();
			// 12 direct blocks, then far enough to need a first indirect block.
			let lbn = 12 + 8;
			ug.inode_write(inr, 0, &vec![0x22u8; (bs * lbn) as usize])
				.unwrap();
			ug.sync_metadata().unwrap();

			let ino = ug.read_inode(inr).unwrap();
			let InodeData::Blocks(b) = &ino.data else {
				panic!();
			};
			assert_ne!(b.indirect[0], 0, "{name}: no first indirect block");

			for n in 0..lbn {
				let want = ug.inode_resolve_block(inr, &ino, n).unwrap();
				let m = ug.inode_map_offset(inr, n * bs).unwrap();
				assert_eq!(m.block(), want.map(|x| x.get()), "{name}: lbn {n}");
				assert!(m.is_data(), "{name}: lbn {n} is not data");
				assert_eq!(m.length(), bs, "{name}: lbn {n} length");
			}
			assert!(ug.check_consistency().unwrap().is_clean(), "{name}");
		}
	}

	/// An offset past the end of the file is end of file, not a hole, and a
	/// wildly out-of-range offset must not overflow into nonsense.
	#[test]
	fn out_of_range_offsets_are_end_of_file() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-oob");
		ug.inode_write(inr, 0, &[0u8; 32768]).unwrap();
		let size = ug.read_inode(inr).unwrap().size;
		assert_eq!(ug.inode_map_offset(inr, size).unwrap(), BlockMapping::Eof);
		assert_eq!(
			ug.inode_map_offset(inr, u64::MAX).unwrap(),
			BlockMapping::Eof
		);
		assert_eq!(
			ug.inode_map_block(inr, u64::MAX / ug.superblock.bsize())
				.unwrap(),
			BlockMapping::Eof
		);
		assert!(ug.inode_map_block(inr, u64::MAX).is_err());
	}

	/// The mapping answers from the live image, not the last flush.  This is
	/// the property the whole mapping layer exists for: Soft Updates may be
	/// holding this inode back from the disk, and a query must still see it.
	#[test]
	fn mapping_sees_unflushed_metadata() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-live");
		ug.inode_write(inr, 0, &[0x33u8; 32768]).unwrap();

		// The block maps now, with nothing flushed.
		assert!(ug.metadata_cache().dirty_count() > 0);
		let live = ug.inode_map_offset(inr, 0).unwrap();
		assert!(live.is_data());

		// After a flush the mapping is the same, and survives a remount.
		ug.sync_metadata().unwrap();
		drop(ug);
		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(ug.inode_map_offset(inr, 0).unwrap(), live);
	}

	/// A directory is block-mapped too, and its blocks are metadata in the
	/// FreeBSD sense — but they are ordinary data blocks to the mapping layer.
	#[test]
	fn directories_map() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-dir"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let ino = ug.read_inode(inr).unwrap();
		assert!(ino.size > 0);
		let m = ug.inode_map_offset(inr, 0).unwrap();
		assert!(m.is_data(), "a directory's first block is data");
		// A fresh directory holds `.` and `..` in one 512-byte directory block,
		// so its whole mapping is `i_size` bytes long, not `fs_bsize`.
		assert_eq!(m.length(), ino.size);
		assert_eq!(
			ug.inode_map_offset(inr, ino.size).unwrap(),
			BlockMapping::Eof
		);
	}

	/// `bmap_of` is the plain-number form: `None` for a hole, a block address
	/// for data, and `ENXIO` past the end of the file.
	#[test]
	fn inode_bmap_reports_a_plain_block() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-bmap");
		let bs = ug.superblock.bsize();
		ug.inode_write(inr, 0, &vec![0u8; (2 * bs) as usize])
			.unwrap();

		assert!(ug.inode_bmap(inr, 0, bs).unwrap().is_some());
		assert_eq!(
			ug.inode_bmap(inr, 5, bs).unwrap_err().raw_os_error(),
			Some(libc::ENXIO),
			"a block past the end is ENXIO, not a hole"
		);
		// The block size comes from the caller, so a coarser one covers more of
		// the file and must not be silently replaced by fs_bsize.
		assert!(ug.inode_bmap(inr, 0, bs * 4).unwrap().is_some());
		assert_eq!(
			ug.inode_bmap(inr, 1, bs * 4).unwrap_err().raw_os_error(),
			Some(libc::ENXIO),
			"the caller's block size decides where the file ends"
		);
		assert!(ug.inode_bmap(inr, 0, 0).is_err());
	}
}

/// `SEEK_DATA` and `SEEK_HOLE`, at the UFS level.
///
/// These live beside [`BlockMapping`] rather than in the run module because
/// they are the two questions a *caller* asks of a mapping, and their answers
/// are properties of the file, not of any platform's ABI.  Linux turns the
/// errors below into `ENXIO`; that translation is the FUSE backend's job.
///
/// Both are answered by scanning [`Ufs::inode_block_runs_from`], which walks the
/// pointer tree rather than the logical block indices, so a sparse region is
/// skipped in one step instead of one block at a time.
impl<R: Backend> Ufs<R> {
	/// The offset of the first byte of allocated data at or after `offset`.
	///
	/// "Allocated data" means a block is pointed at, never "the bytes are not
	/// zero": a block of zeroes that the inode points at is data, and a caller
	/// that asked where the file's data is did not ask what it contains.
	///
	/// The answer is never before `offset`.  A run that straddles it is clipped
	/// to it, which is what `lseek(SEEK_DATA)` means by "the next location at or
	/// after the specified offset where data has been written": asked from
	/// inside a run, the answer is the offset itself, and asked from inside a
	/// hole, it is the start of the next run.
	///
	/// # Errors
	///
	/// `ENXIO` when there is no data at or after `offset`.  That includes a
	/// fully sparse file, an empty file, and an `offset` at or past `i_size`;
	/// `EINVAL` if `inr` is not an inode.
	pub fn inode_seek_data(&mut self, inr: InodeNum, offset: u64) -> IoResult<u64> {
		let size = self.read_inode(inr)?.size;
		if offset >= size {
			// Past the end of the file there is no data, and that is ENXIO
			// rather than an error about the offset.
			log::trace!("inode_seek_data({inr}, {offset}): at EOF");
			return Err(err!(ENXIO));
		}

		let mut runs = self.inode_block_runs_from(inr, offset)?;
		loop {
			match runs.next() {
				Some(Ok(BlockRun::Data { logical, .. })) => return Ok(logical),
				Some(Ok(BlockRun::Hole { .. })) => continue,
				Some(Err(e)) => return Err(e),
				None => break,
			}
		}

		log::trace!("inode_seek_data({inr}, {offset}): no data");
		Err(err!(ENXIO))
	}

	/// The offset of the first hole at or after `offset`.
	///
	/// The end of the file is an implicit hole, so a fully allocated file has a
	/// hole at `i_size` and a fully sparse file has one at `offset`.  A hole is
	/// a missing pointer; a block of zeroes the inode points at is data and is
	/// never reported as a hole.
	///
	/// Unlike [`Self::inode_seek_data`] this does not fail at the end of the
	/// file: there is always a hole at or after `offset` within a file, and past
	/// the end `i_size` is the answer.  A caller that wants `ENXIO` for
	/// `offset >= i_size` instead gets `Ok(i_size)` here and has to check.
	///
	/// # Errors
	///
	/// `EINVAL` if `inr` is not an inode.  Not `ENXIO`: see above.
	pub fn inode_seek_hole(&mut self, inr: InodeNum, offset: u64) -> IoResult<u64> {
		let size = self.read_inode(inr)?.size;
		if offset >= size {
			return Ok(size);
		}

		let mut runs = self.inode_block_runs_from(inr, offset)?;
		loop {
			match runs.next() {
				Some(Ok(BlockRun::Hole { logical, .. })) => return Ok(logical),
				Some(Ok(BlockRun::Data { .. })) => continue,
				Some(Err(e)) => return Err(e),
				None => break,
			}
		}

		// No hole inside the file: the end of the file is one.
		Ok(size)
	}
}

/// Tests for [`Ufs::inode_seek_data`] and [`Ufs::inode_seek_hole`].
#[cfg(test)]
mod seek {
	use std::io::ErrorKind as Kind;

	use super::*;
	use crate::{policy::BlockRole, testutil, InodeNum, InodeType};

	/// `ENXIO`, however the platform spells it.
	fn is_enxio(e: &std::io::Error) -> bool {
		e.raw_os_error() == Some(libc::ENXIO)
	}

	/// Create an empty regular file in the root directory.
	fn create(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
		ug.mknod(
			InodeNum::ROOT,
			OsStr::new(name),
			InodeType::RegularFile,
			0o644,
			0,
			0,
		)
		.unwrap()
		.inr
	}

	/// A file of `size` bytes with data in `ranges` and holes elsewhere.
	///
	/// Clear entry `idx` of the indirect block `blk`.
	fn clear_indirect_entry(ug: &mut Ufs<std::fs::File>, blk: u64, idx: u64) {
		let pbp = ug.superblock.bsize() / 8;
		let mut entries = vec![0u64; pbp as usize];
		ug.read_pblock(blk, &mut entries).unwrap();
		entries[idx as usize] = 0;
		ug.write_pblock(blk, &entries).unwrap();
	}

	/// Zero the pointer to logical block `lbn`, whatever level it lives at.
	///
	/// This is the only honest way to produce a hole in a test: `inode_write`
	/// allocates every block it is asked to fill, so a sparse file has to be
	/// built dense and then have its pointers removed -- which is also exactly
	/// how a hole arises on a real filesystem.
	fn punch(ug: &mut Ufs<std::fs::File>, inr: InodeNum, lbn: u64) {
		let bs = ug.superblock.bsize();
		let pbp = bs / 8;
		let nd = UFS_NDADDR as u64;
		if lbn < nd {
			let mut ino = ug.read_inode(inr).unwrap();
			let InodeData::Blocks(ref mut b) = ino.data else {
				panic!();
			};
			b.direct[lbn as usize] = 0;
			ug.write_inode(inr, &ino).unwrap();
			return;
		}

		let (i0, lvl1) = ((lbn - nd) % pbp, (lbn - nd) / pbp);
		let (ib0, ib1, ib2) = match &ug.read_inode(inr).unwrap().data {
			InodeData::Blocks(b) => {
				(
					b.indirect[0] as u64,
					b.indirect[1] as u64,
					b.indirect[2] as u64,
				)
			}
			_ => panic!(),
		};
		if lvl1 == 0 {
			clear_indirect_entry(ug, ib0, i0);
		} else if lvl1 / pbp == 0 {
			clear_indirect_entry(ug, ib1, lvl1 % pbp);
		} else {
			clear_indirect_entry(ug, ib2, lvl1 / pbp);
		}
	}

	/// A file of `size` bytes with data in `ranges` and holes everywhere else.
	///
	/// `ranges` are `(byte offset, byte length)` pairs.  A logical block counts
	/// as data when any of its bytes falls in a range, so a run boundary
	/// always lands on a block boundary.
	fn sparse(
		ug: &mut Ufs<std::fs::File>,
		name: &str,
		size: u64,
		ranges: &[(u64, u64)],
	) -> InodeNum {
		let bs = ug.superblock.bsize();
		let inr = create(ug, name);
		if size == 0 {
			return inr;
		}
		ug.inode_write(inr, 0, &vec![0xa5u8; size as usize])
			.unwrap();

		for lbn in 0..size.div_ceil(bs) {
			let lo = lbn * bs;
			let hi = lo + bs;
			if !ranges.iter().any(|(o, l)| *o < hi && lo < o + l) {
				punch(ug, inr, lbn);
			}
		}
		inr
	}

	/// Data at offset 0, in every shape.
	#[test]
	fn seek_data_finds_data_at_zero() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-sd");
		ug.inode_write(inr, 0, &vec![1u8; 4096]).unwrap();
		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), 0);
		// Asked from inside the run, the answer is the offset itself: the run
		// has data there.
		assert_eq!(ug.inode_seek_data(inr, 17).unwrap(), 17);
	}

	/// An empty file has no data anywhere, and asking inside or past it is
	/// ENXIO rather than an error about the offset.
	#[test]
	fn empty_file_has_no_data() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-e");
		assert!(is_enxio(&ug.inode_seek_data(inr, 0).unwrap_err()));
		assert!(is_enxio(&ug.inode_seek_data(inr, 1).unwrap_err()));
	}

	/// A file with nothing but holes has no data, and one hole starting at the
	/// offset asked for.
	#[test]
	fn a_fully_sparse_file_has_no_data() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		// Allocate two blocks and then clear both pointers, so the file really
		// is sparse rather than merely empty.
		let inr = sparse(&mut ug, "zzz-sp", 2 * bs, &[]);
		let size = ug.read_inode(inr).unwrap().size;
		assert_eq!(size, 2 * bs);

		assert!(is_enxio(&ug.inode_seek_data(inr, 0).unwrap_err()));
		assert!(is_enxio(&ug.inode_seek_data(inr, bs).unwrap_err()));
		assert!(is_enxio(&ug.inode_seek_data(inr, 2 * bs).unwrap_err()));
		assert_eq!(ug.inode_seek_hole(inr, 0).unwrap(), 0);
		assert_eq!(ug.inode_seek_hole(inr, bs).unwrap(), bs);
	}

	/// Data after a large hole: the hole must be skipped in one step, and the
	/// answer is the start of the data, not the start of the file.
	#[test]
	fn data_after_a_large_hole() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-big", 40 * bs, &[(30 * bs, bs)]);

		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), 30 * bs);
		assert_eq!(ug.inode_seek_data(inr, 29 * bs).unwrap(), 30 * bs);
		// Inside the data, and just after it: no more data either way.
		assert_eq!(ug.inode_seek_data(inr, 30 * bs).unwrap(), 30 * bs);
		assert!(is_enxio(&ug.inode_seek_data(inr, 31 * bs).unwrap_err()));
		assert_eq!(ug.inode_seek_hole(inr, 0).unwrap(), 0);
		assert_eq!(ug.inode_seek_hole(inr, 30 * bs).unwrap(), 31 * bs);
	}

	/// Several holes: each answer is the start of the *next* run, not of the
	/// current one.
	#[test]
	fn several_holes() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(
			&mut ug,
			"zzz-many",
			9 * bs,
			&[(bs, bs), (3 * bs, bs), (5 * bs, 3 * bs)],
		);

		// data at 1, 3, 5..8
		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), bs);
		assert_eq!(ug.inode_seek_data(inr, bs).unwrap(), bs);
		assert_eq!(ug.inode_seek_data(inr, 2 * bs).unwrap(), 3 * bs);
		assert_eq!(ug.inode_seek_data(inr, 4 * bs).unwrap(), 5 * bs);
		assert!(is_enxio(&ug.inode_seek_data(inr, 8 * bs).unwrap_err()));

		assert_eq!(ug.inode_seek_hole(inr, 0).unwrap(), 0);
		assert_eq!(ug.inode_seek_hole(inr, bs).unwrap(), 2 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 3 * bs).unwrap(), 4 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 5 * bs).unwrap(), 8 * bs);
		// Asked from inside the last hole, the answer is the offset itself.
		assert_eq!(ug.inode_seek_hole(inr, 8 * bs).unwrap(), 8 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 8 * bs + 7).unwrap(), 8 * bs + 7);
	}

	/// A fully allocated file has a hole exactly at the end of the file.
	#[test]
	fn a_fully_allocated_file_has_a_hole_at_eof() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-full");
		let bs = ug.superblock.bsize();
		let size = 2 * bs + 77;
		ug.inode_write(inr, 0, &vec![2u8; size as usize]).unwrap();
		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), 0);
		assert!(is_enxio(&ug.inode_seek_data(inr, size).unwrap_err()));
		assert_eq!(ug.inode_seek_hole(inr, 0).unwrap(), size);
		assert_eq!(ug.inode_seek_hole(inr, bs).unwrap(), size);
	}

	/// Exactly at EOF, and well beyond it.
	#[test]
	fn at_and_beyond_eof() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-eof", 4 * bs, &[(0, bs)]);
		let size = ug.read_inode(inr).unwrap().size;

		assert!(is_enxio(&ug.inode_seek_data(inr, size).unwrap_err()));
		assert!(is_enxio(&ug.inode_seek_data(inr, size + 1).unwrap_err()));
		assert!(is_enxio(&ug.inode_seek_data(inr, u64::MAX).unwrap_err()));
		// SEEK_HOLE answers instead of failing: the end of the file is a hole.
		assert_eq!(ug.inode_seek_hole(inr, size).unwrap(), size);
		assert_eq!(ug.inode_seek_hole(inr, size + 1).unwrap(), size);
		assert_eq!(ug.inode_seek_hole(inr, u64::MAX).unwrap(), size);
	}

	/// Asking inside a data block, and inside a hole, both work.
	#[test]
	fn from_inside_a_run() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-in", 4 * bs, &[(bs, bs)]);

		// Inside the data: it is already the run we are in, so the answer is
		// the offset itself.
		assert_eq!(ug.inode_seek_data(inr, bs + 5).unwrap(), bs + 5);
		assert_eq!(ug.inode_seek_hole(inr, bs + 5).unwrap(), 2 * bs);
		// Inside the hole before it.
		assert_eq!(ug.inode_seek_data(inr, 7).unwrap(), bs);
		// Inside the hole: already there.
		assert_eq!(ug.inode_seek_hole(inr, 7).unwrap(), 7);
		// Inside the hole after it.
		assert!(is_enxio(&ug.inode_seek_data(inr, 3 * bs).unwrap_err()));
		assert_eq!(ug.inode_seek_hole(inr, 3 * bs).unwrap(), 3 * bs);
	}

	/// The final fragment: the last run is shorter than a filesystem block, and
	/// both answers have to respect `i_size` rather than rounding it up.
	#[test]
	fn the_final_fragment() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let fs = ug.superblock.fsize();
		let inr = sparse(&mut ug, "zzz-frag", bs + 3 * fs, &[(0, bs + 3 * fs)]);
		let size = ug.read_inode(inr).unwrap().size;
		assert_eq!(size, bs + 3 * fs);

		// The tail is three fragments of a block, and it is data; the hole is
		// at the end of the file rather than anywhere inside the run.
		assert_eq!(ug.inode_seek_data(inr, size - 1).unwrap(), size - 1);
		assert_eq!(ug.inode_seek_data(inr, bs).unwrap(), bs);
		assert_eq!(ug.inode_seek_hole(inr, size - 1).unwrap(), size);
		assert!(is_enxio(&ug.inode_seek_data(inr, size).unwrap_err()));
	}

	/// Data that crosses from the direct blocks into a single-indirect block, so
	/// the answers have to survive the zone boundary.
	#[test]
	fn across_the_direct_to_indirect_boundary() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-bound");
		// 12 direct blocks plus 4 more behind the first indirect block.
		ug.inode_write(inr, 0, &vec![4u8; (16 * bs) as usize])
			.unwrap();

		// Punch out the second and last logical blocks behind the first
		// indirect block, leaving data at 12, 14 and 15.
		punch(&mut ug, inr, 13);
		punch(&mut ug, inr, 16);

		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), 0);
		assert_eq!(ug.inode_seek_data(inr, 12 * bs).unwrap(), 12 * bs);
		// Block 13 is a hole, so the next data is block 14.
		assert_eq!(ug.inode_seek_data(inr, 13 * bs).unwrap(), 14 * bs);
		assert_eq!(ug.inode_seek_data(inr, 15 * bs).unwrap(), 15 * bs);
		assert!(is_enxio(&ug.inode_seek_data(inr, 16 * bs).unwrap_err()));

		assert_eq!(ug.inode_seek_hole(inr, 12 * bs).unwrap(), 13 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 13 * bs).unwrap(), 13 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 14 * bs).unwrap(), 16 * bs);
		assert_eq!(ug.inode_seek_hole(inr, 15 * bs).unwrap(), 16 * bs);
	}

	/// A hole covering most of a single-indirect block is skipped whole: the
	/// answers must not depend on how many logical blocks are inside it.
	#[test]
	fn across_a_whole_indirect_block_hole() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-ihole");

		// One real data block behind the first indirect block, then a hand-built
		// indirect block that is nothing but a hole.
		ug.inode_write(inr, 0, &vec![5u8; ((UFS_NDADDR as u64 + 1) * bs) as usize])
			.unwrap();
		let d0 = {
			let ino = ug.read_inode(inr).unwrap();
			let InodeData::Blocks(b) = &ino.data else {
				panic!();
			};
			b.indirect[0] as u64 + ug.superblock.frag()
		};

		let ib = ug
			.blk_alloc_zeroed_for(
				BlockRole::Indirect { first: true },
				inr,
				UFS_NDADDR as u64,
				0,
				0,
				0,
			)
			.unwrap()
			.get();
		let mut blk = vec![0u64; (bs / 8) as usize];
		blk[0] = d0;
		ug.write_pblock(ib, &blk).unwrap();

		let mut ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(ref mut b) = ino.data else {
			panic!();
		};
		b.indirect[0] = ib as i64;
		ino.size = (UFS_NDADDR as u64 + 1001) * bs;
		ug.write_inode(inr, &ino).unwrap();

		// The direct blocks are data, so the first answer is the offset; the
		// interesting one is that the 1000-block hole after the single data
		// block is stepped over rather than walked.
		assert_eq!(ug.inode_seek_data(inr, 0).unwrap(), 0);
		assert_eq!(
			ug.inode_seek_data(inr, UFS_NDADDR as u64 * bs).unwrap(),
			UFS_NDADDR as u64 * bs
		);
		assert!(is_enxio(
			&ug.inode_seek_data(inr, (UFS_NDADDR as u64 + 1) * bs)
				.unwrap_err()
		));
		// The first hole is after the single data block behind the first
		// indirect block, because the twelve direct blocks are all data.
		assert_eq!(
			ug.inode_seek_hole(inr, 0).unwrap(),
			(UFS_NDADDR as u64 + 1) * bs
		);
		assert_eq!(
			ug.inode_seek_hole(inr, (UFS_NDADDR as u64 + 1) * bs)
				.unwrap(),
			(UFS_NDADDR as u64 + 1) * bs
		);
	}

	/// A seek must not change anything, in particular it must not write.
	#[test]
	fn seeking_does_not_persist_anything() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-ro", 4 * bs, &[(bs, bs)]);
		let dirty = ug.metadata_cache().dirty_count();

		ug.inode_seek_data(inr, 0).unwrap();
		ug.inode_seek_hole(inr, 0).unwrap();
		ug.inode_seek_data(inr, 3 * bs).unwrap_err();
		ug.inode_seek_hole(inr, 3 * bs).unwrap();

		assert_eq!(
			ug.metadata_cache().dirty_count(),
			dirty,
			"a mapping query must not dirty anything"
		);
	}

	/// A file that is not an inode at all is EINVAL, not a wrong answer.
	#[test]
	fn an_unreadable_inode_is_rejected() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bogus = unsafe { InodeNum::new(ug.superblock.ipg + 1) };
		assert_eq!(
			ug.inode_seek_data(bogus, 0).unwrap_err().kind(),
			Kind::InvalidInput
		);
		assert_eq!(
			ug.inode_seek_hole(bogus, 0).unwrap_err().kind(),
			Kind::InvalidInput
		);
	}

	/// Both answers have to agree with the mapping they are derived from, at
	/// every offset, for a file with holes on both sides of a direct-to-indirect
	/// transition.
	#[test]
	fn answers_agree_with_the_mapping() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-cross", 6 * bs, &[(bs, bs), (4 * bs, bs)]);
		let size = ug.read_inode(inr).unwrap().size;

		for off in (0..size.div_ceil(bs)).map(|n| n * bs) {
			// `SEEK_DATA` never goes backwards, and the byte it lands on really
			// is data.  It returns `off` itself exactly when `off` is already in
			// a run, which is the check that would catch a walker that skipped
			// the run containing the offset.
			match ug.inode_seek_data(inr, off) {
				Ok(at) => {
					assert!(at >= off, "seek_data({off}) went back to {at}");
					assert!(at < size);
					assert!(
						ug.inode_map_offset(inr, at).unwrap().is_data(),
						"seek_data({off}) = {at} is not data"
					);
					assert!(
						at == off || !ug.inode_map_offset(inr, off).unwrap().is_data(),
						"seek_data({off}) = {at} skipped data at the offset itself"
					);
				}
				Err(e) => {
					assert!(is_enxio(&e), "{e}");
					assert!(
						!ug.inode_map_offset(inr, off).unwrap().is_data(),
						"seek_data({off}) failed although there is data there"
					);
				}
			}

			// `SEEK_HOLE` never goes backwards either, and returns the offset
			// itself exactly when the offset is already in a hole.
			let hole = ug.inode_seek_hole(inr, off).unwrap();
			assert!(hole >= off && hole <= size, "hole {hole} out of range");
			if hole < size {
				assert!(
					!ug.inode_map_offset(inr, hole).unwrap().is_data(),
					"{hole} is not a hole"
				);
			}
			assert!(
				hole == off || ug.inode_map_offset(inr, off).unwrap().is_data(),
				"seek_hole({off}) = {hole} skipped the hole at the offset itself"
			);
		}
	}
}
