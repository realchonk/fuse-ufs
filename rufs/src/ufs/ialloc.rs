use std::mem::replace;

use super::*;
use crate::{
	err,
	policy::BlockRole,
	softdep::{DepKind, Gate},
	InodeNum,
};

const STAT_BLKSIZE: u64 = 512;

/// The allocation role a request implies, given the inode it belongs to.
///
/// Indirect blocks always take the [`BlockRole::Indirect`] role; data blocks
/// follow the inode's type, which is the distinction that makes a directory's
/// blocks land in the metadata zone while a file's land in the data zone.
fn role_for(ino: &Inode, role: BlockRole) -> BlockRole {
	match role {
		BlockRole::Indirect { .. } => role,
		_ if ino.kind() == InodeType::Directory => BlockRole::DirectoryData,
		_ => BlockRole::FileData,
	}
}

impl<R: Backend> Ufs<R> {
	fn inode_setup(&mut self, inr: InodeNum, ino: &mut Inode) -> IoResult<()> {
		log::trace!("inode_setup({inr});");
		let inp = self.superblock.ino_to_fso(inr);
		// Straight out of the cached inode block, at the two offsets the struct
		// wants them: `di_nlink` at 0 and `di_gen` at 78.  Read raw rather than
		// through a whole `Inode`, because this is an inode of unknown vintage
		// whose layout may not match `struct ufs2_dinode`.
		let old_nlink: u16 = self.metadata_read(inp + 2)?;
		let old_gen: u32 = self.metadata_read(inp + 80)?;

		if old_nlink != 0 {
			log::error!("inode_setup({inr}): use after free");
			if let Ok(ino) = self.read_inode(inr) {
				log::error!("inode_setup({inr}): ino={ino:#?}");
			}
			return Err(err!(EFAULT));
		}
		assert_eq!(ino.nlink, 0);

		ino.gen = old_gen + 1;
		ino.nlink = 1;
		self.write_inode(inr, ino)?;
		// Read it back through the cache: the inode has to be visible to the
		// running filesystem immediately, before anything has been persisted.
		let _ = self.read_inode(inr)?;
		Ok(())
	}

	/// Allocate an inode, placing it according to the UFS2 policy.
	///
	/// `parent` is the directory the new inode is being created in, or `None`
	/// when there is no parent (recovery).  It decides the cylinder group:
	///
	/// * a directory uses the dirpref scheme ([`crate::policy::pref_inode`]),
	///   which spreads a filesystem's top-level directories but clusters deep
	///   ones near their parent;
	/// * anything else simply follows its parent, which keeps a directory and
	///   the files directly inside it in one cylinder group.
	///
	/// Both may fall back to any cylinder group with space, through the same
	/// cylinder-overflow search the block allocator uses.
	pub(super) fn inode_alloc(
		&mut self,
		parent: Option<InodeNum>,
		ino: &mut Inode,
	) -> IoResult<InodeNum> {
		self.assert_rw()?;
		let is_dir = ino.kind() == InodeType::Directory;

		let parent_info = match parent {
			Some(pinr) => {
				let pino = self.read_inode(pinr)?;
				Some(crate::policy::ParentInfo {
					inr:   pinr,
					// A parent that does not record its depth (an image made
					// before i_dirdepth was tracked) reads as depth 0, which
					// the policy treats as "untracked" and falls back to the
					// parent's cylinder group.
					depth: pino.dir_depth().unwrap_or(0),
					nlink: pino.nlink,
				})
			}
			None => None,
		};

		let pref_cg = crate::policy::pref_inode(
			&self.superblock,
			crate::policy::InodePrefInput {
				parent: parent_info,
				is_dir,
				cgs: &self.cg_sums,
				alloc: &self.alloc,
			},
		);

		// A directory's depth must be in the inode before it is written, so
		// that the very first version of the inode on disk already carries it.
		if is_dir {
			let depth = parent_info.map_or(1, |p| p.depth + 1);
			ino.set_dir_depth(depth);
		}

		let inr = self.hash_alloc_inode(pref_cg, None)?.ok_or(err!(ENOSPC))?;
		log::trace!("inode_alloc(): {inr}");

		self.inode_setup(inr, ino)?;

		// The directory count is what makes the dirpref policy self-limiting:
		// is the denominator of "how much of this cylinder group is already
		// spoken for by directories".
		if is_dir {
			let cg = self.superblock.ino_to_cg(inr);
			let mut cgd = self.read_cg(cg)?;
			cgd.cs.ndir += 1;
			self.write_cg(cg, &cgd)?;
			self.update_sb(|sb| sb.cstotal.ndir += 1)?;
		}
		self.note_inode_alloc(self.superblock.ino_to_cg(inr), is_dir);

		Ok(inr)
	}

