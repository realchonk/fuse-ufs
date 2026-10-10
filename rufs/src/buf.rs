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

//! A buffer cache with explicit dirty and safe-to-write state.
//!
//! # Why a dirty buffer is not the same thing as a write
//!
//! Everything this driver did before this module existed had one rule: modify
//! metadata, then write it to the device immediately.  That rule is why a
//! FUSE `mkdir` could not be made crash-safe without rewriting the allocator,
//! the directory code and the inode code all at once, and it is the single
//! structural reason `sys/ufs/ffs/ffs_softdep.c` is 9000 lines: FreeBSD had to
//! build a second, shadow copy of every in-core metadata object (a "safe" copy
//! and a "live" copy) plus a dependency graph to decide which of the two to
//! write, because the VFS path could no longer be changed to delay writes.
//!
//! Here the split is explicit from the start:
//!
//! * the **block device interface** ([`BlockDevice`]) is raw `read_at`/`write_at`
//!   and nothing else;
//! * the **buffer cache** ([`BufferCache`]) owns contents, dirtiness, and the
//!   *live* versus *safe* distinction, and is the only thing that talks to the
//!   device;
//! * the **dependency engine** decides when it is safe to write a buffer, and
//!   tells the cache via [`BufferCache::write_back`].
//!
//! A buffer is therefore dirty long before it is writable, and the two events
//! are separate calls.  Filesystem operations call [`BufferCache::get_mut`],
//! which marks the buffer dirty; only the dependency engine gets to write.
//!
//! # Live versus safe
//!
//! [`Buffer`] carries two images:
//!
//! * the **live** image, what the running filesystem believes is on disk and
//!   what `readdir` must see;
//! * an optional **safe** image, the last state of the buffer that is safe to
//!   make persistent.
//!
//! They differ exactly when the buffer contains a pointer to a block that is not
//! yet safe to write — a new indirect block that has not been initialised, a
//! directory entry naming an inode that has not been written.  Concretely, if a
//! live indirect block is `[A B C D]` and `C` is a freshly allocated block whose
//! bitmap bit is not yet on disk, the safe image is `[A B 0 D]`: writing the live
//! image would leave a persistent pointer to a block the cylinder-group bitmap
//! still calls free, and `fsck_ffs` would report a block that is simultaneously
//! allocated and owned.
//!
//! The safe image is produced by
//! [`BufferCache::unsafe_write`] plus [`BufferCache::publish`]: a caller says
//! "these byte ranges are not safe yet", the cache maintains a safe image that
//! differs from the live image only in those ranges, and `publish` clears the
//! pending-unsafe marks when the dependency that was blocking them resolves.
//!
//! # Why not reuse `BlockReader`
//!
//! [`crate::blockreader::BlockReader`] is a streaming `Read + Write + Seek`
//! adapter with a one-block cache that flushes on every write.  It is the right
//! thing for *file data*, where the kernel is the only writer and there are no
//! cross-block ordering constraints.  It is the wrong thing for metadata, which
//! is what this module is for.  Keeping them separate is deliberate: a file
//! data write must never be delayed by, or ordered against, a metadata write.

use std::{
	collections::{BTreeMap, BTreeSet, VecDeque},
	io::{Error as IoError, Read, Result as IoResult, Seek, Write},
};

