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

//! The dependency engine: what may be written, and in which order.
//!
//! # What this is for
//!
//! Soft Updates is not a feature, it is a *scheduler*.  Given the same set of
//! pending metadata changes, many different on-disk orders are crash-consistent
//! and many are not, and the difference between them is invisible until the
//! machine loses power at the wrong moment.  The engine's only job is to turn
//! "this pointer may not be trusted yet" into a statement about a byte range of
//! a buffer, so that [`crate::buf::BufferCache`] can write the safe image
//! instead of the live one.
//!
//! # The shape of it
//!
//! A [`Dependency`] is a *gate*: a byte range of a buffer that may not reach
//! the disk until some event happens.  The engine holds
//!
//! * the gates, indexed by [`DepId`];
//! * the progress of each [`AllocationState`] machine;
//! * a reverse index from a gated range to its gate, so that completing an
//!   allocation can walk the gates that were waiting on it.
//!
//! When an allocation advances, every gate that was waiting on it is
//! re-evaluated; gates that no longer have an unmet precondition are *published*,
//! which is a call to [`crate::buf::Buffer::publish`] and nothing more.  The
//! buffer cache does the rest.
//!
//! FreeBSD solves the same problem with `struct work`, `struct allocwork`,
//! `struct indirdep` and the `DAG`/`DEPS`/`ALLDONE` bit-flag soup in
//! `sys/ufs/ffs/softdep.h`, plus a `SEBUF` "safe" buffer per pending operation.
//! Those structures are a hand-rolled type system in C.  The Rust equivalent is
//! ordinary ownership: a gate owns the byte range it protects, and progress is
//! an `AllocationState` value rather than a bit that several subsystems clear in
//! different orders.
//!
//! # What is implemented
//!
//! [`NewBlockDep`] (a block's allocation bitmap and its contents), and the two
//! pointer gates that build directly on it: [`DirectPointerDep`] and
//! [`IndirectPointerDep`].  The remaining categories the design calls for —
//! `InodeUpdate`, `DirectoryAdd`, `DirectoryRemove`, `Mkdir`, `FreeBlocks`,
//! `FreeInode` — are described in `docs/soft-updates.md` together with the
//! ordering each one enforces; they are *not* implemented here, and
//! `docs/soft-updates.md` records that explicitly.

use std::collections::{BTreeMap, BTreeSet};

type IoResult<T> = std::io::Result<T>;

use crate::{
	buf::{BufferCache, Written},
	data::InodeNum,
	geom::CgNum,
};

fn invalid_dep(id: DepId) -> std::io::Error {
	std::io::Error::other(format!("no such dependency: {:?}", id.0))
}

/// An identifier for a dependency.  Stable for the lifetime of the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DepId(pub u64);

/// The lifecycle of a newly allocated filesystem resource.
///
/// ```text
///  New ──bitmap written──> BitmapWritten ──contents written──> ContentsWritten
///   │                                │                                │
///   └────────────────────────────────┴────────────────────────────────┤
///                                                                        v
///                                                                  SafeToPublish
///                                                                        │
///                                                       pointer published │
///                                                                        v
///                                                                   Published
///                                                                        │
///                                                                       done
///                                                                        v
///                                                                   Complete
/// ```
///
/// The states are separate rather than one "done" bit because the *order* of
/// the first two transitions does not matter, while both of them must precede
/// the third.  Modelling that as two booleans ([`NewBlockDep`] has exactly that)
/// is why out-of-order progress is safe and a single enum would not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AllocationState {
	/// Reserved: the bitmap has not been updated yet.
	New,
	/// The cylinder-group bitmap says the block is in use.
	BitmapWritten,
	/// The block's contents are on the device.
	ContentsWritten,
	/// Both of the above hold; a pointer to this block may now be persisted.
	SafeToPublish,
	/// Some pointer to this block has reached the disk.
	Published,
	/// Nothing is waiting on this allocation any more.
	Complete,
}

impl AllocationState {
	/// Whether a pointer to the resource may become persistent in this state.
	pub fn allows_publication(self) -> bool {
		self >= AllocationState::SafeToPublish
	}

	/// Whether this allocation is finished with.
	pub fn is_complete(self) -> bool {
		self == AllocationState::Complete
	}
}

/// What a gate is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gate {
	/// The block's allocation bitmap has reached the device.
	AllocationAllocated(DepId),
	/// The block's initialised contents have reached the device.
	AllocationInitialised(DepId),
	/// The block is fully safe: bitmap *and* contents.
	AllocationSafe(DepId),
	/// The named inode has reached the device.
	InodeWritten(InodeNum),
	/// The named inode has reached the device with its link count decremented.
	InodeLinkCounted(InodeNum),
	/// A directory block in the named parent has reached the device.
	///
	/// This one resolves on its own, unlike its neighbours: the block is named
	/// explicitly, so a write of *that* block is the event, with no dependency to
	/// look up.  It is what `MkdirParentDep` waits on -- a parent's link count
	/// must not be published before the entry that justifies it.
	DirectoryPersisted { parent: InodeNum, blk: u64 },

	/// The directory entry at that byte offset has been removed *persistently*.
	///
	/// "Persistently" is the whole word.  The entry's inode number is zeroed in
	/// the live image the moment `dir_unlink()` runs, but the directory block
	/// may not have reached the disk yet, and until it does the directory still
	/// lists a file that is in the middle of being reclaimed.  The reclaim must
	/// wait.
	///
	/// Resolved only when the block is written *fully*, because a safe
	/// write-back leaves that block's other gated ranges behind and cannot be
	/// said to have removed this entry.
	DirectoryRemoved {
		parent: InodeNum,
		blk:    u64,
		off:    u64,
	},
	/// The named inode's cleared image is on the disk.
	///
	/// The last transition of a removal:
	///
	/// ```text
	///   directory entry removed   (Gate::DirectoryRemoved)
	///        -> inode cleared     (gated on the above)
	///        -> bitmap bit freed   (this gate)
	/// ```
	///
	/// The bitmap bit is the *last* thing released because it is the one whose
	/// early release is destructive: an inode whose bit says free while its
	/// image still reads as live is one that `fsck` pass 1 reports as
	/// allocated-but-unused and pass 4 then frees a second time.
	InodeReclaimed(InodeNum),

	/// Every pointer that used to reach a now-freed block has been removed from
	/// the structures in `container`, and those structures have reached the
	/// disk.
	///
	/// The block side of the ordering.  A block's allocation bit may not be
	/// cleared while a pointer to it can still be found on the disk, because the
	/// next allocation would hand the block to somebody else with the old
	/// pointer still in place.
	///
	/// `container` is a cache block: the inode, for a direct block or for a
	/// first-level indirect block, and the enclosing indirect block for
	/// anything deeper.
	PointersRemoved { container: u64 },
}

/// A gated byte range: "these bytes of that block may not be written until the
/// gate opens".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
	/// Which dependency this is.
	pub id:       DepId,
	/// What kind of relationship it protects; see the module documentation.
	pub kind:     DepKind,
	/// The buffer whose bytes are gated.
	pub blk:      u64,
	/// Byte offset within the buffer.
	pub off:      u64,
	/// Length of the gated range.
	pub len:      u64,
	/// What must happen first.
	pub gate:     Gate,
	/// Set once the gate has opened and the range has been published.
	pub resolved: bool,
}

/// The category of a dependency, which is what the tests and the documentation
/// are indexed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
	/// A pointer from an inode's direct array to a newly allocated block.
	DirectPointer,
	/// A pointer from an indirect block to a newly allocated block, and the
	/// pointer from the inode to the indirect block itself.
	IndirectPointer,
	/// A directory entry naming a newly initialised inode.
	DirectoryAdd,
	/// An inode that is being reclaimed, held back until the directory entry
	/// that named it is gone from the disk.
	DirectoryRemove,
	/// A cylinder group's free-block accounting, held back until the structures
	/// that pointed at the freed blocks are on the disk.
	FreeBlocks,
	/// An inode's bitmap entry, held back until the inode's cleared image is on
	/// the disk.
	InodeReclaim,
}

impl DepKind {
	/// The operation that creates this kind of dependency.
	///
	/// This is the first of the five questions the design asks of every
	/// dependency; the other four are in the rustdoc of each variant.
	pub fn created_by(self) -> &'static str {
		match self {
			Self::DirectPointer => "inode_alloc_block(): a file or directory grows a new block",
			Self::IndirectPointer => {
				"inode_set_block(): the pointer table itself is created or grows"
			}
			Self::DirectoryAdd => "dir_newlink() after inode_alloc()",
			Self::DirectoryRemove => {
				"inode_free(): the last link has gone and the inode is being cleared"
			}
			Self::FreeBlocks => {
				"blk_free(): a pointer to this block has just been removed from an inode \
				 or an indirect block"
			}
			Self::InodeReclaim => {
				"free_cg_inode(): this inode's last directory entry has just been removed"
			}
		}
	}

	/// The disk ordering rule this kind enforces.
	pub fn enforces(self) -> &'static str {
		match self {
			Self::DirectPointer => {
				"the inode's pointer must not reach the disk before the block's bitmap bit \
				 and its contents"
			}
			Self::IndirectPointer => {
				"an indirect block's entry must not name a block that is not safe, and an \
				 indirect block must not be published before every entry it gained is safe"
			}
			Self::DirectoryAdd => {
				"a directory entry must not reach the disk before the inode it names is on disk"
			}
			Self::DirectoryRemove => {
				"a cleared inode must not reach the disk before the directory entry that \
				 named it is gone"
			}
			Self::FreeBlocks => {
				"a block is returned to the free list while the disk still holds a pointer \
				 to it; the next allocation hands it to somebody else and the old file \
				 keeps reading somebody else's data"
			}
			Self::InodeReclaim => {
				"an inode's bitmap bit must not reach the disk before its cleared image, \
				 and the cleared image must not reach it before the directory entry that \
				 named it is gone"
			}
		}
	}

	/// The crash inconsistency without this dependency.
	pub fn prevents(self) -> &'static str {
		match self {
			Self::DirectPointer => {
				"an inode permanently references a block the cylinder-group bitmap still \
				 reports as free; fsck_ffs pass 1 reports a block that is owned but \
				 unallocated, and pass 5 clears the pointer, losing the file"
			}
			Self::IndirectPointer => {
				"a pointer chain reaches a block that was never initialised, so a file reads \
				 back as containing garbage offsets; fsck_ffs pass 1 reports an indirect \
				 block entry that is out of range"
			}
			Self::DirectoryAdd => {
				"a directory entry names an inode whose bitmap bit is clear and whose contents \
				 are zero; fsck_ffs pass 2 clears the entry, and the file's data is lost even \
				 though it is on the disk"
			}
			Self::DirectoryRemove => {
				"a directory entry names an inode whose fields are zero and whose bitmap bit is \
				 still set; fsck_ffs pass 2 clears the entry, and pass 4 then frees the inode's \
				 blocks -- a directory entry to a file whose data is still on the disk"
			}
			Self::FreeBlocks => {
				"a block is returned to the free list while the disk still holds a \
				 pointer to it; the next allocation hands it to somebody else and the \
				 old file keeps reading somebody else's data"
			}
			Self::InodeReclaim => {
				"an inode's bitmap bit says free while its image still reads as live; \
				 fsck_ffs pass 1 reports it as allocated but unused and pass 4 then \
				 frees its blocks a second time"
			}
		}
	}

	/// What resolves this kind of dependency.
	pub fn resolved_by(self) -> &'static str {
		match self {
			Self::DirectPointer => "the NewBlockDep for the block reaching SafeToPublish",
			Self::IndirectPointer => {
				"the NewBlockDep for the target block, and then the inode's write"
			}
			Self::DirectoryAdd => "the inode's write reaching the device",
			Self::DirectoryRemove => "the directory block being written in full",
			Self::FreeBlocks => "the inode or indirect block the pointer was removed from",
			Self::InodeReclaim => "the cleared inode image, and the directory entry before it",
		}
	}

	/// Which `fsck_ffs` invariant this protects.
	pub fn protects(self) -> &'static str {
		match self {
			Self::DirectPointer => "invariants 2, 3 and 4 (docs/ufs2-invariants.md)",
			Self::IndirectPointer => "invariants 3 and 7",
			Self::DirectoryAdd => "invariants 5 and 6",
			Self::DirectoryRemove => "invariants 1 and 6",
			Self::FreeBlocks => "invariants 1, 2 and 3",
			Self::InodeReclaim => "invariants 1 and 7",
		}
	}
}

