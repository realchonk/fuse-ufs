use std::{
	ffi::{c_int, OsStr, OsString},
	io::{Cursor, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	os::unix::ffi::{OsStrExt, OsStringExt},
	path::Path,
	time::Duration,
};

use anyhow::{bail, Result};
use fuser::{FileType, Filesystem, KernelConfig, Request};

const MAX_CACHE: Duration = Duration::MAX;

mod xattr;
mod symlink;
mod inode;
mod dir;

use crate::{
	blockreader::BlockReader,
	data::*,
	decoder::{Config, Decoder},
};

#[macro_export]
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

			let mut i = 0;

			self.dir_iter(inr, |name, inr, kind| {
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

		let mut f = || {
			let inr = self.dir_lookup(pinr, name)?;
			let ino = self.read_inode(inr)?;
			Ok::<_, IoError>((ino.as_fileattr(inr), ino.gen))
		};

		match f() {
			Ok((attr, gen)) => reply.entry(&Duration::ZERO, &attr, gen.into()),
			Err(e) => {
				if e.kind() != ErrorKind::NotFound {
					log::error!("Error: {e}");
				}
				reply.error(e.raw_os_error().unwrap_or(libc::EIO))
			}
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
		match run(|| self.symlink_read(inr)) {
			Ok(x) => reply.data(&x),
			Err(e) => reply.error(e),
		}
	}

	fn listxattr(&mut self, _req: &Request<'_>, inr: u64, size: u32, reply: fuser::ReplyXattr) {
		let inr = transino(inr);

		enum R {
			Len(u32),
			Data(Vec<u8>),
		}

		let f = || {
			if size == 0 {
				let len = self.xattr_list_len(inr)?;
				Ok(R::Len(len))
			} else {
				let data = self.xattr_list(inr)?;
				Ok(R::Data(data))
			}
		};

		match run(f) {
			Ok(R::Data(data)) => reply.data(&data),
			Ok(R::Len(len)) => reply.size(len),
			Err(e) => reply.error(e),
		}
	}

	fn getxattr(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		name: &OsStr,
		size: u32,
		reply: fuser::ReplyXattr,
	) {
		let inr = transino(inr);

		enum R {
			Data(Vec<u8>),
			TooShort,
			Len(u32),
		}

		let f = || {
			if size == 0 {
				let len = self.xattr_len(inr, name)?;
				Ok(R::Len(len))
			} else {
				let data = self.xattr_read(inr, name)?;
				if (size as usize) >= data.len() {
					Ok(R::Data(data))
				} else {
					Ok(R::TooShort)
				}
			}
		};

		match run(f) {
			Ok(R::Data(x)) => reply.data(&x),
			Ok(R::TooShort) => reply.error(libc::ERANGE),
			Ok(R::Len(l)) => reply.size(l),
			Err(e) => reply.error(e),
		}
	}
}
