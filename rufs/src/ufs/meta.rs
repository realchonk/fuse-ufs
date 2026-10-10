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

//! The metadata buffer seam.
//!
//! # Why this module exists
//!
//! The architectural rule the Soft Updates work rests on is that *no UFS
//! metadata mutation may reach the device directly*.  Enforcing that by hand
//! would mean scattering `self.buf.get_mut(...)` and `blk * fs_bsize`
//! arithmetic through every filesystem function, which is exactly the kind of
//! mechanical repetition that gets one call site wrong.  So all of it lives
//! here, once, and the rest of the crate goes through the handful of helpers
//! below.
//!
//! # The three layers, and which one this is
//!
//! ```text
//! UFS metadata mutation
//!     -> BufferCache::get_mut()      <- this module: the live image
//!     -> modify the live image
//!     -> create a dependency / gate   <- crate::softdep
//!     -> dependency resolution
//!     -> Buffer::publish()            <- crate::softdep
//!     -> BufferCache::write_back()    <- the only thing that writes
//!     -> BlockDevice::write_at()      <- crate::decoder
//! ```
//!
//! * [`crate::softdep::DependencyEngine`] decides *which parts* of a live
//!   image may be persisted.
//! * [`crate::buf::BufferCache`] decides *when*, and is the only caller of the
//!   device.
//! * this module is below both of them: it turns "a metadata structure changed"
//!   into "these bytes of that cached block changed", and nothing more.
//!
//! Getting a helper into the right one of those three boxes is the whole
//! design discipline of the series; a helper that writes to the device, or one
//! that decides on its own that a range is safe, belongs nowhere near this file.
//!
//! # Serialization is separate from persistence
//!
//! [`Ufs::metadata_write`] serializes a structure with this filesystem's byte
//! order and copies the bytes into a cached block.  It does *not* write the
//! block out: that is the cache's business.  Keeping the two apart is what lets
//! an existing `self.file.encode_at(...)` call site be converted without
//! changing a single byte of the image — `Decoder::encode_to_vec` produces
//! exactly the bytes `Decoder::encode_at` would have written — and it is what
//! lets a gated byte range inside one of those structures be held back while
//! the rest of the block goes out normally.
//!
//! # This is a *metadata* cache
//!
//! Ordinary file data does not come through here.  `inode_read_block()` and
//! `inode_write_block()` are deliberately still direct `Read`/`Write` calls on
//! the decoder: the kernel's page cache is already buffering that data, there
//! are no cross-block ordering constraints between two blocks of a file's
//! payload, and adding a second buffer layer for it would turn every FUSE
//! writeback into an ordering question for no benefit.  See the module
//! documentation of [`crate::buf`] for the same distinction.
//!
//! # Units
//!
//! Two different block numbers meet in this file, and conflating them is the
//! single easiest way to corrupt an image:
//!
//! | unit | size | used by |
//! |---|---|---|
//! | UFS "block" | `fs_fsize`, a *fragment* | `blk_alloc_for`, `cg_start`, `ino_to_fsba`, every on-disk pointer |
//! | `BufferCache` block | `fs_bsize` | [`Ufs::metadata_blk`], the cache, the device |
//!
//! UFS addresses in fragments because `fs_fpg` — the size of a cylinder group —
//! is a fragment count and `fs_iblkno` is a fragment offset, so `cg_start()` and
//! `ino_to_fsba()` both return fragment addresses even though they are named as
//! blocks.  [`Ufs::metadata_blk`] and [`Ufs::metadata_blk_off`] are the only
//! places that conversion happens.

use bincode_next::{Decode, Encode};

use super::*;
use crate::{
	buf::{Buffer, Written},
	softdep::DepId,
};

/// A metadata structure that does not fit in the filesystem block it starts in.
///
/// UFS2 lays out every metadata structure so that it does not straddle a
/// `fs_bsize` boundary — that is what `fs_cgsize_struct() < fs_bsize` and
/// `fs_inopb * UFS_INOSZ == fs_bsize` guarantee — so this is a corrupt
/// superblock or a programming error, not a runtime condition to recover from.
fn straddles(what: &str, at: u64, len: usize, bsize: u64) -> IoResult<()> {
	let end = at + len as u64;
	if end > bsize {
		iobail!(
			ErrorKind::InvalidInput,
			"{what} at offset {at} does not fit in an {bsize}-byte filesystem block \
			 (needs {len} bytes from {at}, overruns to {end})"
		);
	}
	Ok(())
}

impl<R: Backend> Ufs<R> {
	/// The [`crate::buf::BufferCache`] block holding the UFS block `blk`.
	///
	/// See the module documentation on units.  Concretely, a UFS block `blk` is
	/// at image byte offset `blk * fs_fsize`, and `BufferCache` addresses
	/// `fs_bsize`-byte units, so the conversion is a division by `fs_frag`.
	pub(super) fn metadata_blk(&self, blk: u64) -> u64 {
		blk / self.superblock.frag()
	}