/// A newly allocated block whose bitmap and contents must both reach the disk
/// before anything may point at it.
///
/// # The crash this prevents
///
/// `create()` allocates a block, writes the file's first bytes into it, and
/// stores its address in the inode.  If the inode reaches the disk before the
/// cylinder-group bitmap does, a crash in between leaves a filesystem where the
/// inode says "lbn 0 is block 617" while `cg_blksfree[]` says block 617 is free.
/// On the next boot `fsck_ffs` pass 5 rebuilds the bitmap from the inodes it can
/// see, does *not* find block 617 owned, and frees it — the file's first block
/// is silently returned to the free list while the inode still points at it.  The
/// next allocation hands the same block to somebody else.
///
/// If the bitmap reaches the disk before the block's *contents*, a crash leaves
/// an allocated block full of whatever the previous tenant left there, which a
/// file that has not been written past its length will happily expose.
///
/// # Ordering
///
/// ```text
/// ```text
///   allocate block
///        |
///        +--> allocation bitmap --┐
///        +--> block contents -----┴--> SafeToPublish --> Published --> Complete
/// ```
///
/// The two first steps may happen in either order.  Nothing may be published
/// until both have happened.
///
/// # Resolved by
///
/// [`DependencyEngine::note_bitmap_written`] and
/// [`DependencyEngine::note_contents_written`]; the state advances on its own
/// once both have been seen.
///
/// # Protects
///
/// Invariants 2, 3 and 4 in `docs/ufs2-invariants.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlockDep {
	id:       DepId,
	blk:      u64,
	cg:       CgNum,
	bitmap:   bool,
	content:  bool,
	publish:  bool,
	complete: bool,
}

impl NewBlockDep {
	/// This dependency's identifier.
	pub fn id(&self) -> DepId {
		self.id
	}

	/// The block this dependency is about.
	pub fn blk(&self) -> u64 {
		self.blk
	}

	/// The cylinder group whose bitmap must be updated.
	pub fn cg(&self) -> CgNum {
		self.cg
	}

	/// The current lifecycle state.
	pub fn state(&self) -> AllocationState {
		match (self.bitmap, self.content) {
			(false, false) => AllocationState::New,
			(true, false) => AllocationState::BitmapWritten,
			(false, true) => AllocationState::BitmapWritten,
			(true, true) => {
				if self.publish {
					if self.complete {
						AllocationState::Complete
					} else {
						AllocationState::Published
					}
				} else {
					AllocationState::ContentsWritten
				}
			}
		}
	}

	/// Whether a pointer to this block may become persistent.
	pub fn allows_publication(&self) -> bool {
		self.bitmap && self.content
	}

	/// Mark the dependency finished: nothing waits on it any more.
	fn finish(&mut self) {
		self.complete = true;
	}
}

/// A newly allocated inode whose bitmap bit and image must both reach the disk
/// before anything may name it.
///
/// # The crash this prevents
///
/// `mknod()` allocates an inode and then writes a directory entry naming it.
///  If the entry reaches the disk first, a crash leaves a directory that lists
///  a file whose inode bitmap bit is still clear.  `fsck_ffs` pass 2 sees the
///  entry, resolves the inode number, finds an inode that is not allocated and
///  whose fields are zero, and *clears the entry*.  The file's data was never
///  lost -- it was never reachable again.
///
/// If the bitmap reaches the disk first, a crash leaves an allocated inode that
///  nothing points at.  That is the benign direction: `fsck` pass 2 reclaims
///  it.
///
/// # Why this is not just `NewBlockDep`
///
/// Because an inode has two halves on the disk, like a block, and only their
/// conjunction is enough: the image, and the bitmap bit that says it is
/// allocated.  The bitmap lives in the cylinder group's block, which is a
/// *different* buffer from the one holding the inode, so the two events are
/// genuinely independent -- and either order is legal, exactly as for blocks.
///
/// # Ordering
///
/// ```text
///   allocate inode
///        +-- inode bitmap bit ---+
///        +-- inode image ---------+--> safe to name
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeDep {
	id:     DepId,
	inr:    InodeNum,
	cg:     CgNum,
	image:  bool,
	bitmap: bool,
}

impl InodeDep {
	/// This dependency's identifier.
	pub fn id(&self) -> DepId {
		self.id
	}

	/// The inode this dependency is about.
	pub fn inr(&self) -> InodeNum {
		self.inr
	}

	/// The cylinder group whose inode bitmap must be updated.
	pub fn cg(&self) -> CgNum {
		self.cg
	}

	/// Whether something may name this inode yet.
	pub fn allows_naming(&self) -> bool {
		self.image && self.bitmap
	}
}

/// A directory entry that has been removed from the live image and is waiting to
/// reach the disk.
///
/// The counterpart of [`NewBlockDep`] on the removal side: `dir_unlink()` zeroes
/// the entry's inode number immediately, so the live directory no longer lists
/// the file while the disk may still do.  Anything that reclaims the inode has
/// to wait until the disk agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryRemoveDep {
	blk:  u64,
	off:  u64,
	gone: bool,
}

impl DirectoryRemoveDep {
	/// Whether the directory entry is known to be gone from the disk.
	pub fn is_persisted(&self) -> bool {
		self.gone
	}
}

impl DependencyEngine {
	/// Record a directory entry as removed from the live image.
	///
	/// Created by `Ufs::dir_unlink()`, once it has zeroed the entry's inode
	/// number.  Protects invariants 1 and 6 in `docs/ufs2-invariants.md`.
	pub fn directory_removed(&mut self, parent: InodeNum, blk: u64, off: u64, inr: InodeNum) {
		let gate = Gate::DirectoryRemoved { parent, blk, off };
		self.removals.insert(
			gate,
			DirectoryRemoveDep {
				blk,
				off,
				gone: false,
			},
		);
		self.by_removed_inode.insert(inr, gate);
	}

	/// Record that the directory block `blk` has been written *in full*, which
	/// is what makes every removal in it persistent.
	///
	/// Keyed by block rather than by parent because the flush loop knows which
	/// block it wrote and not which directory owns it: the parent is part of the
	/// gate, so two directories can never be confused for one another, but it is
	/// not what the caller has.  Removals outstanding between two flushes are
	/// few, so the scan is not worth an index.
	pub fn note_directory_block_written(&mut self, blk: u64) -> usize {
		let mut gone = Vec::new();
		for (gate, dep) in &mut self.removals {
			if dep.blk == blk && !dep.gone {
				dep.gone = true;
				gone.push(*gate);
			}
		}
		let n = gone.len();
		for gate in gone {
			self.settle_removal(&gate);
		}
		n
	}

	/// The gate that opens when the most recent removal of `inr` is persistent.
	///
	/// Keyed by inode rather than by block, because the reclaiming path has an
	/// inode and not a directory.  The *newest* removal is the one that matters:
	/// a file renamed within one operation has had more than one entry removed,
	/// and only the last one is still live.
	pub fn removal_gate(&self, inr: InodeNum) -> Option<Gate> {
		self.by_removed_inode
			.get(&inr)
			.and_then(|gate| self.removals.get(gate).map(|_| *gate))
	}

	/// Record whether `blk`'s cached contents currently match the disk.
	///
	/// This is *not* "has been written at some point": a buffer that was
	/// written and then modified again is back to not matching, and a pointer
	/// removed after that write is not persistent.  `Ufs` keeps it honest from
	/// the two places it can go stale -- a fetch, which brings in what the disk
	/// holds, and a mutable borrow, which is the start of a change.
	pub fn set_container_persisted(&mut self, blk: u64, persisted: bool) {
		if persisted {
			self.written.insert(blk);
		} else {
			self.written.remove(&blk);
		}
		if persisted {
			self.settle_removal(&Gate::PointersRemoved { container: blk });
			// An inode whose cleared image has just reached the disk may have its
			// bitmap bit released, if the removal it waited on also happened.
			for inr in self.reclaimants_of(blk) {
				let gate = Gate::InodeReclaimed(inr);
				if self.gate_is_open(gate) {
					self.settle_removal(&gate);
				}
			}
		}
	}

	/// Declare that `inr` is being reclaimed.
	///
	/// `removal` is the gate the directory entry's removal waits on, and
	/// `inode_blk` the cache block its cleared image lives in.  `None` for an
	/// inode that was never linked, which has nothing behind it.
	pub fn reclaim_inode(&mut self, inr: InodeNum, removal: Option<Gate>, inode_blk: u64) {
		let Some(removal) = removal else {
			return;
		};
		self.reclaiming.insert(inr, (removal, inode_blk));
	}

	fn reclaimants_of(&self, inode_blk: u64) -> Vec<InodeNum> {
		self.reclaiming
			.iter()
			.filter(|(_, (_, blk))| *blk == inode_blk)
			.map(|(inr, _)| *inr)
			.collect()
	}

