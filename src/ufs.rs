use std::{
	ffi::{c_int, OsStr},
	io::{Cursor, Error as IoError, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	path::Path,
	time::Duration,
};

use anyhow::{bail, Result};
use fuser::{FileType, Filesystem, KernelConfig, Request};

const MAX_CACHE: Duration = Duration::MAX;

use crate::{
	blockreader::BlockReader,
	data::*,
	decoder::{Config, Decoder},
};

macro_rules! err {
	($name:ident) => {
		IoError::from_raw_os_error(libc::$name)
	};
}

pub struct Ufs {
	file:       Decoder<BlockReader>,
	superblock: Superblock,
}

impl Ufs {
	pub fn open(path: &Path) -> Result<Self> {
		let mut file = BlockReader::open(path)?;

		let pos = SBLOCK_UFS2 as u64 + MAGIC_OFFSET;
		file.seek(SeekFrom::Start(pos))?;
		let mut magic = [0u8; 4];
		file.read_exact(&mut magic)?;

		// magic: 0x19 54 01 19
		let config = match magic {
			[0x19, 0x01, 0x54, 0x19] => Config::little(),
			[0x19, 0x54, 0x01, 0x19] => Config::big(),
			_ => bail!("invalid superblock magic number: {magic:?}"),
		};

		let mut file = Decoder::new(file, config);

		let superblock: Superblock = file.decode_at(SBLOCK_UFS2 as u64)?;
		if superblock.magic != FS_UFS2_MAGIC {
			bail!("invalid superblock magic number: {}", superblock.magic);
		}
		//assert_eq!(superblock.cgsize, CGSIZE as i32);
		Ok(Self { file, superblock })
	}

	fn read_inode(&mut self, inr: u64) -> IoResult<Inode> {
		let off = self.superblock.ino_to_fso(inr);
		let ino: Inode = self.file.decode_at(off)?;

		if (ino.mode & S_IFMT) == 0 {
			log::warn!("invalid inode {inr}");
			return Err(err!(EINVAL));
		}

		Ok(ino)
	}

	fn resolve_file_block(
		&mut self,
		inr: u64,
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

		if blkno < nd {
			Ok(NonZeroU64::new(direct[blkno as usize] as u64))
		} else if blkno < (nd + pbp) {
			let low = blkno - nd;
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
		} else if blkno < (nd + pbp * pbp) {
			let x = blkno - nd - pbp;
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
		} else if blkno < (nd + pbp * pbp * pbp) {
			let x = blkno - nd - pbp - pbp * pbp;
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
			let max = nd + pbp * pbp * pbp;
			log::warn!("block number too large: {blkno} >= {max}");
			Ok(None)
		}
	}

	fn find_file_block(&mut self, inr: u64, ino: &Inode, offset: u64) -> BlockInfo {
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

	fn inode_get_block_size(&mut self, ino: &Inode, blkidx: u64) -> usize {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let (blocks, frags) = ino.size(bs, fs);

		if blkidx < blocks {
			bs as usize
		} else if blkidx < blocks + frags {
			fs as usize
		} else {
			dbg!(ino);
			panic!("out of bounds: {blkidx}, blocks: {blocks}, frags: {frags}");
		}
	}

	fn read_file_block(
		&mut self,
		inr: u64,
		ino: &Inode,
		blkidx: u64,
		buf: &mut [u8],
	) -> IoResult<usize> {
		log::trace!("read_file_block({inr}, {blkidx});");
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);
		match self.resolve_file_block(inr, ino, blkidx)? {
			Some(blkno) => {
				self.file.read_at(blkno.get() * fs, &mut buf[0..size])?;
			}
			None => buf.fill(0u8),
		}

		Ok(size)
	}

	fn readdir<T>(
		&mut self,
		inr: u64,
		ino: &Inode,
		mut f: impl FnMut(&OsStr, InodeNum, FileType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let mut block = vec![0u8; self.superblock.bsize as usize];
		let frag = self.superblock.frag as u64;

		for blkidx in 0..(ino.blocks / frag) {
			let size = self.read_file_block(inr, ino, blkidx, &mut block)?;

			let x = readdir_block(inr, &block[0..size], self.file.config(), &mut f)?;
			if x.is_some() {
				return Ok(x);
			}
		}
		Ok(None)
	}
}

fn run<T>(f: impl FnOnce() -> IoResult<T>) -> Result<T, c_int> {
	f().map_err(|e| {
		log::error!("Error: {e}");
		e.raw_os_error().unwrap_or(libc::EIO)
	})
}

fn transino(ino: u64) -> u64 {
	if ino == fuser::FUSE_ROOT_ID {
		2
	} else {
		ino
	}
}

fn readdir_block<T>(
	inr: u64,
	block: &[u8],
	config: Config,
	mut f: impl FnMut(&OsStr, InodeNum, FileType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut name = [0u8; UFS_MAXNAMELEN + 1];
	let file = Cursor::new(block);
	let mut file = Decoder::new(file, config);

	loop {
		let Ok(ino) = file.decode::<InodeNum>() else {
			break;
		};
		if ino == 0 {
			break;
		}

		let reclen: u16 = file.decode()?;
		let kind: u8 = file.decode()?;
		let namelen: u8 = file.decode()?;
		let name = &mut name[0..namelen.into()];
		file.read(name)?;

		// skip remaining bytes of record, if any
		let off = reclen - (namelen as u16) - 8;
		file.seek_relative(off as i64)?;

		let name = unsafe { OsStr::from_encoded_bytes_unchecked(name) };
		let kind = match kind {
			DT_FIFO => FileType::NamedPipe,
			DT_CHR => FileType::CharDevice,
			DT_DIR => FileType::Directory,
			DT_BLK => FileType::BlockDevice,
			DT_REG => FileType::RegularFile,
			DT_LNK => FileType::Symlink,
			DT_SOCK => FileType::Socket,
			DT_WHT => {
				log::warn!("readdir_block({inr}): encountered a whiteout entry: {name:?}");
				continue;
			}
			DT_UNKNOWN => todo!("DT_UNKNOWN: {ino}"),
			_ => panic!("invalid filetype: {kind}"),
		};
		let res = f(name, ino, kind);
		if res.is_some() {
			return Ok(res);
		}
	}

	Ok(None)
}

impl Filesystem for Ufs {
	fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
		let sb = &self.superblock;
		log::debug!("Superblock: {sb:#?}");

		log::info!("Summary:");
		log::info!("Block Size: {}", sb.bsize);
		log::info!("# Blocks: {}", sb.size);
		log::info!("# Data Blocks: {}", sb.dsize);
		log::info!("Fragment Size: {}", sb.fsize);
		log::info!("Fragments per Block: {}", sb.frag);
		log::info!("# Cylinder Groups: {}", sb.ncg);
		log::info!("CG Size: {}MiB", sb.cgsize() / 1024 / 1024);
		assert!(sb.cgsize_struct() < sb.bsize as usize);

		// check that all superblocks are ok.
		for i in 0..sb.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.sblkno) * sb.fsize) as u64;
			let csb: Superblock = self.file.decode_at(addr).unwrap();
			if csb.magic != FS_UFS2_MAGIC {
				log::error!("CG{i} has invalid superblock magic: {:x}", csb.magic);
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.cblkno) * sb.fsize) as u64;
			let cg: CylGroup = self.file.decode_at(addr).unwrap();
			if cg.magic != CG_MAGIC {
				log::error!("CG{i} has invalid cg magic: {:x}", cg.magic);
			}
		}
		log::info!("OK");

		Ok(())
	}

	fn destroy(&mut self) {}

	fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: fuser::ReplyAttr) {
		let ino = transino(ino);
		match run(|| self.read_inode(ino)) {
			Ok(x) => reply.attr(&MAX_CACHE, &x.as_fileattr(ino)),
			Err(e) => reply.error(e),
		}
	}

	fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
		let _ino = transino(ino);
		reply.opened(0, 0);
	}

	fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
		let _ino = transino(ino);
		reply.opened(0, 0);
	}

	// TODO: use offset in a less stupid way
	fn readdir(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		mut reply: fuser::ReplyDirectory,
	) {
		let inr = transino(inr);
		let f = || {
			if offset != 0 {
				return Ok(());
			}

			let ino = self.read_inode(inr)?;

			let mut i = 0;

			self.readdir(inr, &ino, |name, inr, kind| {
				i += 1;
				if i > offset && reply.add(inr.into(), i, kind, name) {
					return Some(());
				}
				None
			})?;

			Ok(())
		};
		match run(f) {
			Ok(_) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}

	fn lookup(&mut self, _req: &Request<'_>, pinr: u64, name: &OsStr, reply: fuser::ReplyEntry) {
		let pinr = transino(pinr);

		let f = || {
			let pino = self.read_inode(pinr)?;

			let x = self.readdir(pinr, &pino, |name2, inr, _| {
				if name == name2 {
					Some(inr.into())
				} else {
					None
				}
			});

			match x {
				Ok(Some(inr)) => {
					let ino = self.read_inode(inr)?;
					Ok((ino.as_fileattr(inr), ino.gen))
				}
				Ok(None) => Err(err!(ENOENT)),
				Err(e) => Err(e),
			}
		};

		match run(f) {
			Ok((attr, gen)) => reply.entry(&Duration::ZERO, &attr, gen.into()),
			Err(e) => reply.error(e),
		}
	}

	fn read(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		size: u32,
		_flags: i32,
		_lock_owner: Option<u64>,
		reply: fuser::ReplyData,
	) {
		let inr = transino(inr);

		let f = || {
			let mut buffer = vec![0u8; size as usize];
			let mut blockbuf = vec![0u8; self.superblock.bsize as usize];
			let ino = self.read_inode(inr)?;

			let mut offset = offset as u64;
			let mut boff = 0;
			let len = size as u64;
			let end = offset + len;

			while offset < end {
				let block = self.find_file_block(inr, &ino, offset);
				let num = (block.size - block.off).min(end - offset);

				self.read_file_block(
					inr,
					&ino,
					block.blkidx,
					&mut blockbuf[0..(block.size as usize)],
				)?;
				buffer[boff..(boff + num as usize)].copy_from_slice(&blockbuf[0..(num as usize)]);

				offset += num;
				boff += num as usize;
			}

			Ok(buffer)
		};

		match run(f) {
			Ok(buf) => reply.data(&buf),
			Err(e) => reply.error(e),
		}
	}

	fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
		let sb = &self.superblock;
		let cst = &sb.cstotal;
		let bfree = cst.nbfree as u64;
		let ffree = cst.nffree as u64;
		let free = bfree * sb.frag as u64 + ffree;
		reply.statfs(
			sb.dsize as u64,
			free,
			free,
			(sb.ipg * sb.ncg) as u64,
			cst.nifree as u64,
			sb.bsize as u32,
			255,
			sb.fsize as u32,
		)
	}

	fn readlink(&mut self, _req: &Request<'_>, inr: u64, reply: fuser::ReplyData) {
		let inr = transino(inr);
		let f = || {
			let ino = self.read_inode(inr)?;

			if ino.kind() != FileType::Symlink {
				return Err(err!(EINVAL));
			}

			match &ino.data {
				InodeData::Shortlink(link) => {
					assert_eq!(ino.blocks, 0);
					let len = ino.size as usize;
					Ok(link[0..len].to_vec())
				}
				InodeData::Blocks { .. } => {
					// TODO: this has to be tested for other configurations, such as 4K/4K
					assert!(ino.blocks <= 8);

					let len = ino.size as usize;
					let mut buf = vec![0u8; self.superblock.bsize as usize];
					self.read_file_block(inr, &ino, 0, &mut buf)?;
					buf.resize(len, 0u8);
					Ok(buf)
				}
			}
		};

		match run(f) {
			Ok(x) => reply.data(&x),
			Err(e) => reply.error(e),
		}
	}
}