/// A raw, fixed-size block device.
///
/// The buffer cache knows nothing else about the medium.  `BlockReader` is a
/// `Backend`, and therefore a `BlockDevice`; so is a plain `File`; so is the
/// in-memory device the tests use.  This is the seam that lets the cache be
/// tested without a filesystem image.
pub trait BlockDevice {
	/// Fill `buf` from byte offset `off`.
	fn read_at(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()>;

	/// Write `buf` at byte offset `off`.
	fn write_at(&mut self, off: u64, buf: &[u8]) -> IoResult<()>;
}

impl<T: Read + Write + Seek> BlockDevice for T {
	fn read_at(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()> {
		seek(self, off)?;
		self.read_exact(buf)?;
		Ok(())
	}

	fn write_at(&mut self, off: u64, buf: &[u8]) -> IoResult<()> {
		seek(self, off)?;
		self.write_all(buf)?;
		Ok(())
	}
}

fn seek(s: &mut impl Seek, off: u64) -> IoResult<()> {
	use std::io::SeekFrom;
	s.seek(SeekFrom::Start(off))?;
	Ok(())
}

/// One cached filesystem block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
	blk:          u64,
	/// The live contents: what the running filesystem reads and writes.
	data:         Vec<u8>,
	/// The last contents known to be safe to make persistent.
	///
	/// `None` means "the live contents are safe", which is the common case and
	/// costs nothing.
	safe:         Option<Vec<u8>>,
	/// The live contents differ from what the device holds.
	dirty:        bool,
	/// The device holds the *safe* image of this buffer, i.e. the last
	/// write-back was a safe one and the live image is still waiting on a
	/// dependency.
	///
	/// Without this flag a safe write-back would have to either leave the
	/// buffer clean -- and silently discard the live changes -- or leave it
	/// dirty, in which case every `write_back_all()` would rewrite it
	/// forever.
	safe_written: bool,
	/// Byte ranges of the live contents that are *not* safe to write.
	///
	/// Held as a sorted set of `(offset, len)` pairs rather than as a
	/// separately maintained `safe` image, because that is the form the
	/// dependency engine can produce: a dependency says "bytes A..B of this
	/// buffer are unsafe until I resolve", and resolving it is just a
	/// `remove`.  `safe_image()` materialises the safe view on demand.
	unsafe_:      BTreeSet<(u64, u64)>,
	/// Pin count: a buffer with a live pin is never evicted.
	pins:         u32,
}

impl Buffer {
	/// The filesystem block number this buffer holds.
	pub fn blk(&self) -> u64 {
		self.blk
	}

	/// The live contents.
	pub fn data(&self) -> &[u8] {
		&self.data
	}

	/// The live contents, for modification.
	pub fn data_mut(&mut self) -> &mut [u8] {
		&mut self.data
	}

	/// Whether the live contents differ from the device.
	pub fn is_dirty(&self) -> bool {
		self.dirty
	}

	/// Whether anything in the live contents is waiting for a dependency.
	pub fn has_unsafe_ranges(&self) -> bool {
		!self.unsafe_.is_empty()
	}

	/// Whether it is safe to write the live contents right now.
	///
	/// This is the question the dependency engine is built to answer.  A dirty
	/// buffer with no pending-unsafe ranges is safe; a dirty buffer with
	/// pending-unsafe ranges is not, and writing it anyway is precisely the bug
	/// Soft Updates exists to prevent.
	pub fn is_safe_to_write(&self) -> bool {
		self.dirty && self.unsafe_.is_empty()
	}

	/// Mark `len` bytes at `off` as not yet safe to write.
	pub fn mark_unsafe(&mut self, off: u64, len: u64) {
		if len == 0 {
			return;
		}
		// Merge with any overlapping or adjacent range so that the set cannot
		// grow without bound under repeated marking.
		let end = off + len;
		let mut merged = (off, end);
		let mut out = BTreeSet::new();
		for &(lo, hi) in &self.unsafe_ {
			if hi < merged.0 || lo > merged.1 {
				out.insert((lo, hi));
				continue;
			}
			merged = (merged.0.min(lo), merged.1.max(hi));
		}
		out.insert(merged);
		self.unsafe_ = out;
	}

	/// Clear the pending-unsafe mark covering `off..off+len`.
	///
	/// This is what a resolving dependency calls.  A merged range is *split*
	/// rather than dropped, because publishing one pointer in an indirect block
	/// must not make its neighbours writable.
	pub fn publish(&mut self, off: u64, len: u64) {
		if len == 0 {
			return;
		}
		let (plo, phi) = (off, off + len);
		let mut out = BTreeSet::new();
		for &(lo, hi) in &self.unsafe_ {
			if hi < plo || lo > phi {
				out.insert((lo, hi));
				continue;
			}
			if lo < plo {
				out.insert((lo, plo));
			}
			if hi > phi {
				out.insert((phi, hi));
			}
		}
		self.unsafe_ = out;
	}

	/// Whether this buffer currently needs a write-back.
	///
	/// A buffer that has had a *safe* write-back does not: its safe part is on
	/// the device and the rest is waiting on a dependency, so offering it again
	/// would spin.
	pub fn needs_write(&self) -> bool {
		self.dirty && !(self.has_unsafe_ranges() && self.safe_written)
	}

	/// Mark the whole buffer safe.
	pub fn publish_all(&mut self) {
		self.unsafe_.clear();
	}