	/// Whether `container`'s current contents are on the disk.
	///
	/// "Currently" is the operative word.  A container written earlier and
	/// modified since is *not* persisted, which is what lets a deferred free
	/// correctly conclude that a pointer removal is still outstanding.
	pub fn container_is_persisted(&self, container: u64) -> bool {
		self.written.contains(&container)
	}

	fn settle_removal(&mut self, gate: &Gate) {
		if !self.gate_is_open(*gate) {
			return;
		}
		if let Some(ids) = self.waiting_on.remove(gate) {
			for id in ids {
				if self.deps.get(&id).is_some_and(|d| d.resolved) {
					continue;
				}
				self.publish_dep(id);
			}
		}
	}
}

/// Identity of a deferred operation.
///
/// Every deferred operation is keyed by *what it frees*, never by when it was
/// queued or by anything about the Rust objects involved.  That is what makes
/// duplicate detection work: a queue, a drain and a retry all have to agree
/// that they are talking about the same filesystem effect.
///
/// The inode number alone is not enough, and the difference matters.  Inode
/// numbers are reused, so a reclaim queued for inode 14 and a reclaim queued
/// later for a *different* inode that also happens to be 14 are not the same
/// operation.  The generation number disambiguates them: only a reclaim
/// carrying the generation that was current when it was queued may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpKey {
	/// Release a block's allocation bit.
	FreeBlock(u64),

	/// Release an inode's allocation bit, at this generation.
	FreeInode(InodeNum, u32),
}

impl OpKey {}

/// A filesystem operation that cannot be performed yet.
///
/// # Why some operations cannot be gates
///
/// A byte-range gate holds a range back by *zeroing* it, which is right for a
/// pointer -- zero means "no pointer here" -- and wrong for a counter, where
/// zero is a different and wrong number rather than a stale one.  Blocking the
/// whole buffer is sound for any content, but not for a cylinder group: it
/// holds shared allocation state, so freezing it would freeze every unrelated
/// allocation in the same group.
///
/// These operations therefore have to *wait to happen at all*, rather than merely
/// wait to be written.  That is what this type is.
///
/// # State machine
///
/// ```text
///                     queued
///                       |
///                       v
///                   Pending  <--------------------- container already on disk:
///                       |                              run immediately, never
///                  container written                 enqueued at all
///                       |
///                       v
///                   (removed from the queue, operation performed)
/// ```
///
/// Only `Pending` is a stored state.  Readiness is *derived* from the container's
/// persisted state rather than stored as a flag, so it cannot go stale between a
/// dependency resolving and the next drain.  "Applied" is represented by absence
/// from the queue: an operation is performed exactly once because performing it
/// removes it, and a second drain has nothing to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredOp {
	/// Release a block's allocation bit.
	///
	/// Until this runs, the block still reads as *allocated* -- on the disk and
	/// in the bitmap -- which is the point.  It must, because a crash in the
	/// window has an on-disk pointer still reaching it, and a block that read as
	/// free could be handed to the next allocation while that pointer survives.
	FreeBlock {
		/// The UFS block, a fragment address.
		blk: u64,

		/// How many bytes to release.  Always `fs_bsize` while allocation is
		/// whole-block, and part of the identity: the same block freed twice with
		/// different sizes is a double free either way.
		size: u64,

		/// The cache block that used to point at `blk`.
		container: u64,
	},

	/// Release an inode, and with it the cylinder group's idea of how many
	/// inodes and directories are in use.
	///
	/// The same reasoning as a block free: the inode still reads as allocated
	/// until this runs, because the directory entry naming it may still be on
	/// the disk.
	///
	/// The directory count is part of the operation rather than a second step.
	/// `cs_ndir`, `cs_nifree` and the bitmap bit all describe the same set of
	/// live inodes, and `check_consistency()` compares them against what the
	/// bitmap holds.  Moving one without the others is exactly the contradiction
	/// it reports, and after the inode is cleared there is nothing left to read
	/// the kind from -- so it is recorded here, where it is still known.
	FreeInode {
		/// The inode.
		inr: InodeNum,

		/// `di_gen` at the time the reclaim was queued.
		///
		/// The inode is zeroed by the time this runs, so `gen` is the only thing
		/// that distinguishes a reclaim of *this* inode from a reclaim of whatever
		/// later took the same inode number.
		gen: u32,

		/// Whether the inode was a directory, and so counted in `cs_ndir`.
		was_dir: bool,

		/// The cache block holding the inode's cleared image.
		container: u64,
	},
}

impl DeferredOp {
	/// This operation's identity.
	pub fn key(&self) -> OpKey {
		match self {
			Self::FreeBlock { blk, .. } => OpKey::FreeBlock(*blk),
			Self::FreeInode { inr, gen, .. } => OpKey::FreeInode(*inr, *gen),
		}
	}

	/// The cache block whose persistence makes this operation safe.
	pub fn container(&self) -> u64 {
		match self {
			Self::FreeBlock { container, .. } | Self::FreeInode { container, .. } => *container,
		}
	}

	/// Whether this operation may run now.
	///
	/// `persisted` says whether the container's current contents are on the
	/// device.  It is passed in rather than looked up so that the queue does not
	/// need the engine, and so that "runnable" is a question with one answer.
	pub fn is_runnable(&self, persisted: bool) -> bool {
		persisted
	}

	/// A short label, for logs and test failures.
	pub fn label(&self) -> String {
		match self {
			Self::FreeBlock { blk, .. } => format!("free-block/{blk}"),
			Self::FreeInode { inr, gen, .. } => format!("free-inode/{inr}@{gen}"),
		}
	}
}

/// The queue of operations waiting for a dependency.
///
/// Ordered by [`OpKey`], so a drain's behaviour does not depend on hash
/// iteration order.  Correctness matters more than the order in which frees are
/// applied -- a block free and an inode free touch different bitmaps -- but a
/// deterministic order makes the tests reproducible and the drain explainable.
///
/// Indexed by container as well, so a dependency resolving does not scan every
/// pending operation.
#[derive(Debug, Default)]
pub struct DeferredQueue {
	pending:      BTreeMap<OpKey, DeferredOp>,
	by_container: BTreeMap<u64, BTreeSet<OpKey>>,
}

impl DeferredQueue {
	/// An empty queue.
	pub fn new() -> Self {
		Self::default()
	}

	/// How many operations are waiting.
	pub fn len(&self) -> usize {
		self.pending.len()
	}

	/// Whether nothing is waiting.
	pub fn is_empty(&self) -> bool {
		self.pending.is_empty()
	}

	/// Every pending operation, in deterministic order.
	pub fn iter(&self) -> impl Iterator<Item = &DeferredOp> {
		self.pending.values()
	}

	/// Whether `op` is already queued.
	pub fn contains(&self, key: &OpKey) -> bool {
		self.pending.contains_key(key)
	}

	/// Queue `op`, refusing a duplicate.
	///
	/// Returns `false` if an operation with the same identity is already
	/// waiting.  The caller decides what that means -- a second free of the same
	/// block is a double free and should be rejected, whereas re-queueing after a
	/// partial drain that never ran the operation is a no-op -- but the queue
	/// itself only reports the fact.
	///
	/// Not relying on the allocation bitmap here is deliberate: a pending free
	/// leaves the block reading as *allocated*, so the bitmap cannot distinguish
	/// "free once" from "free twice".
	pub fn push(&mut self, op: DeferredOp) -> bool {
		let key = op.key();
		if self.pending.contains_key(&key) {
			return false;
		}
		self.by_container
			.entry(op.container())
			.or_default()
			.insert(key);
		self.pending.insert(key, op);
		true
	}

	/// Take every operation whose container has been persisted.
	///
	/// `persisted` is asked per container, not per operation, so a container
	/// holding several pending operations is looked up once.
	///
	/// The operations come out in [`OpKey`] order, so two drains over the same
	/// queue perform them in the same sequence.
	pub fn take_runnable(&mut self, mut persisted: impl FnMut(u64) -> bool) -> Vec<DeferredOp> {
		let ready: Vec<OpKey> = self
			.by_container
			.iter()
			.filter(|&(container, _)| persisted(*container))
			.flat_map(|(_, keys)| keys.iter().copied())
			.collect();
		ready
			.into_iter()
			.filter_map(|key| {
				let op = self.pending.remove(&key)?;
				if let Some(keys) = self.by_container.get_mut(&op.container()) {
					keys.remove(&key);
					if keys.is_empty() {
						self.by_container.remove(&op.container());
					}
				}
				Some(op)
			})
			.collect()
	}

	/// Every operation waiting on `container`.
	pub fn waiting_on(&self, container: u64) -> impl Iterator<Item = &DeferredOp> {
		self.by_container
			.get(&container)
			.into_iter()
			.flat_map(|keys| keys.iter())
			.filter_map(|key| self.pending.get(key))
	}
}

/// The write-ordering engine.
#[derive(Debug, Default)]
pub struct DependencyEngine {
	next:       u64,
	new_blocks: BTreeMap<DepId, NewBlockDep>,

	/// `InodeDep`s indexed by the inode they are about.
	inodes:   BTreeMap<DepId, InodeDep>,
	by_inode: BTreeMap<InodeNum, DepId>,

	/// Directory entries removed from the live image, waiting for the disk.
	removals: BTreeMap<Gate, DirectoryRemoveDep>,

	/// Inodes being reclaimed, with the removal each waits on and the cache
	/// block its cleared image lives in.
	reclaiming: BTreeMap<InodeNum, (Gate, u64)>,

	/// The newest removal for each removed inode.
	by_removed_inode: BTreeMap<InodeNum, Gate>,

	/// Buffers that have been written since this filesystem was opened, and so
	/// whose contents are on the disk.
	///
	/// This is what makes `Gate::PointersRemoved` decidable without a separate
	/// record per removal: "the pointer has been removed persistently" reduces to
	/// "that buffer has been written", because removing a pointer is an ordinary
	/// ungated change.  It is a set rather than a flag per gate because several
	/// frees can be waiting on the same container.
	written: BTreeSet<u64>,

	/// `NewBlockDep`s indexed by the block they are about.
	///
	/// A pointer path that has just allocated a block needs to know which
	/// allocation to gate on, and it has a block number, not a [`DepId`].
	/// Threading a `DepId` out of `Ufs::blk_alloc_for()` and back down into
	/// `Ufs::inode_set_block()` would put a dependency identifier into every
	/// allocator signature for the sake of one caller; the index answers the
	/// same question without that.
	///
	/// A block that is freed and reallocated within one operation gets a second
	/// entry, overwriting the first, which is the one that matters: the old
	/// allocation is no longer the reason any pointer is being held back.
	by_block: BTreeMap<u64, DepId>,

	deps:       BTreeMap<DepId, Dependency>,
	/// Gates keyed by what they wait for, so completing an allocation does not
	/// have to scan every dependency.
	waiting_on: BTreeMap<Gate, Vec<DepId>>,
}