	/// Read a whole indirect block.
	///
	/// An indirect block is an unframed array of `ufs2_daddr_t`, so this pulls
	/// its bytes out of the metadata cache and converts them one entry at a
	/// time with the image's byte order.  Through the cache rather than the
	/// decoder because Soft Updates may be holding a gated entry of this very
	/// block back, and a caller asking where a file currently maps has to be
	/// answered from the live image.
	pub(super) fn read_pblock(&mut self, bno: u64, block: &mut [u64]) -> IoResult<()> {
		let fs = self.superblock.fsize as u64;
		let bs = self.superblock.bsize as usize;
		let pbp = bs / size_of::<u64>();

		assert_eq!(block.len(), pbp);

		let raw = self.metadata_read_at(bno * fs, pbp * size_of::<UfsDaddr>())?;
		let config = self.file.config();
		for (n, slot) in block.iter_mut().enumerate() {
			let at = n * size_of::<UfsDaddr>();
			let mut bytes = [0u8; size_of::<UfsDaddr>()];
			bytes.copy_from_slice(&raw[at..][..size_of::<UfsDaddr>()]);
			*slot = config.u64_from_bytes(&bytes);
		}
		Ok(())
	}

	/// Stage a whole indirect block.
	///
	/// The counterpart of [`Self::read_pblock`], staging rather than writing for
	/// the same reason: Soft Updates decides when this block's bytes may reach
	/// the disk, entry by entry.
	pub(super) fn write_pblock(&mut self, bno: u64, block: &[u64]) -> IoResult<()> {
		let fs = self.superblock.fsize as u64;
		let bs = self.superblock.bsize as usize;
		let pbp = bs / size_of::<u64>();

		assert_eq!(block.len(), pbp);

		let config = self.file.config();
		let mut raw = vec![0u8; pbp * size_of::<u64>()];
		for (n, v) in block.iter().enumerate() {
			let at = n * size_of::<u64>();
			raw[at..][..size_of::<u64>()].copy_from_slice(&config.u64_to_bytes(*v));
		}
		self.metadata_write_at(bno * fs, &raw)
	}

	/// Byte offset of entry `idx` of the indirect block at `bno`.
	pub(super) fn indir_off(&self, bno: u64, idx: u64) -> u64 {
		bno * self.superblock.fsize as u64 + idx * size_of::<UfsDaddr>() as u64
	}

	/// Read one entry of an indirect block.
	fn indir_get(&mut self, bno: u64, idx: u64) -> IoResult<u64> {
		self.metadata_read(self.indir_off(bno, idx))
	}

	/// Stage one entry of an indirect block.
	///
	/// This is where a *single* indirect pointer becomes visible, and therefore
	/// where it will eventually be gated on the block it names: Soft Updates may
	/// not let the entry out until that block is safely allocated and
	/// initialised.  For now it is an ordinary dirty-buffer write, which is why
	/// the block stays visible to readers through the cache.
	fn indir_set(&mut self, bno: u64, idx: u64, val: u64) -> IoResult<()> {
		self.metadata_write(self.indir_off(bno, idx), &(val as UfsDaddr))
	}

	/// Stage one entry of an indirect block and gate it on the allocation of the
	/// block it names.
	///
	/// This is the middle link of the chain a large file builds:
	///
	/// ```text
	///   data block
	///        |
	///        v
	///   indirect entry     <- gated here, on the data block's allocation
	///        |
	///        v
	///   indirect block     <- itself gated on its allocation, by the caller
	///        |
	///        v
	///   inode pointer      <- gated on the indirect block's allocation
	/// ```
	///
	/// Each link is gated independently rather than the whole indirect block
	/// being held back, and that is what the safe image is for.  An indirect
	/// block with one new entry can persist as `[A B 0 D]`: the entries that
	/// were already safe, the new one as a hole, the rest of the block's old
	/// contents.  Blocking the whole block would also be wrong rather than
	/// merely slow -- it would delay entries that have nothing to do with the
	/// allocation in flight.
	///
	/// Only a *non-zero* entry is gated.  Writing zero is a removal, and there
	/// is nothing there for a stale pointer to reach.
	pub(super) fn indir_set_gated(&mut self, bno: u64, idx: u64, val: u64) -> IoResult<()> {
		self.indir_set(bno, idx, val)?;
		let Some(dep) = self.allocation_of(val) else {
			return Ok(());
		};
		let (blk, off) = self.indir_range(bno, idx);
		log::trace!("gating indirect entry {bno}:{idx} at {blk}:{off} on {dep:?}");
		self.softdep.gate(
			&mut self.buf,
			DepKind::IndirectPointer,
			blk,
			off,
			size_of::<UfsDaddr>() as u64,
			Gate::AllocationSafe(dep),
		)?;
		Ok(())
	}