	/// The contents that are safe to write.
	///
	/// Equal to the live contents unless part of them is pending a dependency,
	/// in which case the pending ranges are read as zero — which is the
	/// on-disk representation of "no pointer here".
	pub fn safe_image(&self) -> Vec<u8> {
		if self.unsafe_.is_empty() {
			return self.data.clone();
		}
		let mut out = self.data.clone();
		for &(lo, hi) in &self.unsafe_ {
			let lo = lo.min(out.len() as u64) as usize;
			let hi = (hi.min(out.len() as u64)) as usize;
			out[lo..hi].fill(0);
		}
		out
	}
}

/// What a write-back did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
	/// The buffer was not dirty; nothing was written.
	Clean,
	/// The safe image was written; the live image has pending unsafe ranges.
	Safe,
	/// The live image was written.
	Full,
}

/// Statistics, for tests and for `tunefs`-style reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
	/// Buffers currently held.
	pub resident: usize,
	/// Buffers with unsaved changes.
	pub dirty:    usize,
	/// Buffers with pending-unsafe ranges.
	pub unsafe_:  usize,
	/// Device reads issued.
	pub reads:    u64,
	/// Device writes issued.
	pub writes:   u64,
}

/// A cache of filesystem blocks with explicit dirty and safe-to-write state.
///
/// The cache is deliberately *not* a general-purpose LRU: it has no eviction
/// policy beyond "drop what was asked for", because a metadata cache in a FUSE
/// driver is bounded by the filesystem's working set and a clever eviction
/// policy here would be a source of bugs with no measurable benefit.  The
/// dependency engine is what bounds its contents: buffers are dropped as soon
/// as the operations that pinned them complete.
pub struct BufferCache {
	bsize:   u64,
	fsize:   u64,
	buffers: BTreeMap<u64, Buffer>,
	/// Dirty buffers, oldest first.
	order:   VecDeque<u64>,

	/// Gates holding buffers from being written, keyed by the block and the
	/// opaque token the caller recognises.
	///
	/// The tokens are opaque here on purpose: this layer knows about bytes and
	/// blocks, and [`crate::softdep`] knows about dependencies.  The owner
	/// keeps the token-to-gate mapping and calls [`Self::release`] when one
	/// opens, so the dependency types never reach down here.
	blocked: BTreeMap<u64, BTreeSet<u64>>,

	/// Blocks whose current contents are on the device.
	persisted: BTreeSet<u64>,
	stats:     Stats,
}

impl BufferCache {
	/// A cache over blocks of `bsize` bytes addressed in `fsize`-byte units.
	pub fn new(bsize: u64, fsize: u64) -> Self {
		assert!(bsize > 0 && fsize > 0 && bsize.is_multiple_of(fsize));
		Self {
			bsize,
			fsize,
			buffers: BTreeMap::new(),
			order: VecDeque::new(),
			blocked: BTreeMap::new(),
			persisted: BTreeSet::new(),
			stats: Stats::default(),
		}
	}

	/// Whether a buffer is held back by a gate, and therefore not written.
	///
	/// Unlike an unsafe *range*, this writes nothing wrong: the buffer simply
	/// stays dirty until the gate opens and the device keeps whatever was there
	/// before, which was -- by induction -- consistent.
	///
	/// That is the difference that matters, and it is why a whole buffer can be
	/// held back when a byte range inside one cannot.  `Buffer::safe_image()`
	/// holds a range back by *zeroing* it, which is right for a pointer and
	/// wrong for a count; not writing the buffer at all is right for both.
	///
	/// Hold buffer `blk` from being written until the gate named by `token`
	/// opens.
	///
	/// Returns `false` if the gate has already opened, in which case there is
	/// nothing to hold back.
	pub fn block(&mut self, blk: u64, token: u64) -> bool {
		self.blocked.entry(blk).or_default().insert(token)
	}

	/// Release one gate, and forget the buffer if nothing is holding it.
	///
	/// Returns whether anything was released.
	pub fn release(&mut self, blk: u64, token: u64) -> bool {
		let Some(tokens) = self.blocked.get_mut(&blk) else {
			return false;
		};
		let had = tokens.remove(&token);
		if tokens.is_empty() {
			self.blocked.remove(&blk);
		}
		had
	}

	/// Whether `blk` is held back by any gate.
	pub fn is_blocked(&self, blk: u64) -> bool {
		self.blocked.get(&blk).is_some_and(|t| !t.is_empty())
	}