impl DependencyEngine {
	/// A fresh engine with no dependencies.
	pub fn new() -> Self {
		Self::default()
	}

	/// How many dependencies are outstanding.
	pub fn len(&self) -> usize {
		self.deps.len()
	}

	/// Whether nothing is outstanding.
	pub fn is_empty(&self) -> bool {
		self.deps.is_empty() &&
			self.new_blocks.is_empty() &&
			self.inodes.is_empty() &&
			self.removals.is_empty() &&
			self.reclaiming.is_empty()
	}

	/// Register a newly allocated block.
	pub fn new_block(&mut self, blk: u64, cg: CgNum) -> DepId {
		let id = self.alloc_id();
		self.new_blocks.insert(
			id,
			NewBlockDep {
				id,
				blk,
				cg,
				bitmap: false,
				content: false,
				publish: false,
				complete: false,
			},
		);
		self.by_block.insert(blk, id);
		id
	}

	/// Look up a new-block dependency.
	pub fn new_block_dep(&self, id: DepId) -> Option<&NewBlockDep> {
		self.new_blocks.get(&id)
	}

	/// The dependency a block is waiting on, if it has one.
	///
	/// `None` once the allocation is complete, so a caller must treat "no
	/// dependency" and "already resolved" as the same answer, which they are:
	/// either way there is nothing left to wait for.
	pub fn dep_for_block(&self, blk: u64) -> Option<DepId> {
		self.by_block.get(&blk).copied()
	}

	/// Every outstanding new-block allocation.
	///
	/// The flush loop walks this after each write-back to decide which
	/// allocation events a completed write made true.
	pub fn new_blocks(&self) -> impl Iterator<Item = &NewBlockDep> {
		self.new_blocks.values()
	}

	/// Register a newly allocated inode.
	pub fn new_inode(&mut self, inr: InodeNum, cg: CgNum) -> DepId {
		let id = self.alloc_id();
		self.inodes.insert(
			id,
			InodeDep {
				id,
				inr,
				cg,
				image: false,
				bitmap: false,
			},
		);
		self.by_inode.insert(inr, id);
		id
	}

	/// Every outstanding inode allocation.
	pub fn new_inodes(&self) -> impl Iterator<Item = &InodeDep> {
		self.inodes.values()
	}

	/// Record that an inode's image has reached the device.
	pub fn note_inode_written(&mut self, id: DepId) -> IoResult<bool> {
		let dep = self.inodes.get_mut(&id).ok_or_else(|| invalid_dep(id))?;
		dep.image = true;
		let (inr, safe) = (dep.inr, dep.allows_naming());
		self.settle_inode(inr);
		Ok(safe)
	}

	/// Record that an inode's allocation bitmap bit has reached the device.
	pub fn note_inode_bitmap_written(&mut self, id: DepId) -> IoResult<bool> {
		let dep = self.inodes.get_mut(&id).ok_or_else(|| invalid_dep(id))?;
		dep.bitmap = true;
		let (inr, safe) = (dep.inr, dep.allows_naming());
		self.settle_inode(inr);
		Ok(safe)
	}

	fn settle_inode(&mut self, inr: InodeNum) {
		if !self.gate_is_open(Gate::InodeWritten(inr)) {
			return;
		}
		if let Some(ids) = self.waiting_on.remove(&Gate::InodeWritten(inr)) {
			for id in ids {
				if self.deps.get(&id).is_some_and(|d| d.resolved) {
					continue;
				}
				self.publish_dep(id);
			}
		}
	}

	/// The allocation state of a new-block dependency.
	pub fn allocation_state(&self, id: DepId) -> Option<AllocationState> {
		self.new_blocks.get(&id).map(NewBlockDep::state)
	}

	/// Record that a block's allocation bitmap has reached the device.
	pub fn note_bitmap_written(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| d.bitmap = true)
	}

	/// Record that a block's initialised contents have reached the device.
	pub fn note_contents_written(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| d.content = true)
	}

	/// Record that a pointer to the block has reached the device.
	pub fn note_pointer_published(&mut self, id: DepId) -> IoResult<bool> {
		self.note(id, |d| {
			d.publish = true;
			d.finish();
		})
	}

	fn note(&mut self, id: DepId, f: impl FnOnce(&mut NewBlockDep)) -> IoResult<bool> {
		let dep = self
			.new_blocks
			.get_mut(&id)
			.ok_or_else(|| invalid_dep(id))?;
		f(dep);
		let safe = dep.allows_publication();
		let (blk, done) = (dep.blk, dep.complete);
		self.settle(&id);
		if done {
			// Nothing can be waiting on a finished allocation, so there is
			// nothing to keep.  Dropping it here is what stops `dep_for_block`
			// from reporting a resolved dependency as outstanding.
			self.by_block.remove(&blk);
			self.new_blocks.remove(&id);
		}
		Ok(safe)
	}

	/// Advance a new-block allocation and re-evaluate every gate that was
	/// waiting on it.
	///
	/// This is the *only* place a resource's progress becomes visible to
	/// pointers, which is why a caller cannot accidentally forget to unblock a
	/// dependency.
	fn settle(&mut self, id: &DepId) {
		let open: Vec<Gate> = [
			Gate::AllocationAllocated(*id),
			Gate::AllocationInitialised(*id),
			Gate::AllocationSafe(*id),
		]
		.into_iter()
		.filter(|g| self.gate_is_open(*g))
		.collect();
		for g in open {
			if let Some(ids) = self.waiting_on.remove(&g) {
				for d in ids {
					if self.deps.get(&d).is_some_and(|d| d.resolved) {
						continue;
					}
					self.publish_dep(d);
				}
			}
		}
	}

	fn publish_dep(&mut self, id: DepId) {
		if let Some(d) = self.deps.get_mut(&id) {
			d.resolved = true;
		}
	}

	/// Gate a byte range of a buffer until `gate` opens.
	///
	/// The range is marked unsafe in the buffer cache immediately, so a flush
	/// before the gate opens writes the safe image.
	pub fn gate(
		&mut self,
		cache: &mut BufferCache,
		kind: DepKind,
		blk: u64,
		off: u64,
		len: u64,
		gate: Gate,
	) -> IoResult<DepId> {
		// A gate on something that cannot change is a programming error, and the
		// cost of accepting one is unbounded: the range is held back and nothing
		// will ever release it, so a caller who asked for an ordering guarantee
		// silently gets a filesystem that stops changing instead.
		//
		// The case that matters is a *retired* allocation.  `note_pointer_published`
		// removes a completed `NewBlockDep`, after which `Gate::AllocationSafe(id)`
		// and "an allocation that never started" are indistinguishable -- both are
		// simply absent.  `Ufs` does not hit this, because it creates a pointer
		// dependency before its allocation can complete, but nothing in the
		// engine enforced that and a future change to the ordering would hit it as
		// a hang rather than as an error.
		match gate {
			Gate::AllocationAllocated(id) |
			Gate::AllocationInitialised(id) |
			Gate::AllocationSafe(id)
				if !self.new_blocks.contains_key(&id) =>
			{
				return Err(std::io::Error::other(format!(
					"cannot gate on allocation {id:?}: it is not an outstanding \
					 allocation (it may have been retired)"
				)));
			}
			Gate::InodeWritten(inr) if !self.by_inode.contains_key(&inr) => {
				return Err(std::io::Error::other(format!(
					"cannot gate on inode {inr:?}: it was never registered"
				)));
			}
			_ => {}
		}

		let id = self.alloc_id();
		// The buffer must be resident: a caller that has just written a pointer
		// into it necessarily has, and a gate on a block that is not in the
		// cache would be silently ineffective.
		let Some(b) = cache.peek_mut(blk) else {
			return Err(std::io::Error::other(format!(
				"cannot gate block {blk}: it is not in the buffer cache"
			)));
		};
		b.mark_unsafe(off, len);
		self.deps.insert(
			id,
			Dependency {
				id,
				kind,
				blk,
				off,
				len,
				gate,
				resolved: false,
			},
		);
		// A gate whose precondition already holds must open immediately, or the
		// range would stay unpublished forever.
		//
		// `Gate::PointersRemoved` is the exception, and deliberately so.  "The
		// container's contents are on the disk" is true at the moment the gate is
		// created -- the pointer removal has not been staged yet -- and becomes
		// false again a moment later, when it is.  Deciding here would open the
		// gate and publish the freed block's accounting before the removal it is
		// waiting for had happened.  It is decided on the *next* write or fetch of
		// that container instead, which is the first moment the answer is true.
		let eager = !matches!(gate, Gate::PointersRemoved { .. }) && self.gate_is_open(gate);
		if eager {
			self.publish_dep(id);
		} else {
			self.waiting_on.entry(gate).or_default().push(id);
		}
		Ok(id)
	}

	/// Look up a dependency.
	pub fn dep(&self, id: DepId) -> Option<&Dependency> {
		self.deps.get(&id)
	}

	/// Every outstanding dependency.
	///
	/// Test-only, like the accessors on `Ufs`: the graph is the scheduler's
	/// business, and a caller that could enumerate it could also reason about
	/// gates it has no business touching.
	#[cfg(test)]
	pub fn all(&self) -> impl Iterator<Item = &Dependency> {
		self.deps.values()
	}

	/// Whether a dependency has been published.
	pub fn is_resolved(&self, id: DepId) -> bool {
		self.deps.get(&id).is_some_and(|d| d.resolved)
	}

	/// Declare that an event has happened, opening every gate that waited on
	/// it.
	pub fn signal(&mut self, cache: &mut BufferCache, gate: Gate) -> IoResult<()> {
		let _ = cache;
		let Some(ids) = self.waiting_on.remove(&gate) else {
			return Ok(());
		};
		for id in ids {
			self.publish_dep(id);
		}
		Ok(())
	}

	/// Publish every resolved dependency's byte range into `cache`.
	///
	/// The engine keeps the gates; the cache keeps the buffer state.  This is
	/// the single point where the two meet, which is what keeps the dependency
	/// graph free of any knowledge of `Buffer` internals.
	pub fn publish_into(&self, cache: &mut BufferCache) {
		for d in self.deps.values().filter(|d| d.resolved) {
			if let Some(b) = cache.peek_mut(d.blk) {
				b.publish(d.off, d.len);
			}
		}
	}

	/// Whether a buffer has any gated ranges left.
	pub fn block_is_publishable(&self, cache: &BufferCache, blk: u64) -> bool {
		!cache.peek(blk).is_some_and(|b| b.has_unsafe_ranges())
	}

	/// Write every buffer whose gates have opened, oldest first.
	///
	/// This is the scheduler.  It writes buffers in the order they became
	/// dirty, skips any buffer that still has a gated range, and reports what
	/// each write did so a caller (or a test) can tell a full write from a safe
	/// one.
	pub fn write_ready(
		&mut self,
		cache: &mut BufferCache,
		dev: &mut dyn crate::buf::BlockDevice,
	) -> IoResult<Vec<(u64, Written)>> {
		cache.write_back_all(dev)
	}

	/// Whether every dependency is resolved: the filesystem is in a
	/// crash-consistent state and the cache can be discarded.
	pub fn is_quiescent(&self) -> bool {
		self.deps.values().all(|d| d.resolved) &&
			self.new_blocks
				.values()
				.all(NewBlockDep::allows_publication) &&
			self.inodes.values().all(InodeDep::allows_naming) &&
			self.removals.values().all(DirectoryRemoveDep::is_persisted) &&
			self.reclaiming
				.keys()
				.all(|i| self.gate_is_open(Gate::InodeReclaimed(*i)))
	}

	fn alloc_id(&mut self) -> DepId {
		self.next += 1;
		DepId(self.next)
	}

	/// Re-evaluate a new-block allocation: has it become publishable?
	/// Whether `gate`'s precondition already holds.
	///
	/// A caller that wants to *set* a gate needs this: there is no point
	/// recording a dependency on something that has already happened.
	pub fn gate_is_open(&self, gate: Gate) -> bool {
		match gate {
			Gate::AllocationAllocated(id) => self.new_blocks.get(&id).is_some_and(|n| n.bitmap),
			Gate::AllocationInitialised(id) => self.new_blocks.get(&id).is_some_and(|n| n.content),
			Gate::AllocationSafe(id) => {
				self.new_blocks
					.get(&id)
					.is_some_and(NewBlockDep::allows_publication)
			}
			Gate::InodeWritten(inr) => {
				self.by_inode
					.get(&inr)
					.and_then(|id| self.inodes.get(id))
					.is_some_and(InodeDep::allows_naming)
			}
			Gate::DirectoryRemoved { parent, blk, off } => {
				self.removals
					.get(&Gate::DirectoryRemoved { parent, blk, off })
					.is_some_and(DirectoryRemoveDep::is_persisted)
			}
			Gate::PointersRemoved { container } => self.written.contains(&container),
			// An inode nobody is reclaiming has no transition to wait for, and a
			// gate on one is never created in the first place; the safe answer
			// here is "shut", so that a gate created without a reclamation behind
			// it cannot publish anything on its own.
			Gate::InodeReclaimed(inr) => {
				self.reclaiming.get(&inr).is_some_and(|(removal, blk)| {
					// The entry is gone *and* the cleared image is on the disk.
					self.gate_is_open(*removal) && self.written.contains(blk)
				})
			}
			Gate::DirectoryPersisted { blk, .. } => self.written.contains(&blk),
			Gate::InodeLinkCounted(_) => false,
		}
	}
}

