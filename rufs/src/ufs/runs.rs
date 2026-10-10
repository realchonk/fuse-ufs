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

//! A walk of a file's block map, in runs.
//!
//! [`super::BlockMapping`] answers one offset.  This answers the whole file, in
//! the form consumers actually want: maximal runs of bytes that are backed by
//! consecutive blocks, and runs that are not backed at all.  `SEEK_DATA` and
//! `SEEK_HOLE` scan for exactly this shape, and so does anything else that has
//! to describe where a file lives.
//!
//! # "Run", not "run"
//!
//! UFS2 has no extents.  A file's data layout is a *block map* and nothing
//! else: `di_ext[]`'s twelve direct block pointers, and `di_extb[]`'s three
//! levels of indirect block, each entry naming one block.  There is no
//! run tree, no run header, and no way to record a run of allocated
//! blocks in a single on-disk structure -- a run of *n* consecutive blocks
//! costs *n* pointers here, where a filesystem with extents would cost one.
//!
//! The distinction matters beyond vocabulary.  "Extent" is the vocabulary of
//! FIEMAP and of filesystems like XFS and btrfs, and borrowing it here would
//! put a Linux API's mental model into the generic layer and invite the
//! assumption that UFS can answer run-shaped questions cheaply, which it
//! cannot.  A run is a property of *this* block map: the longest stretch the
//! pointers happen to describe continuously, and not a thing that is stored.
//!
//! # The traversal is over the pointer tree
//!
//! This is deliberately *not* `for lbn in 0..size/bs { map(lbn) }`.  A zero
//! pointer at single-indirect level proves that the next `fs_nindir` blocks —
//! 128 MiB on the golden image — are a hole, and the walk has to be able to say
//! so without reading them one at a time, or `SEEK_DATA` on a sparse file is
//! O(file size) and useless on exactly the files it exists for.
//!
//! The zones of the block map are fixed-size and contiguous, so the current
//! byte offset alone says which of them to open:
//!
//! ```text
//! | logical bytes      | zone                              |
//! |--------------------|-----------------------------------|
//! | 0 .. 12 * fs_bsize | di_ext[]   (the direct pointers)  |
//! |          .. pbp    | di_extb[0] (single indirect)      |
//! |          .. pbp^2  | di_extb[1] (double indirect)      |
//! |          .. pbp^3  | di_extb[2] (triple indirect)      |
//! ```
//!
//! where `pbp` is `fs_bsize / 8`.  Within a zone a non-zero pointer opens the
//! next level and a zero pointer is a hole covering the whole sub-tree.  Depth
//! is at most `UFS_NIADDR`, so the stack never grows large.
//!
//! # What a run guarantees
//!
//! For [`BlockRun::Data`], the `length` bytes starting at `logical` are
//! stored contiguously from the UFS block `physical`, and the run stops at
//! every discontinuity: a zero pointer, a physical break, or end of file.
//! Adjacent runs *are* merged across a direct-to-indirect boundary when the
//! physical blocks really do continue, because there the merge is provable and
//! splitting it again is the consumer's business.
//!
//! For [`BlockRun::Hole`], the `length` bytes starting at `logical` have no
//! block and read as zeroes.  Holes merge freely: they have no physical side to
//! disagree with.
//!
//! The file's last `Data` run can be shorter than `fs_bsize`, because
//! UFS2 accounts for `i_size % fs_bsize` as a run of `fs_fsize` fragments and
//! no more.  That run is not claimed to be contiguous with the block before it
//! unless the pointers actually say so.
//!
//! # Units
//!
//! `logical` and `length` are bytes into the file.  `physical` is a UFS block,
//! i.e. a fragment address, so a run's first byte is at image offset
//! `physical * fs_fsize`.  Turning that into a device byte offset is the
//! caller's job and belongs on the platform side of the boundary.

use std::num::NonZero;

use super::*;
use crate::data::InodeData;

/// One maximal run of a file's block map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRun {
	/// Bytes `logical..logical + length` have no block allocated.
	Hole {
		/// First byte of the run, from the start of the file.
		logical: u64,

		/// Length in bytes.
		length: u64,
	},

	/// Bytes `logical..logical + length` are backed by consecutive blocks
	/// starting at `physical`.
	Data {
		/// First byte of the run, from the start of the file.
		logical: u64,

		/// UFS block (a fragment address) holding the first byte of the run.
		physical: u64,

		/// Length in bytes.
		length: u64,
	},
}