	/// Read-only view of the cached metadata block `blk`.
	///
	/// `blk` is a [`crate::buf::BufferCache`] block number, i.e. a `fs_bsize`
	/// unit — see [`Ufs::metadata_blk`].
	///
	/// This is the *live* image, so a block that has been modified but not yet
	/// persisted reads back with its modifications.  That is the whole point:
	/// `readdir` has to see a directory entry that Soft Updates is still
	/// holding back.
	pub(super) fn metadata_block(&mut self, blk: u64) -> IoResult<&[u8]> {
		// A block that was not in the cache is about to be read from the
		// device, so whatever comes back is what the disk holds.  A block that
		// *was* in the cache is left alone: its answer is whatever the last
		// write or the last modification said, and re-deriving it from the
		// dirty flag on every read is what made a *pending* free look like it
		// had already run.
		let fresh = !self.buf.is_resident(blk);
		let Self { file, buf, .. } = self;
		buf.get(file, blk)?;
		let b = buf.peek(blk).expect("just fetched");
		if fresh {
			self.softdep.set_container_persisted(blk, true);
		}
		Ok(b.data())
	}

	/// Mutable view of the cached metadata block `blk`, marked dirty.
	///
	/// Marking dirty on borrow is the right default: a caller that takes a
	/// mutable buffer and changes nothing has lost nothing but a write-back,
	/// whereas a caller that mutates without marking dirty would lose data.
	pub(super) fn metadata_block_mut(&mut self, blk: u64) -> IoResult<&mut Buffer> {
		let Self { file, buf, .. } = self;
		let out = buf.get_mut(file, blk);
		// Handing out a mutable buffer is the start of a change, so whatever the
		// disk holds is about to stop being true.
		//
		// This is the *only* place that clears the flag, and it clears it for
		// every buffer, not only the dirty ones: a buffer that was written
		// safely and is now being modified again is not persisted, and
		// `is_dirty()` would say so only by accident.
		self.softdep.set_container_persisted(blk, false);
		out
	}

	/// Copy `bytes` into the cached metadata block `blk` at `off`, dirtying it.
	///
	/// The length is checked against the block: a metadata write that ran off
	/// the end of a block would silently overwrite whatever structure follows
	/// it, which for a cylinder group is the block bitmap.
	pub(super) fn metadata_write_range(
		&mut self,
		blk: u64,
		off: u64,
		bytes: &[u8],
	) -> IoResult<()> {
		straddles("metadata write", off, bytes.len(), self.superblock.bsize())?;
		let b = self.metadata_block_mut(blk)?;
		let start = off as usize;
		b.data_mut()[start..start + bytes.len()].copy_from_slice(bytes);
		Ok(())
	}

	/// Fill `len` bytes at `off` of the cached metadata block `blk` with `b`.
	///
	/// This is how an inode is cleared and how a released indirect-block entry
	/// is zeroed.  It goes through the cache like any other metadata write, so
	/// a cleared inode is a dirty buffer rather than a device write.
	pub(super) fn metadata_fill(&mut self, blk: u64, off: u64, b: u8, len: usize) -> IoResult<()> {
		straddles("metadata fill", off, len, self.superblock.bsize())?;
		let buf = self.metadata_block_mut(blk)?;
		let start = off as usize;
		buf.data_mut()[start..start + len].fill(b);
		Ok(())
	}

	/// Load and deserialize the metadata structure at image byte offset `off`.
	///
	/// Reads the *live* image, so a structure that has been written but not
	/// yet persisted is returned as it was written.  This is what makes the
	/// cache invisible to the rest of the filesystem: no caller needs to know
	/// whether a given structure is on the disk yet.
	pub(super) fn metadata_read<T: Decode<()>>(&mut self, off: u64) -> IoResult<T> {
		let blk = off / self.superblock.bsize();
		let at = off % self.superblock.bsize();
		// Read the remainder of the block: `decode_slice` then rejects a
		// structure that does not fit, rather than reading past the block.
		let config = self.file.config();
		let data = self.metadata_block(blk)?;
		config.decode_slice(&data[at as usize..])
	}

	/// Serialize `x` and stage it in the cached block at image byte offset
	/// `off`.
	///
	/// Does *not* persist anything; see the module documentation.
	pub(super) fn metadata_write<T: Encode>(&mut self, off: u64, x: &T) -> IoResult<()> {
		let bytes = self.file.encode_to_vec(x)?;
		self.metadata_write_range(
			off / self.superblock.bsize(),
			off % self.superblock.bsize(),
			&bytes,
		)
	}

