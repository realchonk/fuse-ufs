use super::*;
use crate::{err, InodeNum};

impl<R: Read + Seek> Ufs<R> {
	pub fn inode_attr(&mut self, inr: InodeNum) -> IoResult<InodeAttr> {
		let ino = self.read_inode(inr)?;
		Ok(ino.as_attr(inr))
	}

	pub fn inode_read(
		&mut self,
		inr: InodeNum,
		mut offset: u64,
		buffer: &mut [u8],
	) -> IoResult<usize> {
		let mut blockbuf = vec![0u8; self.superblock.bsize as usize];
		let ino = self.read_inode(inr)?;

		let mut boff = 0;
		let len = buffer.len() as u64;
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
			buffer[boff..(boff + num as usize)].copy_from_slice(&blockbuf[0..(num as usize)]);

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

	pub(super) fn inode_read_block(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		blkidx: u64,
		buf: &mut [u8],
	) -> IoResult<usize> {
		log::trace!("read_file_block({inr}, {blkidx});");
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
			"find_file_block({inr}, {offset}): size={}, blocks={blocks}, frags={frags}",
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
				blkidx: blocks + (offset - blocks * bs) / fs,
				off:    offset % fs,
				size:   fs,
			}
		} else {
			panic!("out of bounds");
		};
		log::trace!("find_file_block({inr}, {offset}) = {x:?}");
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
			log::warn!("resolve_file_block({inr}, {blkno}): inode doesn't have blocks");
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

			log::trace!("resolve_file_block({inr}, {blkno}): 1-indirect: low={low}");

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

			log::trace!("resolve_file_block({inr}, {blkno}): 2-indirect: high={high}, low={low}");

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
				"resolve_file_block({inr}, {blkno}): 3-indirect: x={x:#x} high={high:#x}, mid={mid:#x}, low={low:#x}"
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
			fs as usize
		} else {
			panic!("out of bounds: {blkidx}, blocks: {blocks}, frags: {frags}");
		}
	}
}