impl BlockRun {
	/// First byte of the run.
	pub fn logical(&self) -> u64 {
		match self {
			Self::Hole { logical, .. } | Self::Data { logical, .. } => *logical,
		}
	}

	/// Length in bytes.
	pub fn length(&self) -> u64 {
		match self {
			Self::Hole { length, .. } | Self::Data { length, .. } => *length,
		}
	}

	/// One past the last byte of the run.
	pub fn end(&self) -> u64 {
		self.logical() + self.length()
	}

	/// Whether the run is backed by allocated blocks.
	pub fn is_data(&self) -> bool {
		matches!(self, Self::Data { .. })
	}

	/// The first UFS block of the run, if it is backed.
	pub fn physical(&self) -> Option<u64> {
		match self {
			Self::Data { physical, .. } => Some(*physical),
			Self::Hole { .. } => None,
		}
	}
}

/// One unit of the traversal: one logical block, one whole sub-tree, or the
/// file's tail.
///
/// Not a run.  A unit is what the pointer tree yields one at a time; a run is
/// several
/// units merged, and merging needs the *next* unit to be known, so the two are
/// kept apart.
#[derive(Debug, Clone, Copy)]
struct Unit {
	logical: u64,
	length:  u64,
	block:   Option<u64>,
}

impl Unit {
	fn to_run(self) -> BlockRun {
		match self.block {
			Some(physical) => {
				BlockRun::Data {
					logical: self.logical,
					physical,
					length: self.length,
				}
			}
			None => {
				BlockRun::Hole {
					logical: self.logical,
					length:  self.length,
				}
			}
		}
	}
}

/// One open level of the pointer tree.
///
/// `depth` is how many indirect levels are still *below* these entries, so an
/// entry covers `fs_bsize * pbp^depth` logical bytes:
///
/// * `depth == 0` — the entries are data pointers.  Both the inode's own
///   `di_ext[]` and the contents of a *single*-indirect block are of this kind,
///   which is the whole reason `depth` has to be counted from the bottom and
///   not from the top;
/// * `depth == 1` — the entries point at blocks of data pointers, so entry `i`
///   covers `pbp` logical blocks;
/// * `depth == 2` — the entries point at blocks of single-indirect pointers.
///
/// Getting this off by one is the classic way to write a sparse-file walker
/// that works until it does not: a zero entry would be reported as covering a
/// whole sub-tree instead of one block, and the answers would silently drift
/// past the first indirect level.
struct Level {
	depth:   u32,
	logical: u64,
	ptrs:    Vec<u64>,
	next:    usize,
}

/// What opening the next zone of the block map produced.
enum Zone {
	/// A frame was pushed; keep walking.
	Open,

	/// The zone's root pointer is zero, so the whole zone is one hole.
	Hole {
		/// First byte of the zone.
		logical: u64,
		/// How many bytes the zone covers.
		span:    u64,
	},

	/// Nothing above this point; the traversal is over.
	Done,
}

/// An iterator over the runs of a file's block map.
///
/// Created by [`Ufs::inode_block_runs`] or [`Ufs::inode_block_runs_from`].  Yields
/// `IoResult` because reading an indirect block can fail, and a mapping query
/// that stopped halfway without saying why would be worse than a slow one.
pub struct InodeBlockRuns<'a, R: Backend> {
	ufs: &'a mut Ufs<R>,
	ino: Inode,

	/// `fs_bsize`.
	bs:  u64,
	/// `fs_fsize`.
	fs:  u64,
	/// `fs_bsize / 8`: pointers per indirect block.
	pbp: u64,

	/// First byte the caller asked about; earlier units are clipped away.
	start: u64,
	/// Byte offset the walk has reached.
	pos:   u64,
	/// One past the last byte of the file.
	end:   u64,

	stack: Vec<Level>,

	/// The run being merged, and the one that finished before it.
	pending: Option<BlockRun>,
	ready:   Option<BlockRun>,
}