	/// Fill `len` bytes at image byte offset `off` with `b`.
	///
	/// See [`Self::metadata_fill`]; this is the byte-offset form, for callers
	/// that have an on-disk structure address rather than a block number.
	pub(super) fn metadata_fill_at(&mut self, off: u64, b: u8, len: usize) -> IoResult<()> {
		self.metadata_fill(
			off / self.superblock.bsize(),
			off % self.superblock.bsize(),
			b,
			len,
		)
	}

	/// Copy `bytes` to image byte offset `off`.
	pub(super) fn metadata_write_at(&mut self, off: u64, bytes: &[u8]) -> IoResult<()> {
		self.metadata_write_range(
			off / self.superblock.bsize(),
			off % self.superblock.bsize(),
			bytes,
		)
	}

	/// Copy `len` bytes out of the cached block at image byte offset `off`.
	///
	/// The read side of [`Self::metadata_write_at`], for the metadata that is
	/// not a fixed-size structure: the cylinder-group bitmaps are `howmany()`
	/// -byte arrays whose length comes from the superblock rather than from a
	/// type, so there is nothing to decode them *as*.
	pub(super) fn metadata_read_at(&mut self, off: u64, len: usize) -> IoResult<Vec<u8>> {
		straddles(
			"metadata read",
			off % self.superblock.bsize(),
			len,
			self.superblock.bsize(),
		)?;
		let blk = off / self.superblock.bsize();
		let at = (off % self.superblock.bsize()) as usize;
		Ok(self.metadata_block(blk)?[at..at + len].to_vec())
	}

	/// The metadata buffer cache.
	///
	/// Test-only.  Deciding what gets persisted is `Ufs`'s job, and an accessor a
	/// FUSE callback could use to force a write would quietly undo the
	/// architecture that makes the callback unnecessary.
	#[cfg(test)]
	pub(super) fn metadata_cache(&self) -> &BufferCache {
		&self.buf
	}

	/// The buffer-cache block holding a cylinder group's struct and bitmaps.
	///
	/// This is the block whose write-back satisfies `note_bitmap_written` for
	/// every allocation out of that cylinder group, so getting it wrong would
	/// either stall every dependency or advance them early.
	pub(super) fn cg_blk(&self, cg: CgNum) -> u64 {
		self.metadata_blk(self.superblock.cg_struct(cg))
	}

	/// The Soft Updates dependency graph.
	///
	/// Test-only, for the same reason as [`Self::metadata_cache`]: ordering is
	/// decided here and nowhere else, so a caller that could advance the graph
	/// would be able to publish a range nothing has justified.
	#[cfg(test)]
	pub(super) fn dependencies(&self) -> &DependencyEngine {
		&self.softdep
	}

	/// Publish everything the dependency engine has released, then write the
	/// buffers that are safe to write.
	///
	/// This is the Soft Updates scheduler primitive, and it is the *only* way
	/// cached metadata reaches the device.  It is deliberately **not** a
	/// filesystem `sync()` yet, and it is not exposed through FUSE: the whole
	/// point of the design is that ordinary operations do not call it, because
	/// the dependency engine lets unrelated metadata be written whenever it is
	/// safe rather than forcing every mutation to be flushed synchronously.
	///
	/// The order is fixed and is the order the design requires:
	///
	/// 1. [`DependencyEngine::publish_into`] clears the byte ranges whose gates
	///    have opened.  Until this happens a buffer that was written safely
	///    keeps reading as "needs no write", because its safe image has already
	///    been persisted and only its gated ranges changed;
	/// 2. [`DependencyEngine::write_ready`] writes every buffer that has
	///    nothing left to wait for;
	/// 3. each completed write discharges whatever allocation events it made
	///    true, which can open further gates -- hence the loop.
	///
	/// A buffer with an unresolved gate is *left resident and dirty*: it is not
	/// written, and it is not discarded.  That is what "not safe yet" means.
	///
	/// # What this does not guarantee
	///
	/// Returning `Ok` does not mean the image is crash-consistent yet; it means
	/// everything that *is* safe has been written.  The properties
	/// [`crate::softdep::DependencyEngine::is_quiescent`] and
	/// [`crate::buf::BufferCache::is_clean`] together describe a fully drained
	/// filesystem, and reaching that from every operation is what the
	/// dependency wiring is for.
	/// Drain everything that can be drained, and report what could not be.
	///
	/// This is what shutdown should call.  It differs from
	/// [`Self::sync_metadata`] only in what it does with work that is *not*
	/// runnable -- a deferred free waiting on a container that only a later
	/// operation will write -- which it refuses rather than drops.
	///
	/// Refusing is the point.  The alternative is to unmount with blocks queued
	/// and nothing saying so, which is the one failure mode Soft Updates
	/// exists to prevent: the queue simply vanishes, and whatever those frees
	/// were protecting has already been forgotten.
	pub fn shutdown(&mut self) -> IoResult<MetadataStatus> {
		self.sync_metadata()?;
		let st = self.metadata_status();
		if !st.is_drained() {
			// Do not error, and do not pretend: the caller decides.  Returning
			// the status keeps that decision with whoever can act on it, rather
			// than turning a diagnostic into a second failure mode.
			log::error!("shutdown: metadata is still outstanding: {st:?}");
		}
		Ok(st)
	}