	/// Whether `blk`'s current contents -- everything not gated -- are on the
	/// device.
	///
	/// This is the question a deferred operation needs answered, and it is *not*
	/// the same as "is clean".  A buffer that was written safely and not touched
	/// since is clean, but so is one that was written safely and then gated
	/// again; the second has unsaved changes and this must say so.
	///
	/// It is also not derivable from `is_dirty()` at the point of asking,
	/// because a dirty buffer may have been written safely and left gated.  The
	/// answer is a property of the cache, kept up to date by the two events that
	/// change it -- a fetch and a write.
	pub fn is_persisted(&self, blk: u64) -> bool {
		self.persisted.contains(&blk)
	}

	/// Block size.
	pub fn bsize(&self) -> u64 {
		self.bsize
	}

	/// Fragment size, the unit the device is addressed in.
	pub fn fsize(&self) -> u64 {
		self.fsize
	}

	/// Current statistics.
	pub fn stats(&self) -> &Stats {
		&self.stats
	}

	/// Block numbers of the buffers with unsaved changes, oldest first.
	pub fn dirty_order(&self) -> Vec<u64> {
		self.order.iter().copied().collect()
	}

	/// Is the block resident?
	pub fn is_resident(&self, blk: u64) -> bool {
		self.buffers.contains_key(&blk)
	}

	/// Read a block, fetching it from the device if necessary.
	///
	/// The returned bytes may be mutated in place; the buffer is *not* marked
	/// dirty.  Use [`Self::get_mut`] for that.
	pub fn get<'a>(&'a mut self, dev: &mut dyn BlockDevice, blk: u64) -> IoResult<&'a [u8]> {
		self.fetch(dev, blk)?;
		Ok(&self.buffers.get(&blk).expect("just inserted").data)
	}

	/// Read a block for modification.
	///
	/// The buffer is marked dirty immediately: a caller that takes a mutable
	/// reference and changes nothing has lost nothing but a write-back.
	pub fn get_mut<'a>(
		&'a mut self,
		dev: &mut dyn BlockDevice,
		blk: u64,
	) -> IoResult<&'a mut Buffer> {
		self.fetch(dev, blk)?;
		let b = self.buffers.get_mut(&blk).expect("just inserted");
		if !b.dirty {
			b.dirty = true;
			self.order.push_back(blk);
			self.stats.dirty += 1;
		}
		// A mutable borrow is the start of a change, so whatever the disk holds
		// is about to stop being true -- including for a buffer that was
		// written safely and is now being modified again.
		self.persisted.remove(&blk);
		Ok(b)
	}

	/// Borrow a resident buffer without touching the device.
	///
	/// Returns `None` if the block is not cached, so a caller that needs the
	/// contents regardless can fall back to [`Self::get`].
	pub fn peek(&self, blk: u64) -> Option<&Buffer> {
		self.buffers.get(&blk)
	}

	/// Borrow a resident buffer mutably, without marking it dirty.
	///
	/// This is how the dependency engine adjusts a buffer's *safe* state — the
	/// contents are already dirty, so marking them dirty again is wrong, and
	/// reading from the device would be a pointless I/O.
	pub fn peek_mut(&mut self, blk: u64) -> Option<&mut Buffer> {
		self.buffers.get_mut(&blk)
	}

	/// Write one buffer back to the device.
	///
	/// This is the *only* way a buffer reaches the medium, and the dependency
	/// engine is its only caller.  Writing a buffer that still has
	/// pending-unsafe ranges writes [`Buffer::safe_image`], not the live
	/// image, and reports [`Written::Safe`].
	pub fn write_back(&mut self, dev: &mut dyn BlockDevice, blk: u64) -> IoResult<Written> {
		let Some(b) = self.buffers.get(&blk) else {
			return Ok(Written::Clean);
		};
		// A blocked buffer stays dirty *and* stays in the dirty order, so this
		// is a skip rather than a removal: a later pass picks it up once the
		// gate has opened.
		if self.is_blocked(blk) {
			return Ok(Written::Clean);
		}
		if !b.needs_write() {
			return Ok(Written::Clean);
		}
		let unsafe_ = b.has_unsafe_ranges();
		let (image, written) = if unsafe_ {
			(b.safe_image(), Written::Safe)
		} else {
			(b.data.clone(), Written::Full)
		};
		dev.write_at(blk * self.bsize, &image)?;
		self.stats.writes += 1;
		// A write puts the buffer's current contents on the device: the safe
		// image if it was gated, the live image otherwise.
		self.persisted.insert(blk);
		let b = self.buffers.get_mut(&blk).expect("checked above");
		b.safe_written = unsafe_;
		// A safe write-back does *not* clean the buffer: the live image still
		// differs from the device, and it must be written once the
		// dependency that blocks it resolves.  For the same reason it stays in
		// the dirty order, so that `write_back_all` picks it up again when the
		// last gate opens.
		if b.dirty && !unsafe_ {
			self.stats.dirty -= 1;
		}
		b.dirty = unsafe_;
		b.safe = None;
		if !unsafe_ {
			self.order.retain(|&x| x != blk);
		}
		Ok(written)
	}

	/// Write every dirty buffer back, oldest first.
	pub fn write_back_all(&mut self, dev: &mut dyn BlockDevice) -> IoResult<Vec<(u64, Written)>> {
		// `order` may still contain blocks whose gates have not opened; those
		// are filtered out by `write_back` itself, which returns `Clean` for
		// anything that does not need a write.
		let order: Vec<u64> = self.order.iter().copied().collect();
		let mut out = Vec::with_capacity(order.len());
		for blk in order {
			let w = self.write_back(dev, blk)?;
			if w != Written::Clean {
				out.push((blk, w));
			}
		}
		Ok(out)
	}

	/// Drop a buffer, discarding its contents.
	///
	/// Dropping a *dirty* buffer loses data, so this is an error unless the
	/// buffer is clean.  Making that a checked operation rather than a silent
	/// truncation is the whole point of separating "dirty" from "written".
	pub fn drop_buffer(&mut self, blk: u64) -> IoResult<()> {
		if let Some(b) = self.buffers.get(&blk) {
			if b.dirty {
				log::error!("BufferCache::drop_buffer({blk}): buffer is still dirty");
				return Err(IoError::other("dropping a dirty buffer"));
			}
		}
		self.buffers.remove(&blk);
		self.order.retain(|&x| x != blk);
		Ok(())
	}

	/// Drop every buffer, discarding unsaved changes.
	pub fn drop_all(&mut self) {
		self.buffers.clear();
		self.order.clear();
	}

	/// True if no buffer is waiting to be written.
	pub fn is_clean(&self) -> bool {
		!self.buffers.values().any(Buffer::needs_write)
	}

	/// How many buffers have unsaved changes.
	///
	/// Deliberately not the negation of [`Self::is_clean`]: a buffer whose
	/// *safe* image has been written but which is still waiting on a
	/// dependency is not clean — its live image differs from the device — yet
	/// it does not need another write-back, so `write_back_all` correctly
	/// leaves it alone.  The two questions are separate, and conflating them
	/// is how a flush ends up either spinning forever or discarding the
	/// changes it was supposed to persist.
	pub fn dirty_count(&self) -> usize {
		self.buffers.values().filter(|b| b.is_dirty()).count()
	}

	fn fetch(&mut self, dev: &mut dyn BlockDevice, blk: u64) -> IoResult<()> {
		if self.buffers.contains_key(&blk) {
			return Ok(());
		}
		let mut data = vec![0u8; self.bsize as usize];
		dev.read_at(blk * self.bsize, &mut data)?;
		self.stats.reads += 1;
		// What came back is what the device holds, so the block is persisted
		// until something modifies it.  A fetch that finds it already cached is
		// not a fetch at all and leaves the answer alone.
		self.persisted.insert(blk);
		self.buffers.insert(
			blk,
			Buffer {
				blk,
				data,
				safe: None,
				dirty: false,
				safe_written: false,
				unsafe_: BTreeSet::new(),
				pins: 0,
			},
		);
		self.stats.resident = self.buffers.len();
		Ok(())
	}
}