	/// The cache block and in-block byte offset of entry `idx` of the indirect
	/// block at `bno`.
	///
	/// An indirect block is `fs_bsize` bytes of `ufs2_daddr_t`, addressed by a
	/// UFS block -- that is, a *fragment* address -- so its entries live in one
	/// cache block and the offset is just `entry * sizeof(ufs2_daddr_t)`.  The
	/// block has to be `fs_bsize`-aligned for that to hold, which is the same
	/// assumption `Ufs::read_pblock` makes when it refuses to read one that is
	/// not.
	pub(super) fn indir_range(&self, bno: u64, idx: u64) -> (u64, u64) {
		let bs = self.superblock.bsize();
		let at = bno * self.superblock.fsize();
		(at / bs, at % bs + idx * size_of::<UfsDaddr>() as u64)
	}

	/// Free the sub-tree behind a first-level indirect block.
	///
	/// `inode_blk` is the cache block holding the inode, which is the container
	/// for `bno` itself.  Entries inside `bno` have `bno` as their container,
	/// which is exactly the chain the ordering rule asks for: a child is
	/// released only after the indirect block that named it is on the disk, and
	/// the indirect block is released only after the inode is.
	fn inode_free_l1(
		&mut self,
		ino: &mut Inode,
		bno: u64,
		block: &mut [u64],
		inode_blk: u64,
	) -> IoResult<()> {
		if bno == 0 {
			return Ok(());
		}
		let l1_blk = self.metadata_blk(bno);

		self.read_pblock(bno, block)?;

		let (blocks, frags) = ino.size(self.superblock.bsize(), self.superblock.fsize());
		let end = blocks + frags.min(1);

		for (idx, bno) in block.iter().enumerate() {
			if *bno == 0 {
				continue;
			}
			// Entries past the end of the file should not exist in a
			// consistent filesystem, but they are cheap to tolerate and
			// `inode_get_block_size()` would otherwise panic.  Such a block
			// is still ours to free: it is inside an indirect block that this
			// inode owns.
			let lbn = self.inode_l1_offset(ino, idx as u64);
			if lbn < end {
				let size = self.inode_get_block_size(ino, lbn);
				self.inode_free_block(ino, *bno, size as u64, l1_blk)?;
			} else {
				log::warn!(
					"inode_free_l1: indirect block holds a pointer at lbn {lbn}, \
					 past the end of the file ({end} blocks); freeing it whole"
				);
				self.inode_free_block(ino, *bno, self.superblock.bsize(), l1_blk)?;
			}
		}

		self.blk_free(bno, self.superblock.bsize as u64, inode_blk)?;

		Ok(())
	}

	/// Logical block number of entry `idx` of a single-indirect block.
	fn inode_l1_offset(&self, _ino: &Inode, idx: u64) -> u64 {
		UFS_NDADDR as u64 + idx
	}

	fn inode_free_l2(
		&mut self,
		ino: &mut Inode,
		bno: u64,
		block: &mut [u64],
		inode_blk: u64,
	) -> IoResult<()> {
		if bno == 0 {
			return Ok(());
		}
		let l2_blk = self.metadata_blk(bno);

		self.read_pblock(bno, block)?;
		let indir = block.to_owned();

		for bno in indir {
			self.inode_free_l1(ino, bno, block, l2_blk)?;
		}

		self.blk_free(bno, self.superblock.bsize as u64, inode_blk)?;

		Ok(())
	}

	fn inode_free_l3(
		&mut self,
		ino: &mut Inode,
		bno: u64,
		block: &mut [u64],
		inode_blk: u64,
	) -> IoResult<()> {
		if bno == 0 {
			return Ok(());
		}
		let l3_blk = self.metadata_blk(bno);

		self.read_pblock(bno, block)?;
		let indir = block.to_owned();

		for bno in indir {
			self.inode_free_l2(ino, bno, block, l3_blk)?;
		}

		self.blk_free(bno, self.superblock.bsize as u64, inode_blk)?;

		Ok(())
	}

	/// Increment the reference count of the Inode `inr`.
	pub(super) fn inode_bump(&mut self, inr: InodeNum) -> IoResult<()> {
		self.assert_rw()?;
		let mut ino = self.read_inode(inr)?;
		ino.nlink += 1;
		self.write_inode(inr, &ino)?;
		Ok(())
	}

