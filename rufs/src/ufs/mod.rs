use std::{
	ffi::{OsStr, OsString},
	fs::File,
	io::{Cursor, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	os::unix::ffi::{OsStrExt, OsStringExt},
	path::Path,
};

mod balloc;
mod dir;
mod ialloc;
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

/// Berkley Unix (Fast) Filesystem
pub struct Ufs<R: Backend> {
	file:       Decoder<BlockReader<R>>,
	superblock: Superblock,
	version:    UfsVersion,
}

impl Ufs<File> {
	pub fn open(path: &Path, rw: bool) -> IoResult<Self> {
		let file = BlockReader::open(path, rw)?;
		Self::new(file)
	}
}

impl<R: Backend> Ufs<R> {
	pub fn new(mut file: BlockReader<R>) -> IoResult<Self> {
		// Try multiple superblock locations in order
		let search_order = [
			(SBLOCK_UFS2, UfsVersion::V2),
			(SBLOCK_UFS1, UfsVersion::V1),
			(SBLOCK_FLOPPY, UfsVersion::V1),
			(SBLOCK_PIGGY, UfsVersion::V1),
		];

		for &(offset, version) in &search_order {
			// Seek to magic number location
			let magic_pos = offset as u64 + MAGIC_OFFSET;
			if file.seek(SeekFrom::Start(magic_pos)).is_err() {
				continue;
			}

			let mut magic_bytes = [0u8; 4];
			if file.read_exact(&mut magic_bytes).is_err() {
				continue;
			}

			// Determine endianness and validate magic
			let config = match (version, magic_bytes) {
				// UFSv2 detection
				(UfsVersion::V2, [0x19, 0x01, 0x54, 0x19]) => Some(Config::little()),
				(UfsVersion::V2, [0x19, 0x54, 0x01, 0x19]) => Some(Config::big()),

				// UFSv1 detection - standard magic
				(UfsVersion::V1, [0x54, 0x19, 0x01, 0x00]) => Some(Config::little()),
				(UfsVersion::V1, [0x00, 0x01, 0x19, 0x54]) => Some(Config::big()),

				// HP-UX LFN variant: 0x00095014
				(UfsVersion::V1, [0x14, 0x50, 0x09, 0x00]) => Some(Config::little()),
				(UfsVersion::V1, [0x00, 0x09, 0x50, 0x14]) => Some(Config::big()),

				// HP-UX Security variant: 0x00612195
				(UfsVersion::V1, [0x95, 0x21, 0x61, 0x00]) => Some(Config::little()),
				(UfsVersion::V1, [0x00, 0x61, 0x21, 0x95]) => Some(Config::big()),

				// HP-UX Features variant: 0x00195612
				(UfsVersion::V1, [0x12, 0x56, 0x19, 0x00]) => Some(Config::little()),
				(UfsVersion::V1, [0x00, 0x19, 0x56, 0x12]) => Some(Config::big()),

				// HP-UX 4GB+ variant: 0x05231994
				(UfsVersion::V1, [0x94, 0x19, 0x23, 0x05]) => Some(Config::little()),
				(UfsVersion::V1, [0x05, 0x23, 0x19, 0x94]) => Some(Config::big()),

				_ => None,
			};

			if let Some(config) = config {
				let mut decoder = Decoder::new(file, config);

				// Read and validate superblock
				let superblock_result = match version {
					UfsVersion::V1 => {
						decoder
							.decode_at::<SuperblockV1>(offset as u64)
							.ok()
							.filter(|sb| Self::is_valid_ufs1_magic(sb.magic))
							.map(Superblock::V1)
					}
					UfsVersion::V2 => {
						decoder
							.decode_at::<SuperblockV2>(offset as u64)
							.ok()
							.filter(|sb| sb.magic == FS_UFS2_MAGIC)
							.map(Superblock::V2)
					}
				};

				if let Some(superblock) = superblock_result {
					let mut ufs = Self {
						file: decoder,
						superblock,
						version,
					};
					if ufs.check().is_ok() {
						return Ok(ufs);
					}
					// Extract file from failed UFS instance to try next location
					file = ufs.file.into_inner();
				} else {
					// Superblock validation failed, extract file to try next location
					file = decoder.into_inner();
				}
			}
		}

		iobail!(
			ErrorKind::InvalidInput,
			"No valid UFS superblock found at any known location"
		)
	}

	/// Check if a magic number is valid for UFSv1
	fn is_valid_ufs1_magic(magic: i32) -> bool {
		matches!(
			magic,
			UFS1_MAGIC |
				UFS1_MAGIC_BE |
				UFS_MAGIC_LFN |
				UFS_MAGIC_SEC |
				UFS_MAGIC_FEA |
				UFS_MAGIC_4GB
		)
	}

	pub fn write_enabled(&self) -> bool {
		self.file.inner().write_enabled()
	}

	fn assert_rw(&self) -> IoResult<()> {
		if self.write_enabled() {
			Ok(())
		} else {
			Err(err!(EROFS))
		}
	}

	/// Get filesystem metadata.
	#[doc(alias("statfs", "statvfs"))]
	pub fn info(&self) -> Info {
		let sb = &self.superblock;
		match sb {
			Superblock::V1(sb1) => {
				let cst = &sb1.cstotal;
				Info {
					blocks: sb1.dsize as u64,
					bfree:  (cst.nbfree * sb1.frag as i32 + cst.nffree) as u64,
					files:  (sb1.ipg * sb1.ncg) as u64,
					ffree:  cst.nifree as u64,
					bsize:  sb1.bsize as u32,
					fsize:  sb1.fsize as u32,
				}
			}
			Superblock::V2(sb2) => {
				let cst = &sb2.cstotal;
				Info {
					blocks: sb2.dsize as u64,
					bfree:  (cst.nbfree * sb2.frag as i64 + cst.nffree) as u64,
					files:  (sb2.ipg * sb2.ncg) as u64,
					ffree:  cst.nifree as u64,
					bsize:  sb2.bsize as u32,
					fsize:  sb2.fsize as u32,
				}
			}
		}
	}

	fn check(&mut self) -> IoResult<()> {
		log::debug!("Superblock: {:#?}", self.superblock);
		log::info!("UFS Version: {:?}", self.version);
		log::info!("Summary:");
		log::info!("Block Size: {}", self.superblock.block_size());
		log::info!("Fragment Size: {}", self.superblock.fragment_size());
		log::info!(
			"# Cylinder Groups: {}",
			self.superblock.num_cylinder_groups()
		);
		log::info!("CG Size: {}MiB", self.superblock.cgsize() / 1024 / 1024);

		// Version-specific checks
		match &self.superblock {
			Superblock::V1(sb) => {
				log::info!("# Blocks: {}", sb.size);
				log::info!("# Data Blocks: {}", sb.dsize);
				log::info!("Fragments per Block: {}", sb.frag());

				// UFSv1 basic validation (less strict than V2)
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
				sbassert!(sb.fragments_per_group() > 0);
				sbassert!(sb.frag() > 0 && sb.frag() <= 8);
				sbassert!(sb.fragment_size() == (sb.block_size() / sb.frag() as u64));
				sbassert!(Some(sb.block_size() as i32) == 1i32.checked_shl(sb.bshift() as u32));
				sbassert!(Some(sb.fragment_size() as i32) == 1i32.checked_shl(sb.fshift() as u32));
				sbassert!(Some(sb.frag()) == 1i32.checked_shl(sb.fragshift() as u32));
				sbassert!(sb.block_size() as i32 == (!sb.bmask() + 1));
				sbassert!(sb.fragment_size() as i32 == (!sb.fmask() + 1));

				log::info!("UFSv1 basic checks passed");
			}
			Superblock::V2(sb) => {
				log::info!("# Blocks: {}", sb.size);
				log::info!("# Data Blocks: {}", sb.dsize);
				log::info!("Fragments per Block: {}", sb.frag());

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
				sbassert!(sb.fragments_per_group() > 0);
				sbassert!(sb.frag() > 0 && sb.frag() <= 8);
				sbassert!(sb.fragment_size() == (sb.block_size() / sb.frag() as u64));
				// TODO: this looks ugly:
				sbassert!(Some(sb.block_size() as i32) == 1i32.checked_shl(sb.bshift() as u32));
				sbassert!(Some(sb.fragment_size() as i32) == 1i32.checked_shl(sb.fshift() as u32));
				sbassert!(Some(sb.frag()) == 1i32.checked_shl(sb.fragshift() as u32));
				sbassert!(sb.block_size() as i32 == (!sb.bmask() + 1));
				sbassert!(sb.fragment_size() as i32 == (!sb.fmask() + 1));
				sbassert!(sb.sbsize() == sb.fragment_size() as i32);
				sbassert!(sb.cgsize_struct() < sb.bsize as usize);

				let fpg = sb.fragments_per_group() as u64;
				let sblkno = sb.sblkno as u64;
				let fs = sb.fragment_size();

				// check that all superblocks are ok.
				for i in 0..sb.ncg {
					let addr = (i as u64 * fpg + sblkno) * fs;
					let csb: SuperblockV2 = self.file.decode_at(addr).unwrap();
					if csb.magic != FS_UFS2_MAGIC {
						log::error!("CG{i} has invalid superblock magic: {:x}", csb.magic);
						return Err(err!(EIO));
					}
				}

				// check that all cylgroups are ok.
				for i in 0..sb.ncg {
					let addr = self.cg_addr(i as u64);
					let cg: CylGroup = self.file.decode_at(addr).unwrap();
					if cg.magic != CG_MAGIC {
						log::error!("CG{i} has invalid cg magic: {:x}", cg.magic);
						return Err(err!(EIO));
					}
				}
			}
		}

		log::info!("OK");
		Ok(())
	}

	fn cg_addr(&self, idx: u64) -> u64 {
		let sb = &self.superblock;
		let fpg = sb.fragments_per_group() as u64;
		let cblkno = sb.cblkno() as u64;
		let fs = sb.fragment_size();

		(idx * fpg + cblkno) * fs
	}

	fn update_sb(&mut self, f: impl FnOnce(&mut Superblock)) -> IoResult<()> {
		// Only update the first superblock, because we're lazy.
		f(&mut self.superblock);
		self.file.encode_at(SBLOCK_UFS2 as u64, &self.superblock)?;
		Ok(())
	}
}

fn check_name_is_legal(name: &OsStr, allow_special: bool) -> IoResult<()> {
	let b = name.as_encoded_bytes();

	let x = b.contains(&b'/') ||
		(name == "." && !allow_special) ||
		(name == ".." && !allow_special) ||
		b.contains(&b'\0');

	if x {
		Err(err!(EINVAL))
	} else {
		Ok(())
	}
}
