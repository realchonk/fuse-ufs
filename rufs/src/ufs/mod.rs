use std::{
	ffi::{OsStr, OsString},
	fs::File,
	io::{Cursor, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	os::unix::ffi::{OsStrExt, OsStringExt},
	path::Path,
};

mod dir;
mod inode;
mod symlink;
mod xattr;

use crate::{
	blockreader::{Backend, BlockReader},
	data::*,
	decoder::{Config, Decoder},
};

/// (INTERNAL) Constructs an [`std::io::Error`] from an `errno`.
#[macro_export]
macro_rules! err {
	($name:ident) => {
		IoError::from_raw_os_error(libc::$name)
	};
}

macro_rules! iobail {
	($kind:expr, $($tk:tt)+) => {
		return Err(IoError::new($kind, format!($($tk)+)))
	};
}

/// Summary of filesystem statistics.
#[derive(Debug, Clone)]
#[doc(alias = "Statfs")]
pub struct Info {
	/// Number of blocks.
	pub blocks: u64,

	/// Number of free blocks.
	pub bfree: u64,

	/// Number of inodes (files).
	pub files: u64,

	/// Number of free inodes (files).
	pub ffree: u64,

	/// Block size.
	pub bsize: u32,

	/// Fragment size.
	pub fsize: u32,
}

/// Berkley Unix (Fast) Filesystem v2
pub struct Ufs<R: Backend> {
	file:       Decoder<BlockReader<R>>,
	superblock: Superblock,
}

impl Ufs<File> {
	pub fn open(path: &Path, rw: bool) -> IoResult<Self> {
		let file = BlockReader::open(path, rw)?;
		Self::new(file)
	}
}

impl<R: Backend> Ufs<R> {
	pub fn new(mut file: BlockReader<R>) -> IoResult<Self> {
		let pos = SBLOCK_UFS2 as u64 + MAGIC_OFFSET;
		file.seek(SeekFrom::Start(pos))?;
		let mut magic = [0u8; 4];
		file.read_exact(&mut magic)?;

		// magic: 0x19 54 01 19
		let config = match magic {
			[0x19, 0x01, 0x54, 0x19] => Config::little(),
			[0x19, 0x54, 0x01, 0x19] => Config::big(),
			_ => {
				iobail!(
					ErrorKind::InvalidInput,
					"invalid superblock magic number: {magic:?}"
				)
			}
		};
		// FIXME: Choose based on hash of input or so, to excercise BE as well with introducing non-determinism

		let mut file = Decoder::new(file, config);

		let superblock: Superblock = file.decode_at(SBLOCK_UFS2 as u64)?;
		if superblock.magic != FS_UFS2_MAGIC {
			iobail!(
				ErrorKind::InvalidInput,
				"invalid superblock magic number: {}",
				superblock.magic
			);
		}
		let mut s = Self { file, superblock };
		s.check()?;
		Ok(s)
	}

	pub fn write_enabled(&self) -> bool {
		self.file.inner().write_enabled()
	}

	/// Get filesystem metadata.
	#[doc(alias("statfs", "statvfs"))]
	pub fn info(&self) -> Info {
		let sb = &self.superblock;
		let cst = &sb.cstotal;
		Info {
			blocks: sb.dsize as u64,
			bfree:  (cst.nbfree * sb.frag as i64 + cst.nffree) as u64,
			files:  (sb.ipg * sb.ncg) as u64,
			ffree:  cst.nifree as u64,
			bsize:  sb.bsize as u32,
			fsize:  sb.fsize as u32,
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

		macro_rules! sbassert {
			($e:expr) => {
				if !($e) {
					log::error!("superblock corrupted: {}", stringify!($e));
					return Err(IoError::from_raw_os_error(libc::EIO));
				}
			};
		}

		sbassert!(sb.ncg > 0);
		sbassert!(sb.ipg > 0);
		sbassert!(sb.fpg > 0);
		sbassert!(sb.frag > 0 && sb.frag <= 8);
		sbassert!(sb.fsize == (sb.bsize / sb.frag));
		// TODO: this looks ugly:
		sbassert!(Some(sb.bsize) == 1i32.checked_shl(sb.bshift as u32));
		sbassert!(Some(sb.fsize) == 1i32.checked_shl(sb.fshift as u32));
		sbassert!(Some(sb.frag) == 1i32.checked_shl(sb.fragshift as u32));
		sbassert!(sb.bsize == (!sb.bmask + 1));
		sbassert!(sb.fsize == (!sb.fmask + 1));
		sbassert!(sb.sbsize == sb.fsize);
		sbassert!(sb.cgsize_struct() < sb.bsize as usize);

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