	/// Decrement the reference count of the Inode `inr`
	/// and delete the inode, if it hits 0.
	pub(super) fn inode_free(&mut self, inr: InodeNum) -> IoResult<()> {
		self.assert_rw()?;
		let mut ino = self.read_inode(inr)?;
		ino.nlink -= 1;
		self.write_inode(inr, &ino)?;

		if ino.nlink > 0 {
			return Ok(());
		}

		let is_dir = ino.kind() == InodeType::Directory;

		// Release the inode's blocks *before* its bitmap entry.  The order
		// matters after a crash: an inode that is still marked used in
		// cg_iused[] is visible to fsck phase 4, which will follow its
		// pointers and free them, whereas a block whose inode has vanished is
		// simply lost space.
		if let InodeData::Blocks(blocks) = ino.data.clone() {
			let bs = self.superblock.bsize as u64;
			let mut block = vec![0u64; bs as usize / size_of::<u64>()];
			// Every pointer being removed here lives in the inode, so the inode's
			// block is the container for all of them -- the direct blocks and the
			// three indirect blocks alike.
			let inode_blk = self.metadata_blk(self.superblock.ino_to_fsba(inr));

			for i in 0..UFS_NDADDR {
				let bno = blocks.direct[i] as u64;
				if bno == 0 {
					continue;
				}
				let size = self.inode_get_block_size(&ino, i as u64);
				self.inode_free_block(&mut ino, bno, size as u64, inode_blk)?;
			}

			self.inode_free_l1(&mut ino, blocks.indirect[0] as u64, &mut block, inode_blk)?;
			self.inode_free_l2(&mut ino, blocks.indirect[1] as u64, &mut block, inode_blk)?;
			self.inode_free_l3(&mut ino, blocks.indirect[2] as u64, &mut block, inode_blk)?;
		}

		// Now the inode itself.  `i_blocks` goes to zero first so that a crash
		// between here and the bitmap clear leaves a self-consistent inode.
		//
		// `gen` is captured here because the clear below zeroes it, and it is the
		// only thing left that identifies *this* inode.
		let gen = ino.gen;
		ino.blocks = 0;
		self.write_inode(inr, &ino)?;

		let off = self.superblock.ino_to_fso(inr);
		// Clearing an inode is metadata like any other write: through the cache,
		// so it becomes a dirty buffer that Soft Updates can order behind the
		// removal of the directory entries and the data pointers, instead of a
		// device write that lands immediately.
		self.metadata_fill_at(off, 0u8, UFS_INOSZ)?;

		// ...and it is held back until the directory entry that named this
		// inode is *persistently* gone.
		//
		// Without this, a crash between the two leaves a directory listing a
		// file whose inode is all zeroes: `fsck` pass 2 resolves that by clearing
		// the entry, and pass 4 then frees the inode's blocks -- a directory
		// entry to a file whose data is still sitting on the disk.  Zeroing an
		// inode before its last name disappears is the removal-side twin of
		// letting an entry appear before the inode does.
		//
		// The gate is on the *cleared image* rather than on the operations after
		// it, because that is what the byte-range model can express.  Freeing the
		// blocks and clearing the inode bitmap bit stay immediate; both are safe
		// on their own, because an inode nobody can reach whose blocks have been
		// returned to the free list is lost space, whereas an inode nobody can
		// reach that is still *listed* is a dangling entry.  That asymmetry is
		// the same one the comment above `inode_free`'s block walk relies on.
		if let Some(gate) = self.removal_gate(inr) {
			let blk = self.metadata_blk(self.superblock.ino_to_fsba(inr));
			log::trace!("gating the cleared image of {inr} at {blk}");
			self.softdep.gate(
				&mut self.buf,
				crate::softdep::DepKind::DirectoryRemove,
				blk,
				off % self.superblock.bsize(),
				UFS_INOSZ as u64,
				gate,
			)?;
		}

		// One operation releases the inode *and* the cylinder group's directory
		// count, because they describe the same set of live inodes and
		// `check_consistency()` compares each against the bitmap.  `free_cg_dir`
		// was a second step here and could drift from the bit; it is now part of
		// the release.
		//
		// `gen` is the inode's generation, captured before the clear above
		// zeroed it.  It is the only thing that stops a release queued for this
		// inode being applied to whatever later takes the same number.
		self.free_cg_inode(inr, gen, is_dir)?;

		Ok(())
	}

