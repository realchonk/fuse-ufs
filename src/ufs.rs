use std::{
	ffi::{c_int, OsStr},
	io::{Cursor, Error as IoError, ErrorKind, Result as IoResult},
	mem::size_of,
	num::NonZeroU64,
	path::Path,
	time::Duration,
};

use anyhow::{bail, Result};
use fuser::{FileType, Filesystem, KernelConfig, Request};

const MAX_CACHE: Duration = Duration::MAX;

use crate::{blockreader::BlockReader, data::*, decoder::Decoder};

pub struct Ufs {
	file: Decoder<BlockReader>,
	superblock: Superblock,
}

impl Ufs {
	pub fn open(path: &Path) -> Result<Self> {
		let file = BlockReader::open(path)?;

		let mut file = Decoder::new(file);

		let superblock: Superblock = file.decode_at(SBLOCK_UFS2 as u64)?;
		if superblock.magic != FS_UFS2_MAGIC {
			bail!("invalid superblock magic number: {}", superblock.magic);
		}
		//assert_eq!(superblock.cgsize, CGSIZE as i32);
		Ok(Self { file, superblock })
	}

	// TODO: bincodify inode
	fn read_inode(&mut self, ino: u64) -> IoResult<Inode> {
		let off = self.superblock.ino_to_fso(ino);
		let ino: Inode = self.file.decode_at(off)?;

		if (ino.mode & S_IFMT) == 0 {
			return Err(IoError::new(ErrorKind::BrokenPipe, "invalid inode"));
		}

		Ok(ino)
	}

	fn resolve_file_block(&mut self, ino: &Inode, blkno: u64) -> IoResult<Option<NonZeroU64>> {
		let sb = &self.superblock;
		let fs = sb.fsize as u64;
		let nd = UFS_NDADDR as u64;
		let su64 = size_of::<u64>() as u64;
		let pbp = fs / su64;

		if blkno >= ino.blocks {
			return Err(IoError::new(ErrorKind::InvalidInput, "out of bounds"));
		}

		let InodeData::Blocks { direct, indirect } = &ino.data else {
			return Err(IoError::new(ErrorKind::InvalidInput, "doesn't have blocks"));
		};

		if blkno < nd {
			Ok(NonZeroU64::new(direct[blkno as usize] as u64))
		} else if blkno < (nd + pbp) {
			let first = indirect[0] as u64;
			let pos = first * fs + (blkno - nd) * su64;
			let pos: u64 = self.file.decode_at(pos)?;
			Ok(NonZeroU64::new(pos))
		} else if blkno < (nd + pbp * pbp) {
			eprintln!("TODO: second-level indirect block addressing");
			Ok(None)
		} else if blkno < (nd + pbp * pbp * pbp) {
			eprintln!("TODO: third-level indirect block addressing");
			Ok(None)
		} else {
			eprintln!("WARN: address too large");
			Ok(None)
		}
	}

	fn find_file_block(&mut self, ino: &Inode, offset: u64) -> BlockInfo {
		let bs = self.superblock.bsize as u64;
		let fs = self.superblock.fsize as u64;
		let nfull = ino.blocks / self.superblock.frag as u64;
		let fullend = nfull * bs;

		if offset < fullend {
			BlockInfo {
				blkidx: offset / bs,
				off: offset % bs,
				size: bs,
			}
		} else if offset < (ino.blocks * fs) {
			BlockInfo {
				blkidx: nfull + (offset - fullend) / fs,
				off: offset % fs,
				size: fs,
			}
		} else {
			panic!("out of bounds")
		}
	}

	fn inode_get_block_size(&mut self, ino: &Inode, blkidx: u64) -> usize {
		let nfull = ino.blocks / self.superblock.frag as u64;

		if blkidx < nfull {
			self.superblock.bsize as usize
		} else if blkidx < ino.blocks {
			self.superblock.fsize as usize
		} else {
			panic!("out of bounds")
		}
	}

	fn read_file_block(&mut self, ino: &Inode, blkidx: u64, buf: &mut [u8]) -> IoResult<usize> {
		let fs = self.superblock.fsize as u64;
		let size = self.inode_get_block_size(ino, blkidx);
		match self.resolve_file_block(ino, blkidx)? {
			Some(blkno) => {
				self.file.read_at(blkno.get() * fs, &mut buf[0..size])?;
			}
			None => buf.fill(0u8),
		}

		Ok(size)
	}