	/// What this filesystem has left to do, after a sync.
	///
	/// [`Self::sync_metadata`] drains everything it *can*.  That is not the same
	/// as being finished, and a caller that needs the second claim -- shutdown,
	/// or a test asserting quiescence -- has to ask here rather than infer it.
	///
	/// The distinction matters because the honest answer is often "not
	/// everything": a range gated on an event only a later operation can produce
	/// is not going out no matter how many further syncs run, and a `sync()` that
	/// reports success while metadata sits unpublished forever is worse than one
	/// that says so.
	pub fn metadata_status(&self) -> MetadataStatus {
		MetadataStatus {
			dirty:      self.buf.dirty_count() > 0,
			blocked:    self.blocked_inodes.len(),
			unresolved: self.softdep.unresolved(),
			pending:    self.deferred.len(),
			in_flight:  self.softdep.in_flight(),
		}
	}

	pub fn sync_metadata(&mut self) -> IoResult<()> {
		if self.buf.dirty_count() == 0 && self.softdep.is_empty() {
			return Ok(());
		}
		// A dirty buffer on a read-only mount would reach `BlockReader::write`,
		// which panics rather than returning `EROFS`.  Nothing can dirty a
		// buffer on a read-only mount, so reaching this is a bug, and saying so
		// is better than panicking inside the decoder adapter.
		self.assert_rw()?;

		// Drain: publishing and writing each open gates, so one pass is not
		// enough.  A pass writes the cylinder group, which discharges the
		// allocation, which publishes the inode's pointer range, which is only
		// written by the *next* pass.  Each pass either writes at least one
		// buffer or stops, and a written buffer is clean afterwards, so this
		// terminates -- the loop is bounded by the number of dirty buffers.
		let mut total = 0;
		loop {
			let n = self.sync_metadata_one_pass()?;
			if n == 0 {
				break;
			}
			total += n;
		}
		log::trace!("sync_metadata(): wrote {total} buffer(s)");
		Ok(())
	}

	/// Run exactly one pass of the drain, and report how many buffers it wrote.
	///
	/// This is what a *crash point* is: stop after the Nth pass, throw away
	/// everything still in memory, and look at what the disk says.  Testing only
	/// the fully-drained result would miss every intermediate state, and those
	/// are the ones the ordering rules exist to make safe.
	///
	/// Private because nothing outside the crash harness has a reason to stop
	/// half way: a caller that wanted persistence wants [`Self::sync_metadata`].
	pub(super) fn sync_metadata_one_pass(&mut self) -> IoResult<usize> {
		// Deferred work first: a free performed now dirties a cylinder group
		// that this same pass then writes, so draining before the write gets the
		// release out in one pass instead of two.
		let applied = self.drain_deferred()?;
		self.softdep.publish_into(&mut self.buf);
		let written = self.softdep.write_ready(&mut self.buf, &mut self.file)?;
		for (blk, how) in &written {
			self.note_block_written(*blk, *how)?;
		}
		// An applied free counts as progress even if it dirtied nothing new, so
		// the outer loop runs another pass and drains whatever it unblocked.
		Ok(written.len() + applied)
	}