	/// Remove every block past `new_size`.
	///
	/// The inode's own block is the container for every pointer removed from the
	/// inode itself; deeper containers are the indirect blocks, computed where
	/// the entry was removed.
	fn inode_shrink(&mut self, inr: InodeNum, ino: &mut Inode, new_size: u64) -> IoResult<()> {
		let inode_blk = self.metadata_blk(self.superblock.ino_to_fsba(inr));
		let (begin_indir1, begin_indir2, begin_indir3, _) = self.inode_data_zones();
		let sb = &self.superblock;
		let bs = sb.bsize();
		let fs = sb.fsize();
		let (blocks, frags) = Inode::inode_size(bs, fs, new_size);
		log::trace!("inode_shrink(): blocks={blocks}, frags={frags}");
		let blocks = blocks + (frags > 0) as u64;
		let pbp = bs / size_of::<u64>() as u64;

		let InodeData::Blocks(mut iblocks) = ino.data.clone() else {
			return Err(err!(EINVAL));
		};

		let mut block = vec![0u64; bs as usize / size_of::<u64>()];

		if blocks >= begin_indir3 {
			let used = blocks - begin_indir3;
			self.read_pblock(iblocks.indirect[2] as u64, &mut block)?;
			let mut fst = block.clone();

			let off1 = used / pbp / pbp;
			let off2 = used / pbp % pbp;
			let off3 = used % pbp;

			// handle the first table separately, as it only needs to be partially freed
			self.read_pblock(fst[off1 as usize], &mut block)?;
			let mut snd = block.clone();
			self.read_pblock(snd[off2 as usize], &mut block)?;
			for i in off3..pbp {
				let bno = replace(&mut block[i as usize], 0);
				if bno == 0 {
					continue;
				}
				let size = self
					.inode_get_block_size(ino, begin_indir3 + off3 * pbp * pbp + off2 * pbp + i);
				let snd_blk = self.metadata_blk(snd[off2 as usize]);
				self.inode_free_block(ino, bno, size as u64, snd_blk)?;
			}
			self.write_pblock(snd[off2 as usize], &block)?;
			let snd_blk = self.metadata_blk(snd[off2 as usize]);
			for i in (off2 + 1)..pbp {
				let bno = replace(&mut snd[i as usize], 0);
				self.inode_free_l1(ino, bno, &mut block, snd_blk)?;
			}
			let fst_blk = self.metadata_blk(fst[off1 as usize]);
			self.write_pblock(fst[off1 as usize], &snd)?;

			// the remaining tables can be freed completely
			for i in (off1 + 1)..pbp {
				let bno = replace(&mut fst[i as usize], 0);
				self.inode_free_l2(ino, bno, &mut block, fst_blk)?;
			}

			self.write_pblock(iblocks.indirect[2] as u64, &block)?;
			ino.data = InodeData::Blocks(iblocks);
			return Ok(());
		}

		self.inode_free_l3(
			ino,
			replace(&mut iblocks.indirect[2], 0) as u64,
			&mut block,
			inode_blk,
		)?;

		if blocks >= begin_indir2 {
			let used = blocks - begin_indir2;
			self.read_pblock(iblocks.indirect[1] as u64, &mut block)?;
			let mut fst = block.clone();

			let off1 = used / pbp;
			let off2 = used % pbp;

			// handle the first table specially, as it only needs to be partially freed
			self.read_pblock(fst[off1 as usize], &mut block)?;
			for i in off2..pbp {
				let bno = replace(&mut block[i as usize], 0);
				if bno == 0 {
					continue;
				}
				let size = self.inode_get_block_size(ino, begin_indir2 + off2 * pbp + i);
				let fst_blk = self.metadata_blk(fst[off1 as usize]);
				self.inode_free_block(ino, bno, size as u64, fst_blk)?;
			}
			self.write_pblock(fst[off1 as usize], &block)?;

			// the remaining tables can be freed completely
			let fst_blk = self.metadata_blk(fst[off1 as usize]);
			for i in (off1 + 1)..pbp {
				let bno = replace(&mut fst[i as usize], 0);
				self.inode_free_l1(ino, bno, &mut block, fst_blk)?;
			}

			self.write_pblock(iblocks.indirect[1] as u64, &fst)?;
			ino.data = InodeData::Blocks(iblocks);
			return Ok(());
		}

		self.inode_free_l2(
			ino,
			replace(&mut iblocks.indirect[1], 0) as u64,
			&mut block,
			inode_blk,
		)?;

		if blocks >= begin_indir1 {
			let used = blocks - begin_indir1;
			self.read_pblock(iblocks.indirect[0] as u64, &mut block)?;

			for i in used..pbp {
				let bno = replace(&mut block[i as usize], 0);
				if bno == 0 {
					continue;
				}
				let size = self.inode_get_block_size(ino, begin_indir1 + i);
				let l1_blk = self.metadata_blk(iblocks.indirect[0] as u64);
				self.inode_free_block(ino, bno, size as u64, l1_blk)?;
			}

			self.write_pblock(iblocks.indirect[0] as u64, &block)?;

			ino.data = InodeData::Blocks(iblocks);
			return Ok(());
		}

		self.inode_free_l1(
			ino,
			replace(&mut iblocks.indirect[0], 0) as u64,
			&mut block,
			inode_blk,
		)?;

		for i in (blocks as usize)..UFS_NDADDR {
			let bno = replace(&mut iblocks.direct[i], 0) as u64;
			if bno == 0 {
				continue;
			}
			let size = self.inode_get_block_size(ino, i as u64);
			self.inode_free_block(ino, bno, size as u64, inode_blk)?;
		}

		ino.data = InodeData::Blocks(iblocks);
		Ok(())
	}

