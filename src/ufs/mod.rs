use std::{
	ffi::{OsStr, OsString},
	io::{Cursor, Error as IoError, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	os::unix::ffi::{OsStrExt, OsStringExt},
	path::Path,
};
use fuser::FileType;

use anyhow::{bail, Result};

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

#[derive(Debug, Clone)]
pub struct Info {
	pub blocks: u64,
	pub bfree: u64,
	pub files: u64,
	pub ffree: u64,
	pub bsize: u32,
	pub fsize: u32,
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

		let mut s = Self {
			file,
			superblock,
		};
		s.check()?;
		Ok(s)
	}

	pub fn info(&self) -> Info {
		let sb = &self.superblock;
		let cst = &sb.cstotal;
		Info {
			blocks: sb.dsize as u64,
			bfree: (cst.nbfree * sb.frag as i64 + cst.nffree) as u64,
			files: (sb.ipg * sb.ncg) as u64,
			ffree: cst.nifree as u64,
			bsize: sb.bsize as u32,
			fsize: sb.fsize as u32,
		}
	}

	fn check(&mut self) -> IoResult<()> {
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
				return Err(err!(EIO));
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.cblkno) * sb.fsize) as u64;
			let cg: CylGroup = self.file.decode_at(addr).unwrap();
			if cg.magic != CG_MAGIC {
				log::error!("CG{i} has invalid cg magic: {:x}", cg.magic);
				return Err(err!(EIO));
			}
		}
		log::info!("OK");
		Ok(())
	}
}

