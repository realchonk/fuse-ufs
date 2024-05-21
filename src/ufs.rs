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

use crate::{blockreader::BlockReader, data::*, decoder::Decoder};

pub struct Ufs {
	file:       Decoder<BlockReader>,
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
		assert_eq!(superblock.cgsize, CGSIZE as i32);
		Ok(Self { file, superblock })
	}

	// TODO: bincodify inode
	fn read_inode(&mut self, ino: u64) -> IoResult<Inode> {
		let off = self.superblock.ino_to_fso(ino);

		let buffer: [u8; size_of::<Inode>()] = self.file.decode_at(off)?;

		Ok(unsafe { std::mem::transmute_copy(&buffer) })
	}

	fn resolve_file_block(&mut self, ino: &Inode, blkno: u64) -> IoResult<Option<NonZeroU64>> {
		let nd = UFS_NDADDR as u64;

		if blkno >= ino.blocks {
			return Err(IoError::new(ErrorKind::InvalidInput, "out of bounds"));
		}

		if blkno < nd {
			Ok(NonZeroU64::new(
				unsafe { ino.data.blocks.direct[blkno as usize] } as u64,
			))
		} else {
			todo!("indirect block addressing")
		}
	}

	// TODO: block size is not always 4096
	fn read_file_block(&mut self, ino: &Inode, blkno: u64) -> IoResult<[u8; 4096]> {
		match self.resolve_file_block(ino, blkno)? {
			Some(blkno) => {
				self.file
					.decode_at(blkno.get() * self.superblock.fsize as u64)
			}
			None => Ok([0u8; 4096]),
		}
	}

	fn readdir<T>(
		&mut self,
		ino: &Inode,
		mut f: impl FnMut(&OsStr, InodeNum, FileType) -> Option<T>,
	) -> IoResult<Option<T>> {
		for i in 0..ino.blocks {
			let block = self.read_file_block(ino, i)?;

			let x = readdir_block(&block, &mut f)?;
			if x.is_some() {
				return Ok(x);
			}
		}
		Ok(None)
	}
}

fn run<T>(f: impl FnOnce() -> IoResult<T>) -> Result<T, c_int> {
	f().map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
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
		match self.read_inode(ino) {
			Ok(x) => reply.attr(&Duration::ZERO, &x.as_fileattr(ino)),
			Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
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

	// TODO: support offset
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

			self.readdir(&ino, |name, ino, kind| {
				if reply.add(ino.into(), 123, kind, name) {
					todo!("What if the buffer is full?");
				}
				None::<()>
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
}