	/// Truncate `inr` to `new_size` bytes.
	///
	/// This is the operation `truncate(2)` and `ftruncate(2)` map onto, so it
	/// enforces what POSIX requires of them.  The machinery below does not:
	/// `mkdir` sizes the directory it just created through
	/// [`Self::inode_truncate`], and must keep being able to.
	pub fn truncate(&mut self, inr: InodeNum, new_size: u64) -> IoResult<()> {
		log::trace!("truncate({inr}, {new_size});");
		self.assert_rw()?;

		if self.read_inode(inr)?.kind() == InodeType::Directory {
			// `EISDIR`, and here the wrong errno would be the smaller half of the
			// bug: truncating a directory to zero frees the blocks holding its
			// `.` and `..`, which are how the directory is found at all.  Found by
			// the randomised test, seed 12: `mkdir` followed by `truncate` left a
			// directory whose `.` named seven unrelated inodes, because its first
			// block had been handed back out and read as a directory.
			log::warn!("truncate({inr}): a directory cannot be truncated");
			return Err(err!(EISDIR));
		}

		self.inode_truncate(inr, new_size)
	}

	/// Resize `inr`, with no restriction on what it is.
	///
	/// Not the same as [`Self::truncate`]: this is the primitive, and `mkdir`
	/// uses it to give a new directory its first block.
	pub(super) fn inode_truncate(&mut self, inr: InodeNum, new_size: u64) -> IoResult<()> {
		log::trace!("inode_truncate({inr}, {new_size});");
		self.assert_rw()?;

		let mut ino = self.read_inode(inr)?;
		let old_size = ino.size;

		if new_size < old_size {
			self.inode_shrink(inr, &mut ino, new_size)?;
		}

		ino.size = new_size;

		self.write_inode(inr, &ino)?;

		Ok(())
	}

	/// Store `block` at `blkidx` in `ino`, allocating any indirect blocks the
	/// path needs on the way.
	///
	/// Any indirect block created here is allocated with
	/// [`BlockRole::Indirect`], which is what places the *first* one right
	/// after the last direct block and later ones in the metadata zone.
	/// The byte range of the `i`-th direct block pointer within `inr`'s inode.
	///
	/// `(offset, length)`, relative to the start of the inode.
	///
	/// This is what a Soft Updates gate is expressed in: the range of bytes a
	/// dependency may hold back.  It is derived from [`UFS_EXT_OFF`] rather than
	/// written out here, so there is exactly one place in the crate that knows
	/// where the pointers are, and `the_pointer_offsets_match_the_encoding` in
	/// `rufs/src/ufs/mapping.rs` checks it against the real encoder.
	pub(super) fn direct_pointer_range(&self, i: usize) -> (u64, u64) {
		(
			UFS_EXT_OFF as u64 + i as u64 * size_of::<UfsDaddr>() as u64,
			size_of::<UfsDaddr>() as u64,
		)
	}

	/// The byte range of the `i`-th indirect block pointer.
	///
	/// See [`Self::direct_pointer_range`].
	pub(super) fn indirect_pointer_range(&self, i: usize) -> (u64, u64) {
		(
			UFS_EXTB_OFF as u64 + i as u64 * size_of::<UfsDaddr>() as u64,
			size_of::<UfsDaddr>() as u64,
		)
	}

	/// The byte range of the pointer at logical index `idx`, wherever it lives.
	///
	/// The logical index is `blkidx`, the same numbering
	/// [`Ufs::inode_resolve_block`] takes: `0..UFS_NDADDR` are the direct
	/// pointers and the rest index `di_extb`.  `None` for an index past the end,
	/// which cannot happen for a block index that [`Ufs::decode_blkidx`]
	/// accepted, and is not silently rounded into some neighbouring slot.
	pub(super) fn pointer_range(&self, idx: u64) -> Option<(u64, u64)> {
		if idx < UFS_NDADDR as u64 {
			Some(self.direct_pointer_range(idx as usize))
		} else if idx < (UFS_NDADDR + UFS_NIADDR) as u64 {
			Some(self.indirect_pointer_range((idx - UFS_NDADDR as u64) as usize))
		} else {
			None
		}
	}