#[cfg(test)]
mod t {
	use super::*;

	/// An in-memory device that records every write, so a test can assert
	/// exactly which bytes reached the medium and in which order.
	#[derive(Default)]
	struct MemDev {
		data:   Vec<u8>,
		writes: Vec<(u64, Vec<u8>)>,
	}

	impl BlockDevice for MemDev {
		fn read_at(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()> {
			let end = (off as usize + buf.len()).min(self.data.len());
			let start = (off as usize).min(end);
			buf[..end - start].copy_from_slice(&self.data[start..end]);
			// Beyond the written extent, as on a fresh sparse file, read zeros.
			Ok(())
		}

		fn write_at(&mut self, off: u64, buf: &[u8]) -> IoResult<()> {
			let end = off as usize + buf.len();
			if self.data.len() < end {
				self.data.resize(end, 0);
			}
			self.data[off as usize..end].copy_from_slice(buf);
			self.writes.push((off, buf.to_vec()));
			Ok(())
		}
	}

	const BSIZE: u64 = 4096;

	fn dev() -> MemDev {
		MemDev {
			data:   vec![0xAAu8; (BSIZE * 8) as usize],
			writes: Vec::new(),
		}
	}

	fn cache() -> BufferCache {
		BufferCache::new(BSIZE, BSIZE)
	}