impl<'a, R: Backend> InodeBlockRuns<'a, R> {
	fn new(ufs: &'a mut Ufs<R>, inr: InodeNum, start: u64) -> IoResult<Self> {
		let ino = ufs.read_inode(inr)?;
		let bs = ufs.superblock.bsize();
		let fs = ufs.superblock.fsize();

		// An inode with no block map has no runs.  The only kind that gets
		// here with a non-zero size and no block map is a symlink whose target
		// fits inline in `UFS_SLLEN` bytes: there is no block to describe.
		let end = match ino.data {
			InodeData::Blocks(_) => ino.size,
			_ => 0,
		};

		Ok(Self {
			pos: start.min(end),
			ufs,
			ino,
			bs,
			fs,
			pbp: bs / size_of::<UfsDaddr>() as u64,
			start,
			end,
			stack: Vec::new(),
			pending: None,
			ready: None,
		})
	}

	/// Open the zone of the block map that contains the current position.
	///
	/// The zones are fixed-size and contiguous, so the position alone says which
	/// one to open and the walk never has to remember where it was.  The direct
	/// region is a zone like any other, except that its pointers are the inode's
	/// own rather than an indirect block's.
	fn enter_zone(&mut self) -> IoResult<Zone> {
		let InodeData::Blocks(blocks) = &self.ino.data else {
			return Ok(Zone::Done);
		};

		let direct = UFS_NDADDR as u64 * self.bs;
		if self.pos < direct {
			self.stack.push(Level {
				depth:   0,
				logical: 0,
				ptrs:    blocks.direct.iter().map(|p| *p as u64).collect(),
				next:    (self.pos / self.bs) as usize,
			});
			return Ok(Zone::Open);
		}

		let mut base = direct;
		for level in 0..UFS_NIADDR {
			let span = self.bs * self.pbp.pow(level as u32 + 1);
			if self.pos >= base + span {
				base += span;
				continue;
			}

			let block = blocks.indirect[level] as u64;
			if block == 0 {
				// The whole zone is one hole.  This is the case that makes
				// walking a sparse file cheap.
				return Ok(Zone::Hole {
					logical: base,
					span,
				});
			}

			let mut ptrs = vec![0u64; self.pbp as usize];
			self.ufs.read_pblock(block, &mut ptrs)?;
			self.stack.push(Level {
				// `depth == 0` for `indirect[0]`: the entries of a
				// single-indirect block are data pointers, exactly like
				// `di_ext[]`.
				depth: level as u32,
				logical: base,
				ptrs,
				next: ((self.pos - base) / span) as usize,
			});
			return Ok(Zone::Open);
		}

		Ok(Zone::Done)
	}

	/// The next unit the pointer tree yields, clipped to `start`.
	fn next_unit(&mut self) -> IoResult<Option<Unit>> {
		loop {
			if self.pos >= self.end {
				return Ok(None);
			}

			while self.stack.last().is_some_and(|l| l.next >= l.ptrs.len()) {
				self.stack.pop();
			}
			if self.stack.is_empty() {
				match self.enter_zone()? {
					Zone::Open => continue,
					Zone::Hole { logical, span } => {
						let hole = self.hole(logical, span);
						self.pos = (logical + span).min(self.end);
						return Ok(Some(hole));
					}
					Zone::Done => return Ok(None),
				}
			}

			let depth = self.stack.last().expect("non-empty").depth;
			let span = self.bs * self.pbp.pow(depth);
			let level = self.stack.last_mut().expect("non-empty");
			let idx = level.next;
			level.next += 1;
			let block = level.ptrs[idx];
			let logical = level.logical + idx as u64 * span;

			if depth == 0 {
				// A data pointer.  The file's last block may be shorter.
				let length = self.end.saturating_sub(logical).min(self.bs);
				self.pos = (logical + self.bs).min(self.end);
				return Ok(Some(self.unit(
					logical,
					length,
					NonZero::new(block).map(NonZero::get),
				)));
			}

			if block == 0 {
				// A zero pointer at indirect level: the whole sub-tree below it
				// is a hole, and saying so costs one comparison instead of
				// `pbp^depth` reads.
				let hole = self.hole(logical, span);
				self.pos = (logical + span).min(self.end);
				return Ok(Some(hole));
			}

			// A non-zero pointer at indirect level: descend.
			let mut ptrs = vec![0u64; self.pbp as usize];
			self.ufs.read_pblock(block, &mut ptrs)?;
			self.stack.push(Level {
				depth: depth - 1,
				logical,
				ptrs,
				next: 0,
			});
		}
	}