	/// Turn a completed write-back into the allocation events it made true.
	///
	/// This is the only place the filesystem learns that bytes reached the
	/// device, and it is deliberately downstream of the write: `BufferCache`
	/// reports what it actually did, and a dependency may only be advanced by
	/// that report.  Anything that notified the engine from the *modification*
	/// side -- "the bitmap bit was cleared", "the block was filled" -- would be
	/// claiming the disk has the change when it does not, and every gate on it
	/// would open early.
	///
	/// The two events are not symmetric, and the difference is the whole point
	/// of keeping them apart:
	///
	/// * a cylinder group's bitmap is **never** gated, so *either* kind of write
	///   persists it -- the safe image is byte-for-byte the live image there,
	///   and a full write is a superset of a safe one;
	/// * a block's *contents* are the live image, so only a **full** write
	///   persisted them.  A safe write sent everything except the gated ranges,
	///   which is not the same thing at all.
	fn note_block_written(&mut self, blk: u64, how: Written) -> IoResult<()> {
		let full = how == Written::Full;

		// A write makes the buffer's contents what the disk holds again, whether
		// it was a safe or a full one: removing a pointer is an ordinary ungated
		// change, and a safe write persists it along with everything else that is
		// not gated.  The events that must *not* be told early -- a directory
		// removal, whose block may have other gated ranges behind it -- are the
		// ones that check `full` separately below.
		self.softdep.set_container_persisted(blk, true);

		// Block dependencies: the bitmap belongs to a cylinder group and is
		// never gated, so any write persists it; the contents are the live
		// image, so only a full write persisted them.
		let mut bitmap: Vec<DepId> = Vec::new();
		let mut contents: Vec<DepId> = Vec::new();
		for dep in self.softdep.new_blocks() {
			if self.cg_blk(dep.cg()) == blk {
				bitmap.push(dep.id());
			}
			if full && self.metadata_blk(dep.blk()) == blk {
				contents.push(dep.id());
			}
		}
		for id in bitmap {
			log::trace!("note_block_written({blk}): block bitmap for {id:?}");
			self.softdep.note_bitmap_written(id)?;
		}
		for id in contents {
			log::trace!("note_block_written({blk}): block contents for {id:?}");
			self.softdep.note_contents_written(id)?;
		}

		// Inode dependencies, which have the same shape: the bitmap bit belongs
		// to the cylinder group and is never gated, while the inode's image is
		// whatever the buffer held and may still have a gated pointer in it.
		//
		// Both inode events fire on *any* write, unlike the block contents
		// event.  `InodeWritten` means "this inode exists and is allocated",
		// and an inode whose image went out with a pointer still gated is
		// exactly that: it has a valid mode, size and link count, and its bitmap
		// bit is set.  What it does not yet have is the first block of its data,
		// and that costs an empty file rather than a corrupt one -- the benign
		// direction, and the one the gate is documented as accepting.
		//
		// Requiring `Written::Full` here instead would deadlock: a directory's
		// inode always has a gated pointer, so its block is only ever written
		// safely, and the entry naming it would never be publishable.
		let mut ino_bitmap: Vec<DepId> = Vec::new();
		let mut ino_image: Vec<DepId> = Vec::new();
		for dep in self.softdep.new_inodes() {
			if self.cg_blk(dep.cg()) == blk {
				ino_bitmap.push(dep.id());
			}
			if self.inode_blk(dep.inr()) == blk {
				ino_image.push(dep.id());
			}
		}
		for id in ino_bitmap {
			log::trace!("note_block_written({blk}): inode bitmap for {id:?}");
			self.softdep.note_inode_bitmap_written(id)?;
		}
		for id in ino_image {
			log::trace!("note_block_written({blk}): inode image for {id:?}");
			self.softdep.note_inode_written(id)?;
		}

		// A directory entry that is now on the disk may be what a parent's link
		// count was waiting for.
		self.release_inode_blocks();

		// A directory removal is persistent once its block has been written *at
		// all*, safely or in full.
		//
		// Requiring a full write was wrong, and it deadlocked the whole removal
		// chain: the block holding a removed entry is itself gated, because the
		// entry's inode number is exactly the range a `DirectoryRemove` gate
		// holds back.  So it is only ever written safely, the gate never
		// opened, and the cleared inode stayed unpublished for the life of the
		// mount.
		//
		// A safe write is enough: every gated range in that block is zeroed in the
		// safe image, and each of those ranges is precisely a removal (or a
		// pointer) that must not be followed.  Anything the block was *not* gated
		// for went out too.
		let n = self.softdep.note_directory_block_written(blk);
		if n > 0 {
			log::trace!("note_block_written({blk}): {n} removal(s) persisted");
		}
		Ok(())
	}

	/// The cache block holding `inr`'s inode.
	fn inode_blk(&self, inr: InodeNum) -> u64 {
		self.metadata_blk(self.superblock.ino_to_fsba(inr))
	}

	/// The gate that opens when the last directory entry naming `inr` is gone
	/// from the disk, if one has been removed since the last flush.
	///
	/// `None` for an inode that was never linked, or whose removal is already
	/// persistent -- in which case there is nothing to wait for and the cleared
	/// image may go out as ordinary dirty metadata.
	pub(super) fn removal_gate(&mut self, inr: InodeNum) -> Option<crate::softdep::Gate> {
		self.softdep.removal_gate(inr)
	}