impl DepKind {
	/// Alias kept for the documentation test's readability.
	#[cfg(test)]
	fn prevented_by_check(self) -> &'static str {
		self.prevents()
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::buf::BlockDevice;

	const BSIZE: u64 = 4096;

	#[derive(Default)]
	pub(super) struct MemDev {
		data:   Vec<u8>,
		writes: Vec<(u64, Vec<u8>)>,
	}

	impl BlockDevice for MemDev {
		fn read_at(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()> {
			let end = (off as usize + buf.len()).min(self.data.len());
			let start = (off as usize).min(end);
			buf[..end - start].copy_from_slice(&self.data[start..end]);
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

	pub(super) fn setup() -> (DependencyEngine, BufferCache, MemDev) {
		let dev = MemDev {
			data:   vec![0u8; (BSIZE * 8) as usize],
			writes: Vec::new(),
		};
		(DependencyEngine::new(), BufferCache::new(BSIZE, BSIZE), dev)
	}

	/// The two first steps of a new-block dependency may happen in either
	/// order; neither alone allows publication.
	#[test]
	fn new_block_state_machine() {
		let mut e = DependencyEngine::new();
		let cg = CgNum::new(1);
		let id = e.new_block(617, cg);
		assert_eq!(e.allocation_state(id), Some(AllocationState::New));
		assert!(!e.new_block_dep(id).unwrap().allows_publication());

		e.note_bitmap_written(id).unwrap();
		assert_eq!(e.allocation_state(id), Some(AllocationState::BitmapWritten));
		assert!(!e.new_block_dep(id).unwrap().allows_publication());

		e.note_contents_written(id).unwrap();
		assert_eq!(
			e.allocation_state(id),
			Some(AllocationState::ContentsWritten)
		);
		assert!(e.new_block_dep(id).unwrap().allows_publication());
	}

	/// Contents-first is equally valid, which is the whole reason the state is
	/// two booleans and not a linear enum.
	#[test]
	fn contents_before_bitmap_is_equally_valid() {
		let mut e = DependencyEngine::new();
		let id = e.new_block(617, CgNum::new(1));
		e.note_contents_written(id).unwrap();
		assert!(!e.new_block_dep(id).unwrap().allows_publication());
		e.note_bitmap_written(id).unwrap();
		assert!(e.new_block_dep(id).unwrap().allows_publication());
	}

	/// An unknown dependency is an error rather than a silent no-op: a typo in
	/// a caller's bookkeeping would otherwise be an invisible crash-safety bug.
	#[test]
	fn unknown_dependency_is_an_error() {
		let mut e = DependencyEngine::new();
		assert!(e.note_bitmap_written(DepId(999)).is_err());
		assert!(e.note_contents_written(DepId(999)).is_err());
		assert!(e.note_pointer_published(DepId(999)).is_err());
	}

	/// The fundamental case: a direct pointer must not be publishable until the
	/// block's bitmap *and* contents are on disk.
	#[test]
	fn direct_pointer_waits_for_bitmap_and_contents() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 2).unwrap();
		let blk = e.new_block(617, CgNum::new(1));

		// The inode's direct[0] slot points at the new block.
		let _ptr = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2, // the inode's block
				0, // direct[0]
				8,
				Gate::AllocationSafe(blk),
			)
			.unwrap();
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());
		assert!(!e.is_resolved(_ptr));

		// The bitmap alone is not enough.
		e.note_bitmap_written(blk).unwrap();
		assert!(!e.is_resolved(_ptr));
		e.publish_into(&mut cache);
		assert!(!e.block_is_publishable(&cache, 2));

