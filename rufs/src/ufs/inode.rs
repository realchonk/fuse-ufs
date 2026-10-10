use std::ops::{Bound, RangeBounds};

use super::*;
use crate::{err, InodeNum};

impl<R: Backend> Ufs<R> {
	/// Get metadata about an inode.
	#[doc(alias("stat", "getattr"))]
	pub fn inode_attr(&mut self, inr: InodeNum) -> IoResult<InodeAttr> {
		log::trace!("inode_attr({inr});");
		let ino = self.read_inode(inr)?;
		Ok(ino.as_attr(inr))
	}

	/// Read data from an inode.
	pub fn inode_read(
		&mut self,
		inr: InodeNum,
		mut offset: u64,
		buffer: &mut [u8],
	) -> IoResult<usize> {
		log::trace!("inode_read({inr}, {offset}, {})", buffer.len());
		let mut blockbuf = vec![0u8; self.superblock.bsize as usize];
		let ino = self.read_inode(inr)?;

		let mut boff = 0;
		let len = (buffer.len() as u64).min(ino.size - offset);
		let end = offset + len;

		while offset < end {
			let block = self.inode_find_block(inr, &ino, offset);
			let num = (block.size - block.off).min(end - offset);

			self.inode_read_block(
				inr,
				&ino,
				block.blkidx,
				&mut blockbuf[0..(block.size as usize)],
			)?;
			let off = block.off as usize;
			buffer[boff..(boff + num as usize)]
				.copy_from_slice(&blockbuf[off..(off + num as usize)]);

			offset += num;
			boff += num as usize;
		}

		Ok(boff)
	}

	pub fn inode_write(
		&mut self,
		inr: InodeNum,
		mut offset: u64,
		buffer: &[u8],
	) -> IoResult<usize> {
		log::trace!("inode_write({inr}, {offset}, {})", buffer.len());
		self.assert_rw()?;

		let mut blockbuf = vec![0u8; self.superblock.bsize as usize];
		let mut ino = self.read_inode(inr)?;
		ino.size = ino.size.max(offset + buffer.len() as u64);
		self.write_inode(inr, &ino)?;

		let mut boff = 0;
		let len = (buffer.len() as u64).min(ino.size - offset);
		let end = offset + len;

		while offset < end {
			let block = self.inode_find_block(inr, &ino, offset);
			let num = (block.size - block.off).min(end - offset);

			// TODO: remove this read, if writing a full block
			self.inode_read_block(
				inr,
				&ino,
				block.blkidx,
				&mut blockbuf[0..(block.size as usize)],
			)?;

			let off = block.off as usize;
			blockbuf[off..(off + num as usize)]
				.copy_from_slice(&buffer[boff..(boff + num as usize)]);

			self.inode_write_block(
				inr,
				&mut ino,
				block.blkidx,
				&blockbuf[0..(block.size as usize)],
			)?;

			offset += num;
			boff += num as usize;
		}

		Ok(boff)
	}

	/// Copy data within a file.
	pub(super) fn inode_copy_range(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		from: impl RangeBounds<u64>,
		to: impl RangeBounds<u64>,
	) -> IoResult<u64> {
		fn decode(ino: &Inode, b: impl RangeBounds<u64>) -> (u64, u64, u64) {
			let beg = match b.start_bound() {
				Bound::Unbounded => 0,
				Bound::Included(x) => *x,
				Bound::Excluded(_) => todo!(),
			};
			let end = match b.end_bound() {
				Bound::Unbounded => ino.size,
				Bound::Included(x) => *x - 1,
				Bound::Excluded(x) => *x,
			};
			assert!(beg <= end);
			(beg, end, end - beg)
		}

		self.assert_rw()?;

		let (fbeg, fend, flen) = decode(ino, from);
		let (tbeg, mut tend, mut tlen) = decode(ino, to);

		assert!(tlen >= flen);
		if tlen > flen {
			tend = tbeg + flen;
			tlen = flen;
		}
		assert_eq!(flen, tlen);

		let mut fpos = fbeg;
		let mut tpos = tbeg;
		let mut buf = [0u8; 512];

		while fpos < fend {
			assert!(tpos < tend);
			let n = (fend - fpos).min(buf.len() as u64);
			let nr = self.inode_read(inr, fpos, &mut buf)?;
			assert_eq!(n, nr as u64);

			let nw = self.inode_write(inr, tpos, &buf[..nr])?;
			assert_eq!(n, nw as u64);

			fpos += n;
			tpos += n;
		}

		Ok(flen)
	}