	/// Where the pointer at logical index `idx` of `inr` lives, as
	/// `(cache block, offset in that block, length)`.
	///
	/// This is the form a Soft Updates gate needs, and it is derived from the
	/// inode's *byte address* rather than from `ino_to_fsba()` alone.  That
	/// distinction is not cosmetic: `ino_to_fso()` is
	/// `ino_to_fsba() * fs_fsize + ino_to_fsbo() * UFS_INOSZ`, because an inode
	/// block holds `fs_inopb` inodes and each one starts `UFS_INOSZ` bytes in.
	/// A gate computed from `ino_to_fsba()` alone marks the *first* inode's
	/// pointer unsafe while writing a later inode's -- which corrupts an inode
	/// nobody asked about and leaves the one actually being modified ungated.
	pub(super) fn inode_pointer_range(&self, inr: InodeNum, idx: u64) -> Option<(u64, u64, u64)> {
		let (off, len) = self.pointer_range(idx)?;
		let sb = &self.superblock;
		let at = sb.ino_to_fso(inr);
		Some((at / sb.bsize(), off + at % sb.bsize(), len))
	}

	fn inode_set_block(
		&mut self,
		inr: InodeNum,
		ino: &mut Inode,
		blkidx: u64,
		block: NonZeroU64,
	) -> IoResult<()> {
		let InodeData::Blocks(InodeBlocks { direct, indirect }) = &mut ino.data else {
			log::warn!(
				"inode_set_block({inr}, {blkidx}, {block}): inode doesn't haave data blocks"
			);
			return Err(err!(EIO));
		};

		let last_direct = direct[UFS_NDADDR - 1] as u64;
		let first_indirect = indirect[0] as u64;
		let mut wb = false;
		// Which slot of `di_ext` this call may have just filled, and with which
		// block.  `None` until it actually writes one: an indirect *entry* lives
		// in an indirect block, not in the inode, and the inode must not be
		// written or gated for it.  Assigned in each arm that fills a slot.
		#[allow(unused_assignments)]
		let mut inode_pointer: Option<(usize, u64)> = None;

		match self.decode_blkidx(blkidx)? {
			InodeBlock::Direct(off) => {
				direct[off] = block.get() as i64;
				wb = true;
				inode_pointer = Some((off, block.get()));
			}
			InodeBlock::Indirect1(off) => {
				if indirect[0] == 0 {
					indirect[0] = self
						.blk_alloc_zeroed_for(
							BlockRole::Indirect { first: true },
							inr,
							blkidx,
							0,
							last_direct,
							first_indirect,
						)?
						.get() as i64;
					wb = true;
				}
				inode_pointer = Some((UFS_NDADDR, indirect[0] as u64));

				self.indir_set_gated(indirect[0] as u64, off as u64, block.get())?;
			}
			InodeBlock::Indirect2(high, low) => {
				if indirect[1] == 0 {
					indirect[1] = self
						.blk_alloc_zeroed_for(
							BlockRole::Indirect { first: false },
							inr,
							blkidx,
							0,
							last_direct,
							first_indirect,
						)?
						.get() as i64;
					wb = true;
				}
				inode_pointer = Some((UFS_NDADDR + 1, indirect[1] as u64));

				let x1 = indirect[1] as u64;
				let x2 = self.indir_get(x1, high as u64)?;
				let x2 = if x2 == 0 {
					let b = self.blk_alloc_zeroed_for(
						BlockRole::Indirect { first: false },
						inr,
						blkidx,
						0,
						last_direct,
						first_indirect,
					)?;
					self.indir_set(x1, high as u64, b.get())?;
					b.get()
				} else {
					x2
				};
				self.indir_set_gated(x2, low as u64, block.get())?;
			}
			InodeBlock::Indirect3(high, mid, low) => {
				if indirect[2] == 0 {
					indirect[2] = self
						.blk_alloc_zeroed_for(
							BlockRole::Indirect { first: false },
							inr,
							blkidx,
							0,
							last_direct,
							first_indirect,
						)?
						.get() as i64;
					wb = true;
				}
				inode_pointer = Some((UFS_NDADDR + 2, indirect[2] as u64));

				let x1 = indirect[2] as u64;
				let x2 = self.indir_get(x1, high as u64)?;
				let x2 = if x2 == 0 {
					let b = self.blk_alloc_zeroed_for(
						BlockRole::Indirect { first: false },
						inr,
						blkidx,
						0,
						last_direct,
						first_indirect,
					)?;
					self.indir_set(x1, high as u64, b.get())?;
					b.get()
				} else {
					x2
				};
				let x3 = self.indir_get(x2, mid as u64)?;
				let x3 = if x3 == 0 {
					let b = self.blk_alloc_zeroed_for(
						BlockRole::Indirect { first: false },
						inr,
						blkidx,
						0,
						last_direct,
						first_indirect,
					)?;
					self.indir_set(x2, mid as u64, b.get())?;
					b.get()
				} else {
					x3
				};
				self.indir_set_gated(x3, low as u64, block.get())?;
			}
		}

		if wb {
			self.write_inode(inr, ino)?;
		}

		// Only *now* is the pointer in the inode's live image, and only now can
		// the range that holds it be gated.  Doing it in the other order would
		// either miss the write or gate bytes that do not hold a pointer yet.
		//
		// `Gate::AllocationSafe`, not `AllocationAllocated` or
		// `AllocationInitialised`: a pointer to a block whose bitmap bit has
		// landed but whose contents have not is exactly the failure this
		// dependency exists to prevent, and the other two gates would let it
		// through.
		if let Some((slot, blk)) = inode_pointer {
			self.gate_inode_pointer(inr, slot, blk)?;
		}

		Ok(())
	}