		// Neither is the contents alone.
		let blk2 = e.new_block(618, CgNum::new(1));
		let _p2 = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				8,
				8,
				Gate::AllocationSafe(blk2),
			)
			.unwrap();
		e.note_contents_written(blk2).unwrap();
		assert!(!e.is_resolved(_p2), "contents alone must not publish");
		e.publish_into(&mut cache);
		assert!(!e.is_resolved(_p2));
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());

		// Both, and the gates open.
		e.note_contents_written(blk).unwrap();
		assert!(e.is_resolved(_ptr));
		assert!(!e.is_resolved(_p2));
		e.publish_into(&mut cache);
		assert!(cache.peek(2).unwrap().has_unsafe_ranges());

		e.note_bitmap_written(blk2).unwrap();
		assert!(e.is_resolved(_p2));
		e.publish_into(&mut cache);
		assert!(e.block_is_publishable(&cache, 2));
	}

	/// A direct pointer gate on a buffer that is flushed before its gate opens
	/// writes zeros for the pointer; the rest of the inode still persists.
	#[test]
	fn partial_write_keeps_the_rest_of_the_inode() {
		let (mut e, mut cache, mut dev) = setup();
		let blk = e.new_block(617, CgNum::new(1));

		cache.get_mut(&mut dev, 2).unwrap().data_mut()[0..8].copy_from_slice(&617u64.to_le_bytes());
		cache.get_mut(&mut dev, 2).unwrap().data_mut()[8..16]
			.copy_from_slice(&4242u64.to_le_bytes());
		e.gate(
			&mut cache,
			DepKind::DirectPointer,
			2,
			0,
			8,
			Gate::AllocationSafe(blk),
		)
		.unwrap();

		let w = e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(w, vec![(2, Written::Safe)]);
		assert_eq!(
			&dev.writes[0].1[0..8],
			&0u64.to_le_bytes(),
			"pointer not published"
		);
		assert_eq!(
			&dev.writes[0].1[8..16],
			&4242u64.to_le_bytes(),
			"the rest of the inode must persist"
		);

		// After the allocation completes, the pointer becomes publishable.
		e.note_bitmap_written(blk).unwrap();
		e.note_contents_written(blk).unwrap();
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 2).unwrap();
		let w = e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(w, vec![(2, Written::Full)]);
		assert_eq!(&dev.writes[1].1[0..8], &617u64.to_le_bytes());
	}

	/// The crash-state table: after each prefix of the required ordering, is a
	/// persistent pointer to the new block present without it being allocated?
	#[test]
	fn crash_at_each_intermediate_point() {
		// (bitmap?, contents?) -> is the pointer safe to persist?
		let table = [
			(false, false, false),
			(true, false, false),
			(false, true, false),
			(true, true, true),
		];

		for (bitmap, contents, expect_safe) in table {
			let (mut e, mut cache, mut dev) = setup();
			let blk = e.new_block(617, CgNum::new(1));
			cache.get_mut(&mut dev, 2).unwrap().data_mut()[0..8]
				.copy_from_slice(&617u64.to_le_bytes());
			let p = e
				.gate(
					&mut cache,
					DepKind::DirectPointer,
					2,
					0,
					8,
					Gate::AllocationSafe(blk),
				)
				.unwrap();
			if bitmap {
				e.note_bitmap_written(blk).unwrap();
			}
			if contents {
				e.note_contents_written(blk).unwrap();
			}
			e.publish_into(&mut cache);
			e.write_ready(&mut cache, &mut dev).unwrap();

			let persisted = dev.writes[0].1[0..8] == 617u64.to_le_bytes();
			assert_eq!(
				persisted,
				expect_safe,
				"bitmap={bitmap} contents={contents}: the pointer must {} be persistable",
				if expect_safe { "" } else { "not" }
			);
			// Invariant: a persisted pointer implies an allocated block.
			if persisted {
				assert!(bitmap && contents);
			}
			assert_eq!(e.is_resolved(p), expect_safe);
		}
	}

	/// Two pointers into the same indirect block: one safe, one not.  The safe
	/// image must keep the first and zero the second, which is the `[A B 0 D]`
	/// example from the design.
	#[test]
	fn indirect_block_safe_image() {
		let (mut e, mut cache, mut dev) = setup();
		let a = e.new_block(100, CgNum::new(0));
		let b = e.new_block(101, CgNum::new(0));
		let c = e.new_block(102, CgNum::new(0));
		let d = e.new_block(103, CgNum::new(0));

		{
			let buf = cache.get_mut(&mut dev, 5).unwrap();
			for (slot, v) in [100u64, 101, 102, 103].iter().enumerate() {
				buf.data_mut()[slot * 8..slot * 8 + 8].copy_from_slice(&v.to_le_bytes());
			}
		}

		for (blk, dep, slot) in [
			(a, DepKind::IndirectPointer, 0usize),
			(b, DepKind::IndirectPointer, 1),
			(c, DepKind::IndirectPointer, 2),
			(d, DepKind::IndirectPointer, 3),
		] {
			e.gate(
				&mut cache,
				dep,
				5,
				(slot * 8) as u64,
				8,
				Gate::AllocationSafe(blk),
			)
			.unwrap();
		}

		// Only A and D are safe.
		e.note_bitmap_written(a).unwrap();
		e.note_contents_written(a).unwrap();
		e.note_bitmap_written(d).unwrap();
		e.note_contents_written(d).unwrap();
		e.publish_into(&mut cache);

		e.write_ready(&mut cache, &mut dev).unwrap();
		let img = &dev.writes[0].1;
		assert_eq!(&img[0..8], &100u64.to_le_bytes(), "A is safe");
		assert_eq!(&img[8..16], &0u64.to_le_bytes(), "B is not");
		assert_eq!(&img[16..24], &0u64.to_le_bytes(), "C is not");
		assert_eq!(&img[24..32], &103u64.to_le_bytes(), "D is safe");

		// Now B and C become safe too.
		for dep in [b, c] {
			e.note_bitmap_written(dep).unwrap();
			e.note_contents_written(dep).unwrap();
		}
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 5).unwrap();
		e.write_ready(&mut cache, &mut dev).unwrap();
		let img = &dev.writes[1].1;
		assert_eq!(&img[8..16], &101u64.to_le_bytes());
		assert_eq!(&img[16..24], &102u64.to_le_bytes());
	}

	/// A directory entry may not persist before the inode it names.
	#[test]
	fn directory_add_gates_on_the_inode() {
		let (mut e, mut cache, mut dev) = setup();
		let ino = unsafe { InodeNum::new(700) };
		// `gate()` refuses an inode that was never registered: nothing would ever
		// open that gate, so the range would be held back forever.
		e.new_inode(ino, CgNum::new(0));
		cache.get_mut(&mut dev, 4).unwrap().data_mut()[0..8].copy_from_slice(&700u64.to_le_bytes());
		let p = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		assert!(!e.is_resolved(p));

		e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(&dev.writes[0].1[0..8], &0u64.to_le_bytes());

		// The inode write signals the gate.  (Gates on inode state are opened
		// by `signal`, not by a resource allocation.)
		e.signal(&mut cache, Gate::InodeWritten(ino)).unwrap();
		assert!(e.is_resolved(p));
		e.publish_into(&mut cache);
		cache.get_mut(&mut dev, 4).unwrap();
		e.write_ready(&mut cache, &mut dev).unwrap();
		assert_eq!(&dev.writes[1].1[0..8], &700u64.to_le_bytes());
	}

	/// Gates on inode state do not open on their own.
	#[test]
	fn inode_gates_stay_closed_until_signalled() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(700) };
		// `InodeWritten` needs the inode registered; `gate()` refuses one that
		// was never allocated, because nothing would ever open it.
		e.new_inode(ino, CgNum::new(0));
		// `InodeLinkCounted` and `InodeReclaimed` are left out: they protect
		// counters, nothing opens them, and `a_dependency_that_can_never_resolve_
		// is_reported` covers that they are still visible when something does
		// raise one.
		for gate in [
			Gate::InodeWritten(ino),
			Gate::DirectoryPersisted {
				parent: ino,
				blk:    4,
			},
		] {
			let p = e
				.gate(&mut cache, DepKind::DirectoryAdd, 4, 0, 8, gate)
				.unwrap();
			assert!(!e.is_resolved(p), "{gate:?} must not open by itself");
			e.signal(&mut cache, gate).unwrap();
			assert!(e.is_resolved(p), "{gate:?} must open on its signal");
		}
	}

	/// Gates on a *different* resource must not open each other.
	#[test]
	fn gates_are_not_cross_satisfied() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 2).unwrap();
		let a = e.new_block(100, CgNum::new(0));
		let b = e.new_block(101, CgNum::new(0));
		let pa = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				0,
				8,
				Gate::AllocationSafe(a),
			)
			.unwrap();
		let pb = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				2,
				8,
				8,
				Gate::AllocationSafe(b),
			)
			.unwrap();

		e.note_bitmap_written(a).unwrap();
		e.note_contents_written(a).unwrap();
		assert!(e.is_resolved(pa));
		assert!(!e.is_resolved(pb));

		e.note_bitmap_written(b).unwrap();
		assert!(!e.is_resolved(pb), "bitmap alone is not enough");
	}

	/// Every dependency documents what creates it, what it enforces, what it
	/// prevents, what resolves it and which invariant it protects.
	#[test]
	fn every_dependency_kind_documents_itself() {
		for kind in [
			DepKind::DirectPointer,
			DepKind::IndirectPointer,
			DepKind::DirectoryAdd,
		] {
			for s in [
				kind.created_by(),
				kind.enforces(),
				kind.prevented_by_check(),
				kind.resolved_by(),
				kind.protects(),
			] {
				assert!(!s.is_empty());
			}
		}
	}
}

impl DependencyEngine {
	/// How many dependencies are created and *not yet resolved*.
	///
	/// Counting `deps.len()` instead would include resolved ones, which the
	/// engine keeps for history and never removes.  That mistake made two
	/// correctly-published dependencies look like two stuck ones, and sent the
	/// investigation looking for a lifecycle bug that was not there.
	pub fn unresolved(&self) -> usize {
		self.deps.values().filter(|d| !d.resolved).count()
	}

	/// How many allocations are still short of publishable.
	pub fn in_flight(&self) -> usize {
		self.new_blocks
			.values()
			.filter(|n| !n.allows_publication())
			.count()
	}

	/// The unresolved dependencies and where each says it is waiting.
	///
	/// `(id, kind, gate, resolved, listed)`.  A correct engine lists only
	/// unresolved dependencies, each under its own gate; anything else is a
	/// registration bug, and `validate()` says which.
	pub fn unresolved_gates(&self) -> Vec<(DepId, DepKind, Gate, bool, bool)> {
		self.deps
			.values()
			.map(|d| {
				let listed = self
					.waiting_on
					.get(&d.gate)
					.is_some_and(|ids| ids.contains(&d.id));
				(d.id, d.kind, d.gate, d.resolved, listed)
			})
			.collect()
	}

	/// Report anything that cannot be right.
	///
	/// Small assertions, not a theorem prover.  The point is not to prove the
	/// graph is sound -- it is to make a *future* mistake fail loudly here
	/// rather than quietly leaving metadata unpublished or, worse, published
	/// early.
	///
	/// Returns one string per problem; empty means nothing obviously wrong.
	pub fn validate(&self) -> Vec<String> {
		let mut out = Vec::new();

		// A gate nothing ever opens.  `Gate::InodeLinkCounted` and
		// `Gate::InodeReclaimed` have no producer, because both protect a
		// counter and a counter cannot be gated; a range held back on one of
		// them would stay held back for the life of the mount.
		for (gate, ids) in &self.waiting_on {
			if matches!(gate, Gate::InodeLinkCounted(_) | Gate::InodeReclaimed(_)) {
				out.push(format!(
					"{gate:?} is waiting on {ids:?} but nothing ever opens it; \
					 the gated range would never be published"
				));
			}
			// A gate that has opened while its dependency still waits means the
			// two disagree about the world.
			if self.gate_is_open(*gate) {
				for id in ids {
					out.push(format!("{gate:?} is already open but {id:?} still waits"));
				}
			}
		}

		// A completed allocation still in the map: it should have been pruned.
		for (id, n) in &self.new_blocks {
			if n.complete {
				out.push(format!("{id:?} is complete but was never pruned"));
			}
		}

		// An *unresolved* dependency naming an allocation or an inode that does
		// not exist.  A resolved one may legitimately name a retired
		// allocation: `note_pointer_published` removes the `NewBlockDep`, and the
		// dependency it published stays in `deps` for history.
		for (id, dep) in self.deps.iter().filter(|(_, d)| !d.resolved) {
			match dep.gate {
				Gate::AllocationAllocated(a) |
				Gate::AllocationInitialised(a) |
				Gate::AllocationSafe(a)
					if !self.new_blocks.contains_key(&a) =>
				{
					out.push(format!("{id:?} gates on {a:?}, which is not an allocation"));
				}
				Gate::InodeWritten(inr) if !self.by_inode.contains_key(&inr) => {
					out.push(format!(
						"{id:?} gates on inode {inr:?}, which was never registered"
					));
				}
				_ => {}
			}
		}

		out.extend(self.validate_registration());
		out
	}

	/// The registration invariant: an unresolved dependency occurs in exactly one
	/// `waiting_on` entry -- the one keyed by its own gate -- and a resolved one
	/// occurs in none.
	///
	/// `deps` and `waiting_on` are two representations of the same relationship,
	/// and nothing keeps them in step except the places that touch `waiting_on`:
	/// `gate`, `settle`, `settle_inode`, `settle_removal` and `signal`.  This is
	/// where a mistake in one of them shows up, and naming the shape of the
	/// corruption is worth more than guessing at a fix.
	fn validate_registration(&self) -> Vec<String> {
		let mut out = Vec::new();

		// Where each dependency claims to be waiting.
		let mut claims: BTreeMap<DepId, Vec<&Gate>> = BTreeMap::new();
		for (gate, ids) in &self.waiting_on {
			let mut seen = BTreeSet::new();
			for id in ids {
				if !seen.insert(*id) {
					out.push(format!("case D: {id:?} is listed twice under {gate:?}"));
				}
				claims.entry(*id).or_default().push(gate);
			}
		}

		for (id, dep) in &self.deps {
			let empty: &[&Gate] = &[];
			let sites = claims.get(id).map(Vec::as_slice).unwrap_or(empty);
			match (dep.resolved, sites.len()) {
				// Resolved and listed nowhere: correct.
				(true, 0) => {}
				// Case A: published, but the entry survived.
				(true, _) => {
					out.push(format!(
						"case A: {id:?} is resolved but still listed under {sites:?}"
					))
				}
				// Case B: honestly waiting, under its own gate, and the gate is
				// open -- so whoever should have published it did not.
				(false, 1) => {
					if sites[0] != &dep.gate {
						out.push(format!(
							"case C: {id:?} gates on {:?} but is listed under {:?}",
							dep.gate, sites[0]
						));
					} else if self.gate_is_open(dep.gate) {
						out.push(format!(
							"case B: {id:?} is unresolved under {:?}, which is open",
							dep.gate
						));
					}
				}
				// Case E: unresolved and listed nowhere, so nothing will publish it.
				(false, 0) => out.push(format!("case E: {id:?} is unresolved and listed nowhere")),
				// Case C: listed under more than one gate, or the wrong one.
				(false, _) => {
					out.push(format!(
						"case C: {id:?} gates on {:?} but is listed under {sites:?}",
						dep.gate
					))
				}
			}
		}

		// Case F: listed but no longer a dependency at all.
		for (gate, ids) in &self.waiting_on {
			for id in ids {
				if !self.deps.contains_key(id) {
					out.push(format!(
						"case F: {id:?} is listed under {gate:?} but is not a dependency"
					));
				}
			}
		}

		out
	}
}