	/// Read an inode.
	///
	/// Through the metadata cache, so an inode that has been written but not
	/// yet persisted — because a Soft Updates dependency still gates part of it
	/// — reads back exactly as it was written.  Every caller depends on that:
	/// `dir_iter`, `check_consistency`, the allocation policies and the
	/// mapping layer all have to see the filesystem's current state, not the
	/// last image that reached the disk.
	pub(super) fn read_inode(&mut self, inr: InodeNum) -> IoResult<Inode> {
		log::trace!("read_inode({inr});");
		let off = self.superblock.ino_to_fso(inr);
		let ino: Inode = self.metadata_read(off)?;
		let mode = ino.mode;

		if (mode & S_IFMT) == 0 {
			log::warn!("invalid inode {inr}: Mode {mode:#o}, S_IFMT is 0");
			return Err(err!(EINVAL));
		}

		Ok(ino)
	}

	/// Stage an inode.
	///
	/// An inode occupies `UFS_INOSZ` bytes of a shared inode block, so this is
	/// a read-modify-write of exactly that range: the `fs_inopb - 1` other
	/// inodes in the block must survive untouched, which is what the cache's
	/// whole-block view gives us.  It does not persist anything.
	pub(super) fn write_inode(&mut self, inr: InodeNum, ino: &Inode) -> IoResult<()> {
		log::trace!("write_inode({inr});");
		self.assert_rw()?;
		let off = self.superblock.ino_to_fso(inr);
		self.metadata_write(off, &ino)?;
		Ok(())
	}

	pub fn inode_modify(
		&mut self,
		inr: InodeNum,
		f: impl FnOnce(InodeAttr) -> InodeAttr,
	) -> IoResult<InodeAttr> {
		self.assert_rw()?;
		let mut ino = self.read_inode(inr)?;
		let attr = f(ino.as_attr(inr));

		ino.mode = (ino.mode & S_IFMT) | (attr.perm & !S_IFMT);
		ino.uid = attr.uid;
		ino.gid = attr.gid;
		ino.set_atime(attr.atime);
		ino.set_mtime(attr.mtime);
		ino.set_ctime(attr.ctime);
		ino.set_btime(attr.btime);
		ino.flags = attr.flags;

		self.write_inode(inr, &ino)?;
		Ok(ino.as_attr(inr))
	}