	/// A read that misses the cache goes to the device, and a second read does
	/// not.
	#[test]
	fn read_populates_and_hits() {
		let mut c = cache();
		let mut d = dev();
		assert!(!c.is_resident(3));
		assert_eq!(c.get(&mut d, 3).unwrap()[0], 0xAA);
		assert!(c.is_resident(3));
		assert_eq!(c.stats().reads, 1);
		assert_eq!(c.get(&mut d, 3).unwrap()[0], 0xAA);
		assert_eq!(c.stats().reads, 1, "second read hit the device");
	}

	/// `get` must not dirty anything; `get_mut` must.
	#[test]
	fn only_get_mut_dirties() {
		let mut c = cache();
		let mut d = dev();
		c.get(&mut d, 1).unwrap();
		assert!(!c.peek(1).unwrap().is_dirty());
		assert!(c.is_clean());

		c.get_mut(&mut d, 1).unwrap();
		assert!(c.peek(1).unwrap().is_dirty());
		assert!(!c.is_clean());
		assert_eq!(c.dirty_order(), vec![1]);
	}

	/// The read/modify/mark-dirty/write cycle reaches the medium.
	#[test]
	fn write_back_reaches_the_device() {
		let mut c = cache();
		let mut d = dev();
		c.get_mut(&mut d, 2).unwrap().data[0..4].copy_from_slice(b"UFS2");
		assert_eq!(c.write_back(&mut d, 2).unwrap(), Written::Full);
		assert_eq!(&d.writes[0].0, &(2 * BSIZE));
		assert_eq!(&d.writes[0].1[0..4], b"UFS2");
		assert!(c.is_clean());
		assert_eq!(d.writes.len(), 1);
	}

	/// A clean buffer is not written, so an explicit sync of an untouched
	/// filesystem issues no I/O at all.
	#[test]
	fn clean_buffers_are_not_rewritten() {
		let mut c = cache();
		let mut d = dev();
		c.get(&mut d, 0).unwrap();
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Clean);
		assert!(d.writes.is_empty());
		assert!(c.write_back_all(&mut d).unwrap().is_empty());
	}

	/// `write_back_all` writes in the order the buffers became dirty, which is
	/// what lets a dependency engine express "bitmap before contents" without
	/// having to reorder anything.
	#[test]
	fn write_back_all_is_oldest_first() {
		let mut c = cache();
		let mut d = dev();
		for blk in [5u64, 1, 7, 3] {
			c.get_mut(&mut d, blk).unwrap().data[0] = blk as u8;
		}
		assert_eq!(c.dirty_order(), vec![5, 1, 7, 3]);
		let w = c.write_back_all(&mut d).unwrap();
		assert_eq!(w.len(), 4);
		assert_eq!(
			w.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
			vec![5, 1, 7, 3]
		);
		assert!(c.is_clean());
	}

	/// The safe image differs from the live image only inside the
	/// pending-unsafe ranges, and reads as zero there.
	#[test]
	fn safe_image_zeroes_only_unsafe_ranges() {
		let mut c = cache();
		let mut d = dev();
		let b = c.get_mut(&mut d, 0).unwrap();
		for (i, v) in [1u8, 2, 3, 4].iter().enumerate() {
			b.data[i * 8] = *v;
		}
		assert_eq!(b.safe_image(), b.data);

		// Mark the third pointer (bytes 16..24) unsafe: `[1 2 3 4]` becomes
		// `[1 2 0 0]`.
		b.mark_unsafe(16, 8);
		assert!(b.has_unsafe_ranges());
		assert!(!b.is_safe_to_write());
		let safe = b.safe_image();
		assert_eq!(safe[0], 1);
		assert_eq!(safe[8], 2);
		assert_eq!(&safe[16..24], &[0u8; 8]);
		assert_eq!(safe[24], 4);
	}