#[cfg(test)]
mod deferred {
	use super::{t::setup, *};

	fn free_block(blk: u64, container: u64) -> DeferredOp {
		DeferredOp::FreeBlock {
			blk,
			size: 32768,
			container,
		}
	}

	fn free_inode(inr: u32, gen: u32, container: u64) -> DeferredOp {
		DeferredOp::FreeInode {
			inr: unsafe { InodeNum::new(inr) },
			gen,
			was_dir: false,
			container,
		}
	}

	/// Nothing is runnable until its container is on the disk.
	#[test]
	fn an_operation_is_not_runnable_before_its_container_is_persisted() {
		let op = free_block(100, 7);
		assert!(!op.is_runnable(false));
		assert!(op.is_runnable(true));

		let mut q = DeferredQueue::new();
		assert!(q.push(op));
		assert_eq!(q.len(), 1);
		// The container is not persisted, so nothing comes out -- twice, to show
		// that a retry changes nothing.
		assert!(q.take_runnable(|_| false).is_empty());
		assert!(q.take_runnable(|_| false).is_empty());
		assert_eq!(q.len(), 1, "a failed drain must not lose the work");
	}

	/// A runnable operation comes out exactly once.
	#[test]
	fn a_runnable_operation_is_taken_exactly_once() {
		let mut q = DeferredQueue::new();
		assert!(q.push(free_block(100, 7)));
		let out = q.take_runnable(|c| c == 7);
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].label(), "free-block/100");
		assert!(q.is_empty());
		// A second drain has nothing to do.
		assert!(q.take_runnable(|_| true).is_empty());
		assert_eq!(q.len(), 0);
	}

	/// Duplicate detection is by identity, not by timing.
	#[test]
	fn a_duplicate_is_refused() {
		let mut q = DeferredQueue::new();
		assert!(q.push(free_block(100, 7)), "first");
		assert!(!q.push(free_block(100, 7)), "the same block twice");
		assert_eq!(q.len(), 1, "the duplicate was not queued again");

		// A *different* block, even on the same container, is not a duplicate.
		assert!(q.push(free_block(101, 7)));
		assert_eq!(q.len(), 2);

		// The same inode at a different generation is a different operation:
		// inode numbers are reused, and the old reclaim must not be satisfied by
		// the new inode's.
		assert!(q.push(free_inode(14, 7, 9)));
		assert!(q.push(free_inode(14, 8, 9)));
		assert!(q.push(free_inode(15, 7, 9)));
		assert_eq!(q.len(), 5);
	}

	/// The order does not depend on insertion order.
	#[test]
	fn the_drain_order_is_deterministic() {
		let mut a = DeferredQueue::new();
		let mut b = DeferredQueue::new();
		for blk in [300u64, 100, 200] {
			a.push(free_block(blk, 1));
		}
		for blk in [200u64, 300, 100] {
			b.push(free_block(blk, 1));
		}
		let order = |q: &mut DeferredQueue| {
			q.take_runnable(|_| true)
				.into_iter()
				.map(|o| o.label())
				.collect::<Vec<_>>()
		};
		let first = order(&mut a);
		assert_eq!(
			first,
			order(&mut b),
			"insertion order changed the drain order"
		);
		assert_eq!(
			first,
			["free-block/100", "free-block/200", "free-block/300"]
		);
	}

	/// Several operations can share a container; resolving it releases them all,
	/// and resolving a *different* container releases none of them.
	#[test]
	fn a_container_releases_only_its_own() {
		let mut q = DeferredQueue::new();
		for blk in [100u64, 101, 102] {
			assert!(q.push(free_block(blk, 7)));
		}
		assert!(q.push(free_block(200, 8)));
		assert_eq!(q.waiting_on(7).count(), 3);

		let out = q.take_runnable(|c| c == 7);
		assert_eq!(out.len(), 3);
		assert_eq!(q.len(), 1, "the other container's work is untouched");
		assert_eq!(q.waiting_on(7).count(), 0);

		let out = q.take_runnable(|_| true);
		assert_eq!(out.len(), 1);
		assert!(q.is_empty());
	}

	/// Repeatedly draining reaches zero, which is the liveness property: an
	/// operation whose container is eventually written must not be stranded.
	#[test]
	fn repeated_drains_make_progress() {
		let mut q = DeferredQueue::new();
		for blk in 0..16u64 {
			assert!(q.push(free_block(blk, blk % 4)));
		}
		let mut persisted: std::collections::BTreeSet<u64> = Default::default();
		let mut rounds = 0u64;
		while !q.is_empty() {
			// One more container is persisted per round: this is the
			// "prerequisite eventually happens" the liveness claim rests on.
			persisted.insert(rounds % 4);
			let before = q.len();
			q.take_runnable(|c| persisted.contains(&c));
			assert!(
				q.len() < before,
				"a drain made no progress at round {rounds}"
			);
			rounds += 1;
			assert!(rounds < 16, "the queue did not drain");
		}
		assert!(q.is_empty());
	}

	/// Validation catches a gate that nothing will ever open.
	///
	/// `Gate::InodeLinkCounted` and `Gate::InodeReclaimed` protect counters,
	/// and a counter cannot be byte-range gated, so nothing raises them.  A
	/// range held back on one would stay held back for the life of the mount,
	/// which is the kind of failure that looks like "the filesystem got slow".
	#[test]
	fn validate_reports_a_gate_nothing_opens() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		assert!(
			e.validate().is_empty(),
			"a fresh engine is clean: {:?}",
			e.validate()
		);

		let ino = unsafe { InodeNum::new(700) };
		for gate in [Gate::InodeLinkCounted(ino), Gate::InodeReclaimed(ino)] {
			e.gate(&mut cache, DepKind::DirectoryRemove, 4, 0, 8, gate)
				.unwrap();
		}
		let problems = e.validate();
		assert_eq!(problems.len(), 2, "{problems:?}");
		assert!(
			problems.iter().all(|p| p.contains("nothing ever opens")),
			"{problems:?}"
		);

		// And the buffer is still there, still dirty, and would never be written.
		assert!(cache.peek(4).unwrap().has_unsafe_ranges());
	}

	/// Validation catches a dependency on something that does not exist, which
	/// is what a stale `DepId` looks like.
	#[test]
	fn validate_reports_an_unknown_allocation() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ghost = DepId(4242);
		// `gate()` itself refuses an unknown allocation, so reach the state
		// through a real allocation and then take it away.
		let real = e.new_block(100, crate::geom::CgNum::new(0));
		e.gate(
			&mut cache,
			DepKind::DirectPointer,
			4,
			0,
			8,
			Gate::AllocationSafe(real),
		)
		.unwrap();
		assert!(e.validate().is_empty());
		e.new_blocks.remove(&real);
		let problems = e.validate();
		assert!(
			problems.iter().any(|p| p.contains("not an allocation")),
			"{problems:?}"
		);
		let _ = ghost;
	}

	/// Validation catches a gate that has opened while its dependency waits.
	#[test]
	fn validate_reports_an_open_gate_with_a_waiting_dependency() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(700) };
		// The inode has to be registered before anything may gate on it, or
		// validate is right to complain about the dangling gate instead.
		let id = e.new_inode(ino, crate::geom::CgNum::new(0));
		let dep = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		assert!(e.validate().is_empty(), "{:?}", e.validate());

		// Satisfying the gate settles the dependency, which is what normally
		// happens and is why this state cannot be reached through the API.  It
		// *can* be reached by a future change that opens a gate and forgets to
		// settle, which is exactly what validate is here to catch.
		e.note_inode_bitmap_written(id).unwrap();
		e.note_inode_written(id).unwrap();
		assert!(e.validate().is_empty());
		e.deps.get_mut(&dep).expect("still there").resolved = false;
		e.waiting_on.insert(Gate::InodeWritten(ino), vec![dep]);
		let problems = e.validate();
		assert!(
			problems.iter().any(|p| p.contains("already open")),
			"{problems:?}"
		);
	}

	/// Validation catches an allocation that completed and was never pruned.
	#[test]
	fn validate_reports_an_unpruned_completed_allocation() {
		let (mut e, _cache, _dev) = setup();
		let id = e.new_block(100, crate::geom::CgNum::new(0));
		e.note_bitmap_written(id).unwrap();
		e.note_contents_written(id).unwrap();
		e.note_pointer_published(id).unwrap();
		assert!(e.new_blocks.is_empty(), "the engine pruned it");

		// Put a completed one back, as a bug elsewhere would.
		e.new_blocks.insert(
			id,
			NewBlockDep {
				id,
				blk: 100,
				cg: crate::geom::CgNum::new(0),
				bitmap: true,
				content: true,
				publish: true,
				complete: true,
			},
		);
		assert!(e.validate().iter().any(|p| p.contains("never pruned")));
	}

	/// The registration invariant, in both directions.
	///
	/// `deps` and `waiting_on` are two representations of the same fact, and an
	/// unresolved dependency must appear in exactly one `waiting_on` entry -- the
	/// one keyed by its own gate -- while a resolved one must appear in none.
	/// These build each of the four shapes `validate()` distinguishes and check
	/// that it is reported, so the diagnostic cannot rot.
	fn dep(blk: u64, off: u64, len: u64) -> (u64, u64, u64) {
		(blk, off, len)
	}

	#[test]
	fn registration_invariant_is_reported_in_every_corrupt_shape() {
		// Case A: published, but the entry survived.
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(700) };
		let id = e.new_inode(ino, crate::geom::CgNum::new(0));
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		e.note_inode_bitmap_written(id).unwrap();
		e.note_inode_written(id).unwrap();
		assert!(
			e.validate().is_empty(),
			"the healthy shape is clean: {:?}",
			e.validate()
		);
		// Put the entry back by hand.
		e.waiting_on.insert(Gate::InodeWritten(ino), vec![d]);
		assert!(
			e.validate().iter().any(|p| p.contains("case A")),
			"{:?}",
			e.validate()
		);

		// Case E: unresolved and listed nowhere, so nothing will publish it.
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(701) };
		e.new_inode(ino, crate::geom::CgNum::new(0));
		e.gate(
			&mut cache,
			DepKind::DirectoryAdd,
			4,
			0,
			8,
			Gate::InodeWritten(ino),
		)
		.unwrap();
		e.waiting_on.remove(&Gate::InodeWritten(ino));
		assert!(
			e.validate().iter().any(|p| p.contains("case E")),
			"{:?}",
			e.validate()
		);
		let _ = dep(4, 0, 8);

		// Case C: listed under a gate that is not its own.
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(702) };
		let other = unsafe { InodeNum::new(703) };
		e.new_inode(ino, crate::geom::CgNum::new(0));
		e.new_inode(other, crate::geom::CgNum::new(0));
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		e.waiting_on.remove(&Gate::InodeWritten(ino));
		e.waiting_on.insert(Gate::InodeWritten(other), vec![d]);
		assert!(
			e.validate().iter().any(|p| p.contains("case C")),
			"{:?}",
			e.validate()
		);

		// Case D: the same dependency listed twice.
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(704) };
		e.new_inode(ino, crate::geom::CgNum::new(0));
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap();
		e.waiting_on.insert(Gate::InodeWritten(ino), vec![d, d]);
		assert!(
			e.validate().iter().any(|p| p.contains("case D")),
			"{:?}",
			e.validate()
		);

		// Case F: listed, but no longer a dependency.
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(705) };
		e.waiting_on
			.insert(Gate::InodeWritten(ino), vec![DepId(9999)]);
		assert!(
			e.validate().iter().any(|p| p.contains("case F")),
			"{:?}",
			e.validate()
		);
	}

	/// Case B: waiting under its own gate, with the gate open.  That is the
	/// shape that would mean "the gate opened and nobody published it", and it is
	/// the one worth being able to recognise.
	#[test]
	fn registration_invariant_reports_an_open_gate_with_a_waiter() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(706) };
		let id = e.new_inode(ino, crate::geom::CgNum::new(0));
		e.gate(
			&mut cache,
			DepKind::DirectoryAdd,
			4,
			0,
			8,
			Gate::InodeWritten(ino),
		)
		.unwrap();
		e.note_inode_bitmap_written(id).unwrap();
		e.note_inode_written(id).unwrap();
		assert!(
			e.unresolved().eq(&0),
			"publishing removed it from waiting, as it should"
		);
	}

	/// Both orderings, for both gate families: the dependency may be created
	/// before its gate opens, or after it is already open.
	#[test]
	fn both_orderings_resolve_for_both_gate_families() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();

		// Allocation gate, dependency first.
		let alloc = e.new_block(100, crate::geom::CgNum::new(0));
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				4,
				0,
				8,
				Gate::AllocationSafe(alloc),
			)
			.unwrap();
		assert!(!e.is_resolved(d), "the allocation is not ready yet");
		e.note_bitmap_written(alloc).unwrap();
		e.note_contents_written(alloc).unwrap();
		assert!(e.is_resolved(d), "both halves landed");
		assert!(e.validate().is_empty());

		// Allocation gate, dependency after the gate is already open.
		let alloc = e.new_block(200, crate::geom::CgNum::new(0));
		e.note_bitmap_written(alloc).unwrap();
		e.note_contents_written(alloc).unwrap();
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				4,
				8,
				8,
				Gate::AllocationSafe(alloc),
			)
			.unwrap();
		assert!(e.is_resolved(d), "gate() must publish an already-open gate");
		assert!(e.validate().is_empty());

		// Inode gate, dependency first.
		let inr = unsafe { InodeNum::new(800) };
		let i = e.new_inode(inr, crate::geom::CgNum::new(0));
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				16,
				4,
				Gate::InodeWritten(inr),
			)
			.unwrap();
		assert!(!e.is_resolved(d));
		e.note_inode_bitmap_written(i).unwrap();
		assert!(!e.is_resolved(d), "the image has not landed");
		e.note_inode_written(i).unwrap();
		assert!(e.is_resolved(d));
		assert!(e.validate().is_empty());

		// Inode gate, dependency after the gate is already open.
		let inr = unsafe { InodeNum::new(801) };
		let i = e.new_inode(inr, crate::geom::CgNum::new(0));
		e.note_inode_bitmap_written(i).unwrap();
		e.note_inode_written(i).unwrap();
		let d = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				20,
				4,
				Gate::InodeWritten(inr),
			)
			.unwrap();
		assert!(e.is_resolved(d), "gate() must publish an already-open gate");
		assert!(e.validate().is_empty());
	}

	/// The full allocation lifecycle, ending with the allocation retired.
	///
	/// Retirement is where a `Gate::AllocationSafe(DepId)` can no longer tell
	/// "not ready" from "already done": both look like an absent `NewBlockDep`.
	/// This test documents that, and the one after it says what follows from it.
	#[test]
	fn the_full_allocation_lifecycle() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let alloc = e.new_block(100, crate::geom::CgNum::new(0));
		assert_eq!(e.allocation_state(alloc), Some(AllocationState::New));

		let d = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				4,
				0,
				8,
				Gate::AllocationSafe(alloc),
			)
			.unwrap();
		e.note_bitmap_written(alloc).unwrap();
		e.note_contents_written(alloc).unwrap();
		assert!(e.is_resolved(d));

		e.note_pointer_published(alloc).unwrap();
		assert!(
			!e.new_blocks.contains_key(&alloc),
			"a completed allocation is retired"
		);
		assert!(!e.gate_is_open(Gate::AllocationSafe(alloc)));
		assert!(e.validate().is_empty());
	}

	/// The lifecycle hole that retirement leaves, stated as a fact rather than a
	/// fix.
	///
	/// A retired allocation and an allocation that has not started both look
	/// like "the gate is shut" to `gate_is_open`.  That is harmless while every
	/// dependency is created before its allocation completes -- which is the
	/// order `Ufs` uses -- and wrong the moment one is created afterwards.  This
	/// test pins the behaviour so that changing it has to be a deliberate act.
	#[test]
	fn a_retired_allocation_looks_the_same_as_an_unstarted_one() {
		let (mut e, _cache, _dev) = setup();

		// Never started: shut.
		let never = e.new_block(1, crate::geom::CgNum::new(0));
		assert!(!e.gate_is_open(Gate::AllocationSafe(never)));
		assert!(e.new_blocks.contains_key(&never));

		// Started, completed and retired: also shut.
		let done = e.new_block(2, crate::geom::CgNum::new(0));
		e.note_bitmap_written(done).unwrap();
		e.note_contents_written(done).unwrap();
		e.note_pointer_published(done).unwrap();
		assert!(
			!e.new_blocks.contains_key(&done),
			"the completed allocation is retired"
		);
		assert!(
			!e.gate_is_open(Gate::AllocationSafe(done)),
			"a retired allocation is indistinguishable from an unstarted one"
		);
	}

	/// A gate on a retired allocation is refused rather than accepted.
	///
	/// `note_pointer_published` removes a completed `NewBlockDep`, after which a
	/// retired allocation and one that never started are indistinguishable: both
	/// are simply absent from `new_blocks`, so `gate_is_open` says "shut" for
	/// both.  A dependency created in that window would be held back for the life
	/// of the mount.
	///
	/// `Ufs` cannot reach that window -- it creates a pointer dependency before
	/// the allocation can complete -- but nothing in the engine enforced that,
	/// so a future change to the ordering would have produced a hang rather
	/// than an error.  Refusing turns it into an error at the point of the
	/// mistake.
	#[test]
	fn a_gate_on_a_retired_allocation_is_refused() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();

		// Retire an allocation: allocate it, make it safe, publish its pointer.
		let alloc = e.new_block(100, CgNum::new(0));
		e.note_bitmap_written(alloc).unwrap();
		e.note_contents_written(alloc).unwrap();
		e.note_pointer_published(alloc).unwrap();
		assert!(!e.new_blocks.contains_key(&alloc), "it is retired");

		// A dependency on it now would never open.
		let err = e
			.gate(
				&mut cache,
				DepKind::DirectPointer,
				4,
				0,
				8,
				Gate::AllocationSafe(alloc),
			)
			.unwrap_err();
		assert!(err.to_string().contains("retired"), "{err}");
		assert_eq!(e.unresolved(), 0, "nothing was registered");
		assert!(e.validate().is_empty(), "{:?}", e.validate());

		// And a dependency on an inode that was never allocated, likewise.
		let ino = unsafe { InodeNum::new(901) };
		let err = e
			.gate(
				&mut cache,
				DepKind::DirectoryAdd,
				4,
				0,
				8,
				Gate::InodeWritten(ino),
			)
			.unwrap_err();
		assert!(err.to_string().contains("never registered"), "{err}");
	}

	/// A dependency that can never resolve has to be visible, or a range gated on
	/// it is held back for the life of the mount with nothing saying why.
	///
	/// `Gate::InodeLinkCounted` has no producer today: it protects a counter, and
	/// a counter cannot be byte-range gated.  Nothing in the filesystem raises it,
	/// which is exactly why `validate()` has to name it if one ever does.
	#[test]
	fn a_dependency_that_can_never_resolve_is_reported() {
		let (mut e, mut cache, mut dev) = setup();
		cache.get(&mut dev, 4).unwrap();
		let ino = unsafe { InodeNum::new(900) };
		e.gate(
			&mut cache,
			DepKind::DirectoryRemove,
			4,
			0,
			8,
			Gate::InodeLinkCounted(ino),
		)
		.unwrap();
		let problems = e.validate();
		assert!(
			problems.iter().any(|p| p.contains("nothing ever opens")),
			"{problems:?}"
		);
		assert_eq!(e.unresolved(), 1, "the dependency is still waiting");
		assert!(
			cache.peek(4).unwrap().has_unsafe_ranges(),
			"and the range is held"
		);
	}

	/// An operation enqueued when its container is *already* on the disk is
	/// runnable immediately, so a caller may enqueue unconditionally.
	#[test]
	fn an_operation_whose_container_is_already_persisted_runs_at_once() {
		let op = free_block(100, 7);
		assert!(op.is_runnable(true));
		let mut q = DeferredQueue::new();
		assert!(q.push(op));
		assert_eq!(q.take_runnable(|_| true).len(), 1);
		assert!(q.is_empty());
	}

	/// The inode operation carries the generation, so a reused inode number is
	/// distinguishable in the queue and in its label.
	#[test]
	fn an_inode_operation_is_keyed_by_generation() {
		let old = free_inode(14, 7, 9);
		let new = free_inode(14, 8, 9);
		assert_ne!(old.key(), new.key());
		assert_eq!(old.key(), OpKey::FreeInode(unsafe { InodeNum::new(14) }, 7));
		assert_eq!(old.label(), "free-inode/14@7");
	}
}