	/// Perform every deferred operation whose prerequisite has been persisted.
	///
	/// The drain point.  It is reached from the same place a crash harness's
	/// one-pass variant reaches it, so a crash test and a real drain take the
	/// same path -- which is the only way the crash tests mean anything.
	///
	/// Operations come out in [`crate::OpKey`] order and are performed in that
	/// order.  Performing one dirties a cylinder group, which is ordinary
	/// writeback work rather than more deferred work -- a free never *creates* a
	/// deferred operation -- so the queue strictly shrinks and the loop
	/// terminates.
	pub(super) fn drain_deferred(&mut self) -> IoResult<usize> {
		let persisted: Vec<u64> = self
			.deferred_pending_containers()
			.into_iter()
			.filter(|c| self.softdep.container_is_persisted(*c))
			.collect();
		// Cheap enough to always run and worth it in a debug build: an invariant
		// violation here would otherwise show up much later as metadata that is
		// silently never published.
		debug_assert!(
			self.softdep.validate().is_empty(),
			"soft updates dependency graph is inconsistent: {:?}",
			self.softdep.validate()
		);

		let ops = self.deferred.take_runnable(|c| persisted.contains(&c));
		for op in &ops {
			log::trace!("drain_deferred: applying {}", op.label());
		}
		for op in &ops {
			self.apply_deferred(op)?;
		}
		Ok(ops.len())
	}

	/// Every container the deferred queue is waiting on, deduplicated.
	fn deferred_pending_containers(&self) -> Vec<u64> {
		self.deferred
			.iter()
			.map(|o| o.container())
			.collect::<std::collections::BTreeSet<_>>()
			.into_iter()
			.collect()
	}

	/// Perform one operation.
	///
	/// Split out so that a drain and a retry share one definition of what an
	/// operation *does*.
	fn apply_deferred(&mut self, op: &crate::softdep::DeferredOp) -> IoResult<()> {
		match op {
			crate::softdep::DeferredOp::FreeBlock { blk, size, .. } => {
				let cg = self.superblock.blk_to_cg(*blk);
				self.blk_free_now(*blk, *size, cg)?;
			}
			crate::softdep::DeferredOp::FreeInode {
				inr, gen, was_dir, ..
			} => {
				self.free_cg_inode_now(*inr, *gen, *was_dir)?;
			}
		}
		Ok(())
	}

	/// Hold `parent`'s inode buffer back until the directory entry that
	/// justifies a link count is on the disk.
	///
	/// This is `MkdirParentDep`.  The *whole buffer* is held back rather than
	/// the eight bytes of `i_nlink`, because `Buffer::safe_image()` holds a
	/// range back by zeroing it, and a zeroed `nlink` is a different and wrong
	/// number rather than a stale one.  Not writing the buffer at all is sound
	/// for any content: the device keeps what was already there, which was
	/// consistent.
	pub(super) fn block_inode_on_dir(&mut self, parent: InodeNum, inode_blk: u64, dir_blk: u64) {
		if self
			.softdep
			.gate_is_open(crate::softdep::Gate::DirectoryPersisted {
				parent,
				blk: dir_blk,
			}) {
			return;
		}
		self.buf.block(inode_blk, dir_blk);
		self.blocked_inodes.push((inode_blk, dir_blk, parent));
	}

	/// Release every held-back inode buffer whose directory entry is now on the
	/// disk.
	fn release_inode_blocks(&mut self) {
		self.blocked_inodes.retain(|&(inode_blk, dir_blk, parent)| {
			if self
				.softdep
				.gate_is_open(crate::softdep::Gate::DirectoryPersisted {
					parent,
					blk: dir_blk,
				}) {
				self.buf.release(inode_blk, dir_blk);
				false
			} else {
				true
			}
		});
	}

	/// Record that the directory entry at `block_off` in `dinr` has been
	/// removed from the live image.
	///
	/// The entry is already gone as far as the running filesystem is concerned.
	/// What this records is that the *disk* does not know yet, which is what the
	/// inode's reclamation has to wait for.
	pub(super) fn note_dirent_removed(
		&mut self,
		dinr: InodeNum,
		pos: u64,
		block_off: u64,
		inr: InodeNum,
	) -> IoResult<()> {
		let Some(blk) = self.dirent_block(dinr, pos + block_off)? else {
			return Ok(());
		};
		self.softdep.directory_removed(dinr, blk, block_off, inr);
		log::trace!("directory entry for {inr} removed at {dinr}'s block {blk}:{block_off}");
		Ok(())
	}
}

#[cfg(test)]
mod t {
	use std::{ffi::OsStr, io::ErrorKind};

	use crate::{
		data::{CylGroup, InodeData},
		geom::CgNum,
		testutil,
		InodeNum,
		InodeType,
		Ufs,
	};

	/// A metadata read sees the on-disk structure when nothing has touched it.
	#[test]
	fn reads_see_the_on_disk_structure() {
		let (_img, mut ug) = testutil::open_ro("ufs-little");
		let cg = CgNum::new(2);
		let off = ug.cg_addr(cg);
		let want = ug.file.decode_at::<CylGroup>(off).unwrap();
		let got = ug.metadata_read::<CylGroup>(off).unwrap();
		assert_eq!(got.cs, want.cs);
	}