	/// Writing a buffer that is still partly unsafe writes the safe image.
	#[test]
	fn write_back_of_unsafe_buffer_writes_the_safe_image() {
		let mut c = cache();
		let mut d = dev();
		{
			let b = c.get_mut(&mut d, 0).unwrap();
			for (i, v) in [1u8, 2, 3, 4].iter().enumerate() {
				b.data[i * 8] = *v;
			}
			b.mark_unsafe(16, 8);
		}
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Safe);
		assert_eq!(d.writes[0].1[0], 1);
		assert_eq!(d.writes[0].1[8], 2);
		assert_eq!(&d.writes[0].1[16..24], &[0u8; 8]);
		assert_eq!(d.writes[0].1[24], 4);
	}

	/// The live contents survive a safe write-back: `readdir` must still see
	/// the new entry even though the disk does not.
	#[test]
	fn safe_write_back_keeps_the_live_contents() {
		let mut c = cache();
		let mut d = dev();
		{
			let b = c.get_mut(&mut d, 0).unwrap();
			b.data[16..24].copy_from_slice(&7u64.to_le_bytes());
			b.mark_unsafe(16, 8);
		}
		c.write_back(&mut d, 0).unwrap();
		assert_eq!(&c.peek(0).unwrap().data[16..24], &7u64.to_le_bytes());
		// It is clean now, so the next write-back is a full one.
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Clean);
	}

	/// A resolving dependency un-marks its range, and the buffer becomes safe.
	#[test]
	fn publish_resolves_a_dependency() {
		let mut c = cache();
		let mut d = dev();
		let b = c.get_mut(&mut d, 0).unwrap();
		b.mark_unsafe(0, 32);
		assert!(!b.is_safe_to_write());
		// Publishing part of a merged range splits it rather than dropping it.
		b.publish(16, 8);
		assert!(!b.is_safe_to_write(), "16..24 alone does not clear 0..32");
		// The device image is 0xAA throughout (see `dev()`); the pending ranges
		// are zeroed and the published one is not.
		let safe = b.safe_image();
		assert_eq!(safe[0], 0, "0..16 still pending");
		assert_eq!(safe[16], 0xAA, "16..24 was published");
		assert_eq!(safe[24], 0, "24..32 still pending");
		b.publish_all();
		assert!(b.is_safe_to_write());
	}

	/// Overlapping and adjacent marks merge, so a buffer with many small
	/// dependencies does not accumulate a range per dependency.
	#[test]
	fn unsafe_marks_merge() {
		let mut c = cache();
		let mut d = dev();
		let b = c.get_mut(&mut d, 0).unwrap();
		b.mark_unsafe(0, 10);
		b.mark_unsafe(5, 10);
		b.mark_unsafe(15, 1);
		assert_eq!(b.safe_image()[..16], [0u8; 16]);
		// Everything merged into one range 0..16.
		assert!(!b.is_safe_to_write());
		b.publish_all();
		assert!(b.is_safe_to_write());
	}

	/// Dropping a dirty buffer is refused; dropping a clean one works.
	#[test]
	fn dropping_a_dirty_buffer_is_refused() {
		let mut c = cache();
		let mut d = dev();
		c.get_mut(&mut d, 4).unwrap();
		assert!(c.drop_buffer(4).is_err());
		assert!(c.is_resident(4));
		c.write_back(&mut d, 4).unwrap();
		c.drop_buffer(4).unwrap();
		assert!(!c.is_resident(4));
		// Dropping a non-resident buffer is a no-op, not an error.
		c.drop_buffer(4).unwrap();
	}

	/// Statistics track reads, writes and residency.
	#[test]
	fn stats_track_activity() {
		let mut c = cache();
		let mut d = dev();
		c.get(&mut d, 0).unwrap();
		c.get(&mut d, 1).unwrap();
		c.get_mut(&mut d, 1).unwrap().data[0] = 1;
		assert_eq!(c.dirty_count(), 1);
		c.write_back_all(&mut d).unwrap();
		let s = c.stats();
		assert_eq!(s.resident, 2);
		assert_eq!(s.reads, 2);
		assert_eq!(s.writes, 1);
		assert_eq!(s.dirty, 0);
		assert_eq!(c.dirty_count(), 0);
	}

	/// A safe write-back leaves the buffer dirty — its live image still differs
	/// from the device — but it must not be offered a write again until the
	/// gate that blocked it opens.
	#[test]
	fn a_gated_buffer_stays_dirty_after_a_safe_write_back() {
		let mut c = cache();
		let mut d = dev();
		c.get_mut(&mut d, 0).unwrap().mark_unsafe(0, 8);
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Safe);
		assert_eq!(c.dirty_count(), 1, "the live image is still unsaved");
		assert_eq!(c.stats().dirty, 1);
		assert!(
			!c.peek(0).unwrap().needs_write(),
			"but it does not need a write"
		);
		// `is_clean` asks "does anything need writing right now", not "is
		// everything on the disk".  The honest answer is yes: the last half of
		// the buffer must not be written again until the gate that held it
		// back opens.
		assert!(c.is_clean());

		c.get_mut(&mut d, 0).unwrap().publish_all();
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Full);
		assert_eq!(c.dirty_count(), 0);
		assert_eq!(c.stats().dirty, 0);
		assert!(c.is_clean());
	}

	/// A realistic sequence: allocate a block (bitmap), initialise it
	/// (contents), then point at it (inode).  Only after the dependency that
	/// makes the pointer safe resolves does the full live image get written.
	#[test]
	fn allocation_then_publication() {
		let mut c = cache();
		let mut d = dev();

		// 1. The cylinder group bitmap has the new block marked allocated.
		let cg = c.get_mut(&mut d, 0).unwrap();
		cg.data[0] = 0b1111_0000;
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Full);

		// 2. The new block's contents are initialised.
		let blk = c.get_mut(&mut d, 9).unwrap();
		for b in blk.data.iter_mut() {
			*b = 0;
		}
		blk.data[0..8].copy_from_slice(&0xABCDu64.to_le_bytes());
		assert_eq!(c.write_back(&mut d, 9).unwrap(), Written::Full);

		// 3. The inode points at it, but the pointer is not yet known safe.
		let ino = c.get_mut(&mut d, 1).unwrap();
		ino.data[0..8].copy_from_slice(&9u64.to_le_bytes());
		ino.mark_unsafe(0, 8);
		assert_eq!(c.write_back(&mut d, 1).unwrap(), Written::Safe);
		assert_eq!(&d.writes[2].1[0..8], &0u64.to_le_bytes());

		// 4. The buffer is still dirty, but it is waiting on the dependency,
		//    so a flush must not offer it again.
		assert!(c.peek(1).unwrap().is_dirty());
		assert!(!c.peek(1).unwrap().needs_write());
		assert_eq!(c.write_back(&mut d, 1).unwrap(), Written::Clean);
		assert!(c.is_clean(), "nothing is pending a write right now");
		assert_eq!(d.writes.len(), 3, "no extra write was issued");

		// 5. The dependency resolves; the pointer becomes publishable.
		c.get_mut(&mut d, 1).unwrap().publish_all();
		assert!(c.peek(1).unwrap().needs_write());
		assert_eq!(c.write_back(&mut d, 1).unwrap(), Written::Full);
		assert_eq!(&d.writes[3].1[0..8], &9u64.to_le_bytes());
		assert!(!c.peek(1).unwrap().is_dirty());
	}

	/// `is_persisted` is the question a deferred operation asks, and it is not
	/// the same as "is clean".  These are the four states, and the fourth is the
	/// one a dirty flag cannot see.
	#[test]
	fn persisted_is_not_the_same_as_clean() {
		let mut c = cache();
		let mut d = dev();

		// A fresh fetch: what came back is what the disk holds.
		c.get(&mut d, 0).unwrap();
		assert!(c.is_persisted(0));
		assert!(!c.peek(0).unwrap().is_dirty());

		// Modified: not persisted.
		c.get_mut(&mut d, 0).unwrap().data[0] = 1;
		assert!(!c.is_persisted(0));

		// Written in full: persisted, and clean.
		c.write_back(&mut d, 0).unwrap();
		assert!(c.is_persisted(0));

		// Written safely and left gated: **persisted and dirty at the same
		// time**.  The device has everything except the gated range, and the
		// buffer is dirty precisely because that range is still unsaved.
		//
		// This single state is why `!is_dirty()` cannot answer the question: it
		// reports this buffer as unsaved, which is true of the gated range and
		// false of everything a deferred operation cares about.
		c.get_mut(&mut d, 0).unwrap().mark_unsafe(0, 4);
		assert_eq!(c.write_back(&mut d, 0).unwrap(), Written::Safe);
		assert!(c.is_persisted(0));
		assert!(c.peek(0).unwrap().is_dirty());

		// The state a dirty flag cannot express: persisted by a safe write, and
		// then modified again.  It has unsaved changes, so it is *not*
		// persisted -- while `is_dirty()` alone would have been needed to know
		// that, and would have been the wrong reason.
		c.get_mut(&mut d, 0).unwrap().data[0] = 2;
		assert!(!c.is_persisted(0));
		assert!(c.peek(0).unwrap().is_dirty());

		// A block the cache has never seen is not persisted: the cache does not
		// know what is on the device, and guessing would be worse.
		assert!(!c.is_persisted(99));
	}
}
