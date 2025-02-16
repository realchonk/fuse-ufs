use super::*;
use crate::{err, InodeNum};

impl<R: Backend> Ufs<R> {
	/// Get metadata about an inode.
	#[doc(alias("stat", "getattr"))]
	pub fn inode_attr(&mut self, inr: InodeNum) -> IoResult<InodeAttr> {
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
			buffer[boff..(boff + num as usize)].copy_from_slice(&blockbuf[off..(off + num as usize)]);

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
		let ino = self.read_inode(inr)?;

		if offset + buffer.len() as u64 > ino.size {
			todo!("resizing files")
		}

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
			blockbuf[off..(off + num as usize)].copy_from_slice(&buffer[boff..(boff + num as usize)]);

			self.inode_write_block(
				inr,
				&ino,
				block.blkidx,
				&blockbuf[0..(block.size as usize)],
			)?;

			offset += num;
			boff += num as usize;
		}

		Ok(boff)
	}

	pub(super) fn read_inode(&mut self, inr: InodeNum) -> IoResult<Inode> {
		let off = self.superblock.ino_to_fso(inr);
		let ino: Inode = self.file.decode_at(off)?;

		if (ino.mode & S_IFMT) == 0 {
			log::warn!("invalid inode {inr}");
			return Err(err!(EINVAL));
		}

		Ok(ino)
	}

	pub(super) fn write_inode(&mut self, inr: InodeNum, ino: &Inode) -> IoResult<()> {
		self.assert_rw()?;
		let off = self.superblock.ino_to_fso(inr);
		self.file.encode_at(off, &ino)?;
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
		log::trace!("inode_read_block({inr}, {blkidx});");
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);
		match self.inode_resolve_block(inr, ino, blkidx)? {
			Some(blkno) => {
				self.file.read_at(blkno.get() * fs, &mut buf[0..size])?;
			}
			None => buf.fill(0u8),
		}

		Ok(size)
	}

	pub(super) fn inode_write_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		blkidx: u64,
		buf: &[u8],
	) -> IoResult<()> {
		log::trace!("inode_write_block({inr}, {blkidx})");
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);
		match self.inode_resolve_block(inr, ino, blkidx)? {
			Some(blkno) => {
				self.file.write_at(blkno.get() * fs, &buf[0..size])?;
			}
			None => todo!("TODO: implement block allocation"),
		}
		Ok(())
	}

	pub(super) fn inode_find_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		offset: u64,
	) -> BlockInfo {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let (blocks, frags) = ino.size(bs, fs);
		log::trace!(
			"inode_find_block({inr}, {offset}): size={}, bs={bs}, blocks={blocks}, fs={fs}, frags={frags}",
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
			panic!("inode_find_block({inr}, {offset}): out of bounds");
		};
		log::trace!("inode_find_block({inr}, {offset}) = {x:?}");
		x
	}

	fn inode_resolve_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		blkno: u64,
	) -> IoResult<Option<NonZeroU64>> {
		let sb = &self.superblock;
		let fs = sb.fsize as u64;
		let bs = sb.bsize as u64;
		let nd = UFS_NDADDR as u64;
		let su64 = size_of::<UfsDaddr>() as u64;
		let pbp = bs / su64;

		let InodeData::Blocks(InodeBlocks { direct, indirect }) = &ino.data else {
			log::warn!("inode_resolve_block({inr}, {blkno}): inode doesn't have blocks");
			return Err(err!(EIO));
		};

		let begin_indir1 = nd;
		let begin_indir2 = nd + pbp;
		let begin_indir3 = nd + pbp + pbp * pbp;
		let begin_indir4 = nd + pbp + pbp * pbp + pbp * pbp * pbp;

		if blkno < begin_indir1 {
			Ok(NonZeroU64::new(direct[blkno as usize] as u64))
		} else if blkno < begin_indir2 {
			let low = blkno - begin_indir1;
			assert!(low < pbp);

			log::trace!("inode_resolve_block({inr}, {blkno}): 1-indirect: low={low}");

			let first = indirect[0] as u64;
			if first == 0 {
				return Ok(None);
			}

			let pos = first * fs + low * su64;
			let block: u64 = self.file.decode_at(pos)?;
			log::trace!("first={first:#x} *{pos:#x} = {block:#x}");
			Ok(NonZeroU64::new(block))
		} else if blkno < begin_indir3 {
			let x = blkno - begin_indir2;
			let low = x % pbp;
			let high = x / pbp;
			assert!(high < pbp);

			log::trace!("inode_resolve_block({inr}, {blkno}): 2-indirect: high={high}, low={low}");

			let first = indirect[1] as u64;
			if first == 0 {
				return Ok(None);
			}
			let pos = first * fs + high * su64;
			let snd: u64 = self.file.decode_at(pos)?;
			log::trace!("first={first:x} pos={pos:x} snd={snd:x}");
			if snd == 0 {
				return Ok(None);
			}

			let pos = snd * fs + low * su64;
			let block: u64 = self.file.decode_at(pos)?;
			log::trace!("*{pos:x} = {block:x}");
			Ok(NonZeroU64::new(block))
		} else if blkno < begin_indir4 {
			let x = blkno - begin_indir3;
			let low = x % pbp;
			let mid = x / pbp % pbp;
			let high = x / pbp / pbp;
			assert!(high < pbp);

			log::trace!(
				"inode_resolve_block({inr}, {blkno}): 3-indirect: x={x:#x} high={high:#x}, mid={mid:#x}, low={low:#x}"
			);

			let first = indirect[2] as u64;
			log::trace!("first = {first:#x}");
			if first == 0 {
				return Ok(None);
			}

			let pos = first * fs + high * su64;
			let second: u64 = self.file.decode_at(pos)?;
			log::trace!("second = {second:#x}");
			if second == 0 {
				return Ok(None);
			}

			let pos = second * fs + mid * su64;
			let third: u64 = self.file.decode_at(pos)?;
			log::trace!("third = {third:#x}");
			if third == 0 {
				return Ok(None);
			}
			let pos = third * fs + low * su64;
			let block: u64 = self.file.decode_at(pos)?;
			Ok(NonZeroU64::new(block))
		} else {
			log::warn!("block number too large: {blkno} >= {begin_indir4}");
			Ok(None)
		}
	}

	fn inode_get_block_size(&mut self, ino: &Inode, blkidx: u64) -> usize {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let (blocks, frags) = ino.size(bs, fs);

		if blkidx < blocks {
			bs as usize
		} else if blkidx < blocks + frags {
			(fs * frags) as usize
		} else {
			panic!("out of bounds: {blkidx}, blocks: {blocks}, frags: {frags}");
		}
	}
}