	/// Create an empty regular file in the root directory.
	fn mknod_file(ug: &mut Ufs<std::fs::File>, name: &str) -> InodeNum {
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

	/// The `i`-th direct block pointer of `inr`, or 0.
	fn direct(ug: &mut Ufs<std::fs::File>, inr: InodeNum, i: usize) -> u64 {
		let ino = ug.read_inode(inr).unwrap();
		let InodeData::Blocks(b) = &ino.data else {
			panic!("inode {inr} has no block map");
		};
		b.direct[i] as u64
	}

	/// Mounting reads the cylinder-group summaries through the cache, but
	/// reads alone must not dirty anything: a filesystem that has only been
	/// mounted has nothing to persist.
	#[test]
	fn mounting_does_not_dirty_the_cache() {
		let (_img, mut ug) = testutil::open_ro("ufs-little");
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
		assert!(ug.metadata_cache().is_clean());
		// One cylinder-group block per cylinder group, and nothing else.
		assert_eq!(
			ug.metadata_cache().stats().resident,
			ug.superblock.ncg as usize
		);
		// A read-only mount must never need to write.
		ug.sync_metadata().unwrap();
	}

	/// A metadata write dirties the buffer and does *not* reach the device.
	/// This is the behavioural change the whole series rests on, so it is
	/// asserted directly rather than only implied by the later commits.
	#[test]
	fn metadata_write_dirties_without_persisting() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let cg = CgNum::new(1);
		let off = ug.cg_addr(cg);
		let mut cgd = ug.metadata_read::<CylGroup>(off).unwrap();
		let before = cgd.cs.nbfree;
		cgd.cs.nbfree -= 1;
		ug.metadata_write(off, &cgd).unwrap();

		// Live: the new value is what the allocator will see.
		assert_eq!(
			ug.metadata_read::<CylGroup>(off).unwrap().cs.nbfree,
			before - 1
		);
		assert!(ug.metadata_cache().is_resident(off / ug.superblock.bsize()));

		// Persistent: untouched, because nothing has flushed.
		drop(ug);
		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(ug.metadata_read::<CylGroup>(off).unwrap().cs.nbfree, before);
	}

	/// A metadata write touches only the bytes of the structure it staged: the
	/// neighbouring half of the block is unchanged.  For a cylinder-group block
	/// the neighbour is the block bitmap, which is the one structure a
	/// mis-sized write would silently destroy.
	#[test]
	fn metadata_write_leaves_neighbours_alone() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let cg = CgNum::new(1);
		let off = ug.cg_addr(cg);
		let bsize = ug.superblock.bsize();
		let blk = off / bsize;
		let at = off % bsize;

		let before = ug.metadata_block(blk).unwrap().to_vec();
		let mut cgd = ug.metadata_read::<CylGroup>(off).unwrap();
		cgd.cs.ndir += 1;
		ug.metadata_write(off, &cgd).unwrap();
		let after = ug.metadata_block(blk).unwrap().to_vec();
		let staged = ug.file.encode_to_vec(&cgd).unwrap();
		let start = at as usize;
		let end = start + staged.len();