	/// A hole covering up to `span` bytes from `logical`.
	fn hole(&self, logical: u64, span: u64) -> Unit {
		let length = self.end.saturating_sub(logical).min(span);
		self.unit(logical, length, None)
	}

	/// Clip a unit back to the offset the caller asked about.
	///
	/// Clipping here rather than in the walk keeps `pos` advancing in whole
	/// blocks, which is what the zone arithmetic assumes.
	fn unit(&self, logical: u64, mut length: u64, block: Option<u64>) -> Unit {
		if logical < self.start {
			length = length.saturating_sub(self.start - logical);
		}
		let logical = logical.max(self.start);
		Unit {
			logical,
			length,
			block,
		}
	}

	/// Fold a unit into the run being merged, finishing the previous run when
	/// this unit cannot extend it.
	fn absorb(&mut self, unit: Unit) {
		if unit.length == 0 {
			return;
		}
		let fs = self.fs;

		let merged = match (self.pending, unit) {
			(None, _) => None,
			(
				Some(BlockRun::Hole { logical, length }),
				Unit {
					logical: l,
					length: run,
					block: None,
				},
			) if logical + length == l => {
				Some(BlockRun::Hole {
					logical,
					length: length + run,
				})
			}
			(
				Some(BlockRun::Data {
					logical,
					physical,
					length,
				}),
				Unit {
					logical: l,
					length: run,
					block: Some(b),
				},
			) if logical + length == l
				// `physical` counts fragments, so the expected next block is the
				// current one advanced by the bytes covered so far.  That
				// division is exact, because every run length is a multiple of
				// `fs`.
				&& b == physical + length / fs =>
			{
				Some(BlockRun::Data {
					logical,
					physical,
					length: length + run,
				})
			}
			_ => None,
		};

		match merged {
			Some(run) => self.pending = Some(run),
			None => {
				self.ready = self.pending.replace(unit.to_run());
			}
		}
	}
}

impl<R: Backend> Iterator for InodeBlockRuns<'_, R> {
	type Item = IoResult<BlockRun>;

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some(ready) = self.ready.take() {
				return Some(Ok(ready));
			}
			match self.next_unit() {
				Ok(Some(unit)) => self.absorb(unit),
				Ok(None) => return self.pending.take().map(Ok),
				Err(e) => {
					// The traversal is over either way; report once and do not
					// spin.  A caller that keeps iterating after an error gets
					// an empty iterator.
					self.end = 0;
					self.pos = 0;
					return Some(Err(e));
				}
			}
		}
	}
}