	pub(super) fn inode_read_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		blkidx: u64,
		buf: &mut [u8],
	) -> IoResult<usize> {
		log::trace!("inode_read_block({inr}, {blkidx}, [u8; {}]);", buf.len());
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);
		match self.inode_resolve_block(inr, ino, blkidx)? {
			Some(blkno) => {
				if Self::blocks_are_metadata(ino) {
					// Read the *live* image, not the disk.  A directory block
					// that has been extended but not flushed is the common
					// case right after a link, and reading past the cache would
					// make the new entry invisible to the very lookup that is
					// about to find it.
					let at = blkno.get() * fs;
					buf[0..size].copy_from_slice(&self.metadata_read_at(at, size)?);
				} else {
					self.file.read_at(blkno.get() * fs, &mut buf[0..size])?;
				}
			}
			None => buf.fill(0u8),
		}

		Ok(size)
	}

	/// Whether `ino`'s data blocks are filesystem metadata rather than file
	/// data.
	///
	/// A directory's blocks are not a file's bytes.  They hold the entries that
	/// make a name resolvable, and Soft Updates has to be able to hold *one
	/// entry* back while the rest of the block goes out, so they belong in the
	/// metadata cache exactly like inode blocks and indirect blocks do.
	///
	/// This has to be decided here rather than in `dir.rs` because every
	/// directory path -- `dir_iter`, `dir_newlink`, `dir_unlink`,
	/// `inode_copy_range`, `inode_truncate` -- reaches its blocks through the
	/// generic read and write below.  Gating an entry in `dir_newlink` while
	/// every other directory path went straight to the device would give one
	/// block two writers, which is the same mistake the cylinder-group bitmaps
	/// already made: the write-back of the cached block would restore whatever
	/// the direct write had replaced.
	///
	/// A *file's* blocks deliberately stay direct.  The kernel page cache is
	/// already buffering them, there are no ordering constraints between two
	/// blocks of a file's payload, and a second layer would turn every FUSE
	/// writeback into an ordering question for no benefit.
	fn blocks_are_metadata(ino: &Inode) -> bool {
		ino.kind() == InodeType::Directory
	}

	pub(super) fn inode_write_block(
		&mut self,
		inr: InodeNum,
		ino: &mut Inode,
		blkidx: u64,
		buf: &[u8],
	) -> IoResult<()> {
		log::trace!("inode_write_block({inr}, {blkidx})");
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);

		let blkno = match self.inode_resolve_block(inr, ino, blkidx)? {
			Some(blkno) => blkno,
			None => self.inode_alloc_block(inr, ino, blkidx, size as u64)?.0,
		};

		if Self::blocks_are_metadata(ino) {
			// Staged, not written.  Whether this block may reach the disk -- and
			// which of its entries may -- is the dependency engine's decision,
			// and `sync_metadata()` is the only thing that carries it out.
			let at = blkno.get() * fs;
			self.metadata_write_at(at, &buf[0..size])?;
			return Ok(());
		}

		self.file.write_at(blkno.get() * fs, &buf[0..size])?;

		// `buf[0..size]` is the whole allocated region for this logical block --
		// `inode_write()` always assembles it -- so the block's initialised
		// contents have now reached the device, and any dependency waiting on
		// that fact may advance.  Noting it here rather than at allocation time
		// is the point: the allocation only *reserved* the block.
		//
		// A metadata block does not come through here, so its contents event
		// comes from the flush loop instead, which is where its bytes actually
		// go.
		self.note_block_contents(blkno.get())?;
		Ok(())
	}

	/// Where a file offset falls, without the panic.
	///
	/// `None` means the offset is beyond the end of the geometry `i_size`
	/// implies, which is the filesystem saying "there is nothing here" rather
	/// than "something is broken" — the caller decides whether that is `EOF`,
	/// a hole or an error.
	///
	/// The last block of a file is the fragment case: `i_size % fs_bsize` bytes
	/// that UFS2 accounts for as a run of `fs_fsize` fragments, so its size is
	/// the rounded-up remainder rather than a whole `fs_bsize`.  Every caller
	/// that assumed otherwise would over-read the last block.
	pub(super) fn inode_locate(
		&self,
		inr: InodeNum,
		ino: &Inode,
		offset: u64,
	) -> Option<BlockInfo> {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let (blocks, frags) = ino.size(bs, fs);
		log::trace!(
			"inode_locate({inr}, {offset}): size={}, bs={bs}, blocks={blocks}, fs={fs}, frags={frags}",
			ino.size
		);

		let x = if offset < (bs * blocks) {
			BlockInfo {
				blkidx: offset / bs,
				off:    offset % bs,
				size:   bs,
			}
		} else if offset < (bs * blocks + fs * frags) {
			BlockInfo {
				blkidx: blocks,
				off:    offset % bs,
				size:   frags * fs,
			}
		} else {
			return None;
		};
		log::trace!("inode_locate({inr}, {offset}) = {x:?}");
		Some(x)
	}

	/// [`Self::inode_locate`], panicking when the offset is out of bounds.
	///
	/// The read and write paths use this because they have already clamped to
	/// `i_size`, so being out of bounds is a bug in *them* and not something
	/// the caller asked for.  The mapping layer uses [`Self::inode_locate`],
	/// because an arbitrary offset past the end of a file is a perfectly good
	/// question.
	pub(super) fn inode_find_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		offset: u64,
	) -> BlockInfo {
		match self.inode_locate(inr, ino, offset) {
			Some(x) => x,
			None => panic!("inode_find_block({inr}, {offset}): out of bounds"),
		}
	}

	pub(super) fn inode_data_zones(&self) -> (u64, u64, u64, u64) {
		let nd = UFS_NDADDR as u64;
		let pbp = self.superblock.bsize as u64 / size_of::<u64>() as u64;

		(
			nd,
			nd + pbp,
			nd + pbp + (pbp * pbp),
			nd + pbp + (pbp * pbp) + (pbp * pbp * pbp),
		)
	}

	pub(super) fn decode_blkidx(&self, blkidx: u64) -> IoResult<InodeBlock> {
		let bs = self.superblock.bsize as u64;
		let pbp = bs / size_of::<u64>() as u64;
		let (begin_indir1, begin_indir2, begin_indir3, begin_indir4) = self.inode_data_zones();

		if blkidx < begin_indir1 {
			Ok(InodeBlock::Direct(blkidx as usize))
		} else if blkidx < begin_indir2 {
			let x = blkidx - begin_indir1;
			Ok(InodeBlock::Indirect1(x as usize))
		} else if blkidx < begin_indir3 {
			let x = blkidx - begin_indir2;
			let high = x / pbp;
			let low = x % pbp;
			Ok(InodeBlock::Indirect2(high as usize, low as usize))
		} else if blkidx < begin_indir4 {
			let x = blkidx - begin_indir3;
			let high = x / pbp / pbp;
			let mid = x / pbp % pbp;
			let low = x % pbp;
			Ok(InodeBlock::Indirect3(
				high as usize,
				mid as usize,
				low as usize,
			))
		} else {
			Err(err!(EINVAL))
		}
	}

	pub(super) fn inode_resolve_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		blkno: u64,
	) -> IoResult<Option<NonZeroU64>> {
		let sb = &self.superblock;
		let bs = sb.bsize as u64;
		let su64 = size_of::<UfsDaddr>() as u64;
		let pbp = bs / su64;

		let InodeData::Blocks(InodeBlocks { direct, indirect }) = &ino.data else {
			log::warn!("inode_resolve_block({inr}, {blkno}): inode doesn't have blocks");
			return Err(err!(EIO));
		};

		let mut data = vec![0u64; pbp as usize];
		let bno = match self.decode_blkidx(blkno)? {
			InodeBlock::Direct(off) => NonZeroU64::new(direct[off] as u64),
			InodeBlock::Indirect1(off) => {
				let x1 = indirect[0] as u64;
				if x1 == 0 {
					return Ok(None);
				}

				self.read_pblock(x1, &mut data)?;
				NonZeroU64::new(data[off])
			}
			InodeBlock::Indirect2(high, low) => {
				let x1 = indirect[1] as u64;
				if x1 == 0 {
					return Ok(None);
				}

				self.read_pblock(x1, &mut data)?;
				let x2 = data[high];
				if x2 == 0 {
					return Ok(None);
				}

				self.read_pblock(x2, &mut data)?;
				NonZeroU64::new(data[low])
			}
			InodeBlock::Indirect3(high, mid, low) => {
				let x1 = indirect[2] as u64;
				if x1 == 0 {
					return Ok(None);
				}

				self.read_pblock(x1, &mut data)?;
				let x2 = data[high];
				if x2 == 0 {
					return Ok(None);
				}

				self.read_pblock(x2, &mut data)?;
				let x3 = data[mid];
				if x3 == 0 {
					return Ok(None);
				}

				self.read_pblock(x3, &mut data)?;
				NonZeroU64::new(data[low])
			}
		};

		log::trace!("inode_resolve_block({inr}, {blkno}): bno={bno:?}");

		Ok(bno)
	}

	pub(super) fn inode_get_block_size(&mut self, ino: &Inode, blkidx: u64) -> usize {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let (blocks, frags) = ino.size(bs, fs);

		let res = if blkidx < blocks {
			bs as usize
		} else if blkidx < blocks + frags {
			(fs * frags) as usize
		} else {
			panic!("out of bounds: blkidx={blkidx}, blocks={blocks}, frags={frags}");
		};

		log::trace!(
			"inode_get_block_size(blkidx={blkidx}) = {res}; frags={frags}, blocks={blocks}"
		);
		res
	}
}