		assert_eq!(
			&after[start..end],
			&staged[..],
			"the structure's own bytes must be replaced"
		);
		assert_eq!(
			&after[..start],
			&before[..start],
			"the structures before this one must be untouched"
		);
		assert_eq!(
			&after[end..],
			&before[end..],
			"the block bitmap that follows must be untouched"
		);
	}

	/// A structure that would run off the end of its block is rejected rather
	/// than allowed to overwrite whatever follows it.
	#[test]
	fn a_straddling_write_is_refused() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let bsize = ug.superblock.bsize() as usize;
		let err = ug
			.metadata_write_range(0, (bsize - 4) as u64, &[0u8; 8])
			.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
	}

	/// The cylinder-group struct and its two bitmaps share one `fs_bsize` block,
	/// so they must all be staged through the cache.  If the bitmaps were still
	/// written straight to the device while the struct was staged, the next
	/// write-back of that block would roll the bitmaps back to the copy taken
	/// when it was fetched — losing an allocation while keeping its counter.
	#[test]
	fn cylinder_group_bitmaps_share_the_struct_block() {
		let (_img, mut ug) = testutil::open_rw("ufs-little");
		let cg = CgNum::new(1);
		// Fetch the block, then change all three structures in it.
		let _ = ug.read_cg(cg).unwrap();
		let mut cgd = ug.read_cg(cg).unwrap();
		let mut map = ug.read_blkmap(cg, &cgd).unwrap();
		map.set_free_block(0, false);
		cgd.cs.nbfree -= 1;

		// Stage all three, then flush the one block that contains them.
		ug.write_blkmap(cg, &cgd, &map).unwrap();
		ug.write_cg(cg, &cgd).unwrap();
		ug.sync_metadata().unwrap();

		assert_eq!(ug.metadata_cache().dirty_count(), 0);

		// Everything must have gone out together.
		let cgd2 = ug.read_cg(cg).unwrap();
		assert_eq!(cgd2.cs.nbfree, cgd.cs.nbfree);
		let map2 = ug.read_blkmap(cg, &cgd2).unwrap();
		assert_eq!(map2.as_bytes(), map.as_bytes());
	}

	/// `sync_metadata` persists what is safe and nothing else, and leaves the
	/// cache with nothing left to write.
	#[test]
	fn sync_metadata_persists_and_drains() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let cg = CgNum::new(2);
		let before = ug.read_cg(cg).unwrap().cs.nbfree;

		let mut cgd = ug.read_cg(cg).unwrap();
		cgd.cs.nbfree += 1;
		ug.write_cg(cg, &cgd).unwrap();
		assert_eq!(ug.read_cg(cg).unwrap().cs.nbfree, before + 1, "live");

		ug.sync_metadata().unwrap();
		assert_eq!(ug.metadata_cache().dirty_count(), 0);
		assert!(ug.metadata_cache().is_clean());
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_eq!(ug.read_cg(cg).unwrap().cs.nbfree, before + 1, "persistent");
	}

	/// An ordinary operation's metadata write is live immediately and persistent
	/// only after a flush.  The mapping layer, `SEEK_DATA` and `FIEMAP` all
	/// depend on this, so it is asserted directly: a caller asking where a file
	/// currently maps must see the filesystem's state, not the last image that
	/// reached the disk.
	#[test]
	fn an_inode_write_is_live_before_it_is_persistent() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = mknod_file(&mut ug, "zzz-live");
		ug.inode_write(inr, 0, &[0x5au8; 4 * 32768]).unwrap();

		// Live: the block map is there, and it is a dirty buffer.
		let live = direct(&mut ug, inr, 3);
		assert_ne!(live, 0, "the write did not update the block map");
		assert!(ug.metadata_cache().dirty_count() > 0);

		// Not yet persistent: on disk the inode is still the empty one
		// `mknod` left behind, so it cannot even be read as an inode.
		drop(ug);
		let mut ug = Ufs::open(img.path(), true).unwrap();
		let off = ug.superblock.ino_to_fso(inr);
		let raw: crate::data::Inode = ug.metadata_read(off).unwrap();
		assert_eq!(
			raw.mode & crate::data::S_IFMT,
			0,
			"the inode reached the disk before it was flushed"
		);
	}

	/// The other half of the same property: a flush does make it survive, and
	/// the image is consistent afterwards.
	#[test]
	fn flushing_makes_the_inode_visible_after_a_reopen() {
		let (img, mut ug) = testutil::open_rw("ufs-little");
		let inr = mknod_file(&mut ug, "zzz-flush");
		ug.inode_write(inr, 0, &[0x5au8; 4 * 32768]).unwrap();
		ug.sync_metadata().unwrap();
		drop(ug);

		let mut ug = Ufs::open(img.path(), true).unwrap();
		assert_ne!(
			direct(&mut ug, inr, 3),
			0,
			"the flushed inode did not survive"
		);
		assert!(ug.check_consistency().unwrap().is_clean());
	}

	/// A read-only mount has nothing to write, so draining it is a no-op rather
	/// than `EROFS` or a panic inside the decoder adapter.
	#[test]
	fn sync_metadata_on_a_read_only_mount_is_a_no_op() {
		let (_img, mut ug) = testutil::open_ro("ufs-little");
		ug.sync_metadata().unwrap();
		ug.inode_read(InodeNum::ROOT, 0, &mut [0u8; 512]).unwrap();
		ug.sync_metadata().unwrap();
	}
}

/// What a filesystem has left to do after a sync.
///
/// Every field is zero in a filesystem that has nothing outstanding, which is
/// the state a caller may assume the disk reflects in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataStatus {
	/// Buffers with unsaved changes.
	pub dirty: bool,

	/// Inode buffers held back by a whole-buffer dependency.
	pub blocked: usize,

	/// Dependencies created and not yet resolved.
	///
	/// Non-zero after a full drain means something waits on an event only a later
	/// operation can produce: normal mid-operation, abnormal at shutdown.
	pub unresolved: usize,

	/// Deferred operations waiting for a container to be written.
	pub pending: usize,

	/// Allocations still short of publishable.
	pub in_flight: usize,
}

impl MetadataStatus {
	/// Whether nothing at all is outstanding.
	///
	/// This is the claim `sync()` cannot make on its own and a caller needs
	/// before it may assume the image on the disk is the whole story.
	pub fn is_drained(&self) -> bool {
		!self.dirty &&
			self.blocked == 0 &&
			self.unresolved == 0 &&
			self.pending == 0 &&
			self.in_flight == 0
	}
}