impl<R: Backend> Ufs<R> {
	/// Iterate over the runs of `inr`'s block map, from byte zero.
	///
	/// Reading the whole file, in one pass over the pointer tree.  See
	/// [`InodeBlockRuns`].
	pub fn inode_block_runs(&mut self, inr: InodeNum) -> IoResult<InodeBlockRuns<'_, R>> {
		InodeBlockRuns::new(self, inr, 0)
	}

	/// Iterate over the runs of `inr`'s block map at or after `start`.
	///
	/// The first run may begin before `start`, because a run is a property of
	/// the file and not of the query.  Clipping it is what keeps
	/// "these bytes are physically contiguous" true of the answer.
	///
	/// Starting part-way through matters for a sparse file: without it,
	/// `SEEK_DATA` near the end of a file with holes before it would walk every
	/// run in between.
	pub fn inode_block_runs_from(
		&mut self,
		inr: InodeNum,
		start: u64,
	) -> IoResult<InodeBlockRuns<'_, R>> {
		InodeBlockRuns::new(self, inr, start)
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::{policy::BlockRole, testutil, InodeNum, InodeType};

	/// Build a file of `size` bytes with data exactly in `ranges` and holes
	/// everywhere else.
	///
	/// `ranges` are `(byte offset, byte length)` pairs inside the direct block
	/// region.  `inode_write()` allocates every block it is asked to fill, so a
	/// genuine hole has to be made by writing the whole file and then clearing
	/// the pointers of the bytes that should not be there — which is also
	/// exactly how a hole arises on a real filesystem, and the only honest way
	/// to produce one in a test.
	///
	/// A logical block counts as data when *any* of its bytes falls in a range,
	/// so a range that does not start on a block boundary keeps the whole block
	/// it touches.  That is the right bias: an run boundary has to land on a
	/// block boundary to mean anything.
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
			let covered = ranges.iter().any(|(o, l)| *o < hi && lo < o + l);
			if !covered {
				clear_direct(ug, inr, lbn);
			}
		}

		inr
	}

	/// Zero one direct pointer behind the filesystem's back.
	fn clear_direct(ug: &mut Ufs<std::fs::File>, inr: InodeNum, lbn: u64) {
		let mut ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &mut ino.data else {
			panic!("{inr} has no block map");
		};
		assert!(
			(lbn as usize) < UFS_NDADDR,
			"clear_direct only handles direct blocks"
		);
		b.direct[lbn as usize] = 0;
		ug.write_inode(inr, &ino).unwrap();
	}

	/// Collect every run, failing the test on the first error.
	fn runs(ug: &mut Ufs<std::fs::File>, inr: InodeNum) -> Vec<BlockRun> {
		ug.inode_block_runs(inr)
			.unwrap()
			.collect::<IoResult<Vec<_>>>()
			.unwrap()
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

	/// An empty file has no runs at all: not a hole, because there are no
	/// bytes at all.
	#[test]
	fn empty_file_has_no_runs() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-e");
		assert!(runs(&mut ug, inr).is_empty());
	}

	/// A fully written file has one run per *physical* run of its pointers, and
	/// the runs tile the file exactly.  The count is derived from the inode
	/// rather than assumed, because "a dense file is one run" would be a
	/// statement about the allocator rather than about the mapping -- and on the
	/// golden image it is false, because the first block of a new file does not
	/// land next to the ones after it.
	#[test]
	fn dense_file_runs_are_the_physical_runs() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = create(&mut ug, "zzz-d");
		let bs = ug.superblock.bsize();
		let size = 3 * bs + 100;
		ug.inode_write(inr, 0, &vec![7u8; size as usize]).unwrap();

		let want = physical_runs(&mut ug, inr);
		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), want, "{e:?} vs {want} physical runs");
		assert!(e.iter().all(BlockRun::is_data));
		assert_eq!(e[0].logical(), 0);
		assert_eq!(e.last().expect("non-empty").end(), size);
		assert_eq!(e.iter().map(BlockRun::length).sum::<u64>(), size);
	}

	/// How many runs a file's logical blocks fall into, by its own pointers.
	fn physical_runs(ug: &mut Ufs<std::fs::File>, inr: InodeNum) -> usize {
		let ino = ug.read_inode(inr).unwrap();
		let bs = ug.superblock.bsize();
		let fs = ug.superblock.fsize();
		let mut runs = 0;
		let mut prev = None;
		for lbn in 0..ino.size.div_ceil(bs) {
			match ug.inode_resolve_block(inr, &ino, lbn).unwrap() {
				None => prev = None,
				Some(b) => {
					// A run continues only when this block is the previous one
					// advanced by a whole block.
					if prev.map(|p| p + bs / fs) != Some(b.get()) {
						runs += 1;
					}
					prev = Some(b.get());
				}
			}
		}
		runs
	}

	/// `Data`, `Hole`, `Data`: the shape every one of these tests is a version
	/// of.  The hole has to come out as *one* run even though it spans whole
	/// indirect blocks.
	#[test]
	fn data_hole_data() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-dhd", 6 * bs, &[(0, bs), (4 * bs, 2 * bs)]);

		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), 3, "{e:?}");
		assert!(e[0].is_data() && e[0].logical() == 0 && e[0].length() == bs);
		assert!(!e[1].is_data() && e[1].logical() == bs && e[1].length() == 3 * bs);
		assert!(e[2].is_data() && e[2].logical() == 4 * bs && e[2].length() == 2 * bs);
		// The runs tile the file with no gap and no overlap.
		assert_eq!(e[0].end(), e[1].logical());
		assert_eq!(e[1].end(), e[2].logical());
	}

	/// A hole at the start, and the first run after it.
	#[test]
	fn hole_then_data() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-hd", 3 * bs, &[(2 * bs, bs)]);

		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), 2, "{e:?}");
		assert_eq!(
			e[0],
			BlockRun::Hole {
				logical: 0,
				length:  2 * bs,
			}
		);
		assert_eq!(e[1].logical(), 2 * bs);
		assert_eq!(e[1].length(), bs);
	}

	/// Data, then a hole that runs to the end of the file.  End of file is
	/// *not* turned into data, and a `Data` run never claims the bytes past
	/// `i_size`.
	#[test]
	fn data_then_hole_to_eof() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-dh", 3 * bs, &[(0, bs)]);

		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), 2, "{e:?}");
		assert!(e[0].is_data());
		assert_eq!(
			e[1],
			BlockRun::Hole {
				logical: bs,
				length:  2 * bs,
			}
		);
		assert_eq!(e[1].end(), ug.read_inode(inr).unwrap().size);
	}

	/// A file whose blocks are all there, with a physical break forced in the
	/// middle, must split the run.  Coalescing that ignores the physical side
	/// would hand a caller a run that is not on the disk.
	#[test]
	fn a_physical_break_splits_a_run() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = create(&mut ug, "zzz-break");
		ug.inode_write(inr, 0, &vec![3u8; (3 * bs) as usize])
			.unwrap();
		// Move the first block far away, leaving the second and third where they
		// are: one break, then a run that must still merge.
		let mut ino = ug.read_inode(inr).unwrap();
		let second = match &ino.data {
			InodeData::Blocks(b) => b.direct[1] as u64,
			_ => panic!(),
		};
		let far = second + 1000 * ug.superblock.frag();
		let InodeData::Blocks(ref mut b) = ino.data else {
			panic!();
		};
		b.direct[0] = far as i64;
		ug.write_inode(inr, &ino).unwrap();

		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), 2, "the physical break was coalesced away: {e:?}");
		assert_eq!(e[0].logical(), 0);
		assert_eq!(e[0].physical(), Some(far));
		assert_eq!(e[0].length(), bs);
		assert_eq!(
			e[1],
			BlockRun::Data {
				logical:  bs,
				physical: second,
				length:   2 * bs,
			},
			"the two contiguous blocks behind the break still merge"
		);
	}

	/// A hole covering most of a single-indirect block is one run, not
	/// `fs_nindir` of them.  This is the property that makes the traversal
	/// worth having: 4095 logical blocks of hole cost one comparison.
	///
	/// The indirect block is built by hand rather than by writing 128 MiB,
	/// because the golden image is 94% full and does not have that many blocks.
	/// Two of its entries name real data and the rest are zero, which is the
	/// shape a file truncated in the middle leaves behind.
	#[test]
	fn a_whole_indirect_block_hole_is_one_run() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let pbp = bs / 8;
		let inr = create(&mut ug, "zzz-big-hole");

		// One real data block behind the first indirect block.
		ug.inode_write(inr, 0, &vec![9u8; ((UFS_NDADDR as u64 + 1) * bs) as usize])
			.unwrap();
		let d0 = {
			let ino = ug.read_inode(inr).unwrap();
			let InodeData::Blocks(b) = &ino.data else {
				panic!();
			};
			// `fs_cpolicy` puts the first data block behind the first
			// indirect block, one whole block after it.
			b.indirect[0] as u64 + ug.superblock.frag()
		};

		// A fresh indirect block with a big hole in it.
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
		let mut blk = vec![0u64; pbp as usize];
		blk[0] = d0;
		ug.write_pblock(ib, &blk).unwrap();

		let mut ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(ref mut b) = ino.data else {
			panic!();
		};
		b.indirect[0] = ib as i64;
		// Reach 1000 logical blocks past the data, with nothing allocated
		// there: a file that was truncated down and then re-extended.
		ino.size = (UFS_NDADDR as u64 + 1001) * bs;
		ug.write_inode(inr, &ino).unwrap();

		// What matters here is the hole, not the count of the leading runs: the
		// direct blocks come in as many runs as the allocator happened to make.
		let e = runs(&mut ug, inr);
		assert_eq!(
			e.last(),
			Some(&BlockRun::Hole {
				logical: (UFS_NDADDR as u64 + 1) * bs,
				length:  1000 * bs,
			}),
			"{e:?}"
		);
		// And it is a single run: the allocator could not have produced 1000
		// separate holes out of one indirect block.
		assert_eq!(e.iter().filter(|x| !x.is_data()).count(), 1, "{e:?}");
		// The hole really is that big: 1000 blocks, skipped in one step.
		assert!(e.last().expect("non-empty").length() > 30 * 1024 * 1024);
	}

	/// Starting part-way clips the first run instead of restarting it, and
	/// does not change any of the others.
	#[test]
	fn starting_part_way_clips_the_first_run() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(&mut ug, "zzz-clip", 5 * bs, &[(0, bs), (4 * bs, bs)]);

		let all = runs(&mut ug, inr);
		let from: Vec<BlockRun> = ug
			.inode_block_runs_from(inr, bs / 2)
			.unwrap()
			.collect::<IoResult<Vec<_>>>()
			.unwrap();
		assert_eq!(from.len(), all.len());
		assert_eq!(from[0].logical(), bs / 2);
		assert_eq!(from[0].length(), bs / 2, "the run is clipped, not split");
		for (a, b) in all.iter().skip(1).zip(from.iter().skip(1)) {
			assert_eq!(a, b);
		}

		// Starting in a hole reports the hole from the start offset.
		let from: Vec<BlockRun> = ug
			.inode_block_runs_from(inr, bs + 7)
			.unwrap()
			.collect::<IoResult<Vec<_>>>()
			.unwrap();
		assert_eq!(from[0].logical(), bs + 7);
		assert!(!from[0].is_data());
	}

	/// Runs must agree with the single-offset mapping, run by run.  The two
	/// are independent answers to the same question, so a disagreement is a bug
	/// in one of them and this is what catches which.
	#[test]
	fn runs_agree_with_the_single_offset_mapping() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bs = ug.superblock.bsize();
		let inr = sparse(
			&mut ug,
			"zzz-agree",
			8 * bs,
			&[(0, 2 * bs), (5 * bs, 3 * bs)],
		);
		let size = ug.read_inode(inr).unwrap().size;

		let fs = ug.superblock.fsize();
		for run in runs(&mut ug, inr) {
			// The run's first byte names the run's first block.
			let m = ug.inode_map_offset(inr, run.logical()).unwrap();
			assert_eq!(m.is_data(), run.is_data(), "at {}", run.logical());
			assert_eq!(m.block(), run.physical());

			// And every block boundary inside it follows from that one, which is
			// the contiguity the run claims.
			for b in 1..=run.length().div_ceil(bs) {
				let probe = run.logical() + b * bs;
				if probe >= run.end() {
					break;
				}
				let m = ug.inode_map_offset(inr, probe).unwrap();
				assert_eq!(m.is_data(), run.is_data(), "offset {probe}");
				if run.is_data() {
					assert_eq!(
						m.block(),
						// Physical block numbers count fragments, so a run
						// advances a whole `fs_bsize` block at a time.
						run.physical().map(|p| p + b * bs / fs),
						"offset {probe}"
					);
				} else {
					assert_eq!(m.block(), None, "offset {probe} is in a hole");
				}
			}
		}
		// And EOF is EOF, not a hole.
		assert_eq!(
			ug.inode_map_offset(inr, size).unwrap(),
			super::super::mapping::BlockMapping::Eof
		);
	}

	/// A directory's runs are ordinary data runs.
	#[test]
	fn directories_have_runs() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let inr = ug
			.mkdir(InodeNum::ROOT, OsStr::new("zzz-ext"), 0o755, 0, 0)
			.unwrap()
			.inr;
		let e = runs(&mut ug, inr);
		assert_eq!(e.len(), 1, "{e:?}");
		assert!(e[0].is_data());
		assert_eq!(e[0].length(), ug.read_inode(inr).unwrap().size);
	}
}