	fn readdir<T>(
		&mut self,
		ino: &Inode,
		mut f: impl FnMut(&OsStr, InodeNum, FileType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let mut block = vec![0u8; self.superblock.bsize as usize];

		for blkidx in 0..ino.blocks {
			let size = self.read_file_block(ino, blkidx, &mut block)?;

			let x = readdir_block(&block[0..size], &mut f)?;
			if x.is_some() {
				return Ok(x);
			}
		}
		Ok(None)
	}
}

fn run<T>(f: impl FnOnce() -> IoResult<T>) -> Result<T, c_int> {
	f().inspect_err(|e| eprintln!("Error: {e:?}"))
		.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
}

fn transino(ino: u64) -> u64 {
	if ino == fuser::FUSE_ROOT_ID {
		2
	} else {
		ino
	}
}

fn readdir_block<T>(
	block: &[u8],
	mut f: impl FnMut(&OsStr, InodeNum, FileType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut name = [0u8; UFS_MAXNAMELEN + 1];
	let file = Cursor::new(block);
	let mut file = Decoder::new(file);

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
			DT_WHT => todo!("DT_WHT: {ino}"),
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
		println!("Superblock: {:#?}", sb);

		println!("Summary:");
		println!("Block Size: {}", sb.bsize);
		println!("# Blocks: {}", sb.size);
		println!("# Data Blocks: {}", sb.dsize);
		println!("Fragment Size: {}", sb.fsize);
		println!("Fragments per Block: {}", sb.frag);
		println!("# Cylinder Groups: {}", sb.ncg);
		println!("CG Size: {}MiB", sb.cgsize() / 1024 / 1024);
		assert!(sb.cgsize_struct() < sb.bsize as usize);

		// check that all superblocks are ok.
		for i in 0..sb.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.sblkno) * sb.fsize) as u64;
			let csb: Superblock = self.file.decode_at(addr).unwrap();
			if csb.magic != FS_UFS2_MAGIC {
				eprintln!("CG{i} has invalid superblock magic: {:x}", csb.magic);
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.cblkno) * sb.fsize) as u64;
			let cg: CylGroup = self.file.decode_at(addr).unwrap();
			if cg.magic != CG_MAGIC {
				eprintln!("CG{i} has invalid cg magic: {:x}", cg.magic);
			}
		}
		println!("OK");

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
		ino: u64,
		_fh: u64,
		offset: i64,
		mut reply: fuser::ReplyDirectory,
	) {
		let ino = transino(ino);
		let f = || {
			if offset != 0 {
				return Ok(());
			}

			let ino = self.read_inode(ino)?;

			let mut i = 0;

			self.readdir(&ino, |name, ino, kind| {
				i += 1;
				if i > offset && reply.add(ino.into(), i, kind, name) {
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

	fn lookup(&mut self, _req: &Request<'_>, pino: u64, name: &OsStr, reply: fuser::ReplyEntry) {
		let pino = transino(pino);

		let f = || {
			let pino = self.read_inode(pino)?;

			let x = self.readdir(&pino, |name2, ino, _| {
				if name == name2 {
					Some(ino.into())
				} else {
					None
				}
			});

			match x {
				Ok(Some(inr)) => {
					let ino = self.read_inode(inr)?;
					Ok((ino.as_fileattr(inr), ino.gen))
				}
				Ok(None) => Err(IoError::new(ErrorKind::NotFound, "file not found")),
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
		ino: u64,
		_fh: u64,
		offset: i64,
		size: u32,
		_flags: i32,
		_lock_owner: Option<u64>,
		reply: fuser::ReplyData,
	) {
		let ino = transino(ino);

		let f = || {
			let mut buffer = vec![0u8; size as usize];
			let mut blockbuf = vec![0u8; self.superblock.bsize as usize];
			let ino = self.read_inode(ino)?;

			let mut offset = offset as u64;
			let mut boff = 0;
			let len = size as u64;
			let end = offset + len;

			while offset < end {
				let block = self.find_file_block(&ino, offset);
				let num = (block.size - block.off).min(end - offset);

				dbg!((&block, boff, num));
				self.read_file_block(&ino, block.blkidx, &mut blockbuf[0..(block.size as usize)])?;
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
		// TODO: scan bitmaps for bfree & ffree
		let sb = &self.superblock;
		let bfree = 0;
		let ffree = 0;
		reply.statfs(
			sb.dsize as u64,
			bfree,
			bfree,
			(sb.ipg * sb.ncg) as u64,
			ffree,
			sb.bsize as u32,
			255,
			sb.fsize as u32,
		)
	}

	fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: fuser::ReplyData) {
		let ino = transino(ino);
		let f = || {
			let ino = self.read_inode(ino)?;

			if ino.kind() != FileType::Symlink {
				return Err(IoError::new(ErrorKind::InvalidInput, "not a symlink"));
			}

			match &ino.data {
				InodeData::Shortlink(link) => {
					assert_eq!(ino.blocks, 0);
					let len = ino.size as usize;
					Ok(link[0..len].to_vec())
				}
				_ => Err(IoError::new(ErrorKind::Unsupported, "TODO: long links")),
			}
		};

		match run(f) {
			Ok(x) => reply.data(&x),
			Err(e) => reply.error(e),
		}
	}
}