	/// Gate the slot of `di_ext` that was just filled with `blk`.
	///
	/// The inode pointer at logical index `idx` is `di_ext[idx]` when
	/// `idx < UFS_NDADDR` and `di_extb[idx - UFS_NDADDR]` above it; both are
	/// `Ufs::pointer_range`'s business, and this only decides which dependency
	/// kind the gate is filed under.
	fn gate_inode_pointer(&mut self, inr: InodeNum, idx: usize, blk: u64) -> IoResult<()> {
		let Some(dep) = self.allocation_of(blk) else {
			return Ok(());
		};
		let idx = idx as u64;
		let (range_blk, off, len) = self
			.inode_pointer_range(inr, idx)
			.ok_or_else(|| err!(EIO))?;
		let kind = if idx < UFS_NDADDR as u64 {
			DepKind::DirectPointer
		} else {
			// The pointer names an indirect block, not a data block, so it is
			// an indirect pointer even though it lives in the inode.  Collapsing
			// the two would make the documentation of `DepKind::IndirectPointer`
			// -- which promises that an indirect block is not published before
			// every entry it gained is safe -- untrue.
			DepKind::IndirectPointer
		};
		log::trace!("gating {inr}'s di_ext[{idx}] at {range_blk}:{off}+{len} on {dep:?}");
		self.softdep.gate(
			&mut self.buf,
			kind,
			range_blk,
			off,
			len,
			Gate::AllocationSafe(dep),
		)?;
		Ok(())
	}

	/// Allocate a block of file or directory data for `inr` and store it at
	/// `blkidx`.
	pub(super) fn inode_alloc_block(
		&mut self,
		inr: InodeNum,
		ino: &mut Inode,
		blkidx: u64,
		size: u64,
	) -> IoResult<(NonZeroU64, u64)> {
		let role = role_for(ino, BlockRole::FileData);

		// The policy needs to know where the previous logical block went (to
		// continue a contiguous run), where the last direct block is (so the
		// first indirect block can follow it) and where the first indirect
		// block is (so its first data block can follow it).
		let (prev, last_direct, first_indirect) = match &ino.data {
			InodeData::Blocks(b) => {
				let prev = if blkidx == 0 {
					0
				} else {
					self.inode_resolve_block(inr, ino, blkidx - 1)?
						.map_or(0, |b| b.get())
				};
				(prev, b.direct[UFS_NDADDR - 1] as u64, b.indirect[0] as u64)
			}
			InodeData::Shortlink(_) => (0, 0, 0),
		};

		log::trace!("inode_alloc_block({inr}): old_blocks: {}", ino.blocks);
		let block = self.blk_alloc_for(role, inr, blkidx, prev, last_direct, first_indirect)?;
		let block_size = self.superblock.bsize as u64;
		ino.blocks += block_size / STAT_BLKSIZE;
		log::trace!("inode_alloc_block({inr}): new_blocks: {}", ino.blocks);
		self.inode_set_block(inr, ino, blkidx, block)?;
		log::trace!(
			"inode_alloc_block({inr}, {blkidx}, {size}): block={block}, block_size={block_size}"
		);
		Ok((block, block_size))
	}

	/// Free one block of `ino`, whose pointer has just been removed.
	///
	/// `container` is the cache block the pointer lived in: the inode for a
	/// direct block, the enclosing indirect block for an indirect entry.  It is
	/// what `blk_free` waits for before releasing the allocation bit, because
	/// until it is on the disk something can still point at the block.
	fn inode_free_block(
		&mut self,
		ino: &mut Inode,
		bno: u64,
		_size: u64,
		container: u64,
	) -> IoResult<()> {
		// Allocation always takes a whole block (see the module docs of
		// `balloc`), so freeing must as well; `size` is kept in the signature
		// to document the caller's intent and to be used once fragment
		// allocation lands.
		self.blk_free(bno, self.superblock.bsize as u64, container)?;
		log::trace!("inode_free_block({bno}): old_blocks={}", ino.blocks);
		// Saturate rather than wrap: a block found outside the inode's size
		// (see `inode_free_l1`) was never charged to `i_blocks`, and
		// underflowing here would turn a recoverable inconsistency into a
		// wildly wrong block count.
		ino.blocks = ino
			.blocks
			.saturating_sub((self.superblock.bsize as u64) / STAT_BLKSIZE);
		Ok(())
	}
}
