use std::{
	ffi::OsStr,
	fs::File,
	io::{Cursor, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
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
///
/// The value is a libc errno on every target. On non-Unix targets it is not a
/// native error code, so read it with `raw_os_error()`; `kind()` means nothing there.
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

	/// Size of the file system in fragments, metadata included (`fs_size`).
	pub frags: u64,

	/// The file system was unmounted cleanly (`fs_clean`).
	pub clean: bool,

	/// Status flags (`fs_flags`), as defined in FreeBSD's `ufs/ffs/fs.h`.
	pub flags: u32,

	/// File system id (`fs_id`).
	pub id: [u32; 2],

	/// Volume name (`fs_volname`), without the trailing NULs.
	pub label: Vec<u8>,

	/// Where the file system was last mounted (`fs_fsmnt`), without the trailing NULs.
	pub fsmnt: Vec<u8>,
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
		let cst = &sb.cstotal;
		Info {
			blocks: sb.dsize as u64,
			bfree:  cst
				.nbfree
				.saturating_mul(sb.frag as i64)
				.saturating_add(cst.nffree)
				.max(0) as u64,
			files:  sb.ipg as u64 * sb.ncg as u64,
			ffree:  cst.nifree.max(0) as u64,
			bsize:  sb.bsize as u32,
			fsize:  sb.fsize as u32,
			frags:  sb.size as u64,
			clean:  sb.clean != 0,
			flags:  sb.flags as u32,
			id:     [sb.id[0] as u32, sb.id[1] as u32],
			label:  until_nul(&sb.volname),
			fsmnt:  until_nul(&sb.fsmnt),
		}
	}

	fn check(&mut self) -> IoResult<()> {
		let sb = &self.superblock;
		log::debug!("Superblock: {sb:#?}");

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
		// MINBSIZE, MAXBSIZE, DEV_BSIZE
		sbassert!(sb.bsize >= 4096 && sb.bsize <= 65536);
		sbassert!(sb.fsize >= 512);
		sbassert!(sb.fsize == (sb.bsize / sb.frag));
		sbassert!(sb.size > 0 && sb.dsize > 0 && sb.dsize <= sb.size);
		// The last cylinder group may be partial, but not empty.
		sbassert!(sb.size > (sb.ncg as i64 - 1) * sb.fpg as i64);
		sbassert!(sb.size <= sb.ncg as i64 * sb.fpg as i64);
		sbassert!(sb.size.checked_mul(sb.fsize as i64).is_some());
		sbassert!(sb.sblkno >= 0 && sb.sblkno < sb.fpg);
		sbassert!(sb.cblkno >= 0 && sb.cblkno < sb.fpg);
		sbassert!(sb.iblkno >= 0 && sb.iblkno < sb.fpg);
		// TODO: this looks ugly:
		sbassert!(Some(sb.bsize) == 1i32.checked_shl(sb.bshift as u32));
		sbassert!(Some(sb.fsize) == 1i32.checked_shl(sb.fshift as u32));
		sbassert!(Some(sb.frag) == 1i32.checked_shl(sb.fragshift as u32));
		sbassert!(sb.bmask == !(sb.bsize - 1));
		sbassert!(sb.fmask == !(sb.fsize - 1));
		// The inode blocks of a cylinder group fit inside it, and its data follows them.
		sbassert!(sb.inopb == sb.bsize as u32 / UFS_INOSZ as u32);
		sbassert!(sb.ipg.is_multiple_of(sb.inopb));
		let inode_frags = (sb.ipg / (sb.inopb >> sb.fragshift)) as i64;
		sbassert!(sb.iblkno as i64 + inode_frags <= sb.fpg as i64);
		sbassert!(sb.dblkno as i64 == sb.iblkno as i64 + inode_frags);
		// sizeof(struct fs)..=SBLOCKSIZE; newfs rounds sbsize up to a fragment.
		sbassert!(sb.sbsize >= 1376 && sb.sbsize <= 8192);
		sbassert!(sb.cgsize_struct() < sb.bsize as usize);

		log::info!("Summary:");
		log::info!("Block Size: {}", sb.bsize);
		log::info!("# Blocks: {}", sb.size);
		log::info!("# Data Blocks: {}", sb.dsize);
		log::info!("Fragment Size: {}", sb.fsize);
		log::info!("Fragments per Block: {}", sb.frag);
		log::info!("# Cylinder Groups: {}", sb.ncg);
		log::info!("CG Size: {}MiB", sb.cgsize() / 1024 / 1024);

		// Only the allocator uses the cylinder groups.
		if !self.write_enabled() {
			log::info!("OK (cylinder groups not checked)");
			return Ok(());
		}

		let fpg = sb.fpg as u64;
		let sblkno = sb.sblkno as u64;
		let fs = sb.fsize as u64;

		// check that all superblocks are ok.
		for i in 0..sb.ncg {
			let addr = (i as u64 * fpg + sblkno) * fs;
			let csb: Superblock = self.file.decode_at(addr)?;
			if csb.magic != FS_UFS2_MAGIC {
				log::error!("CG{i} has invalid superblock magic: {:x}", csb.magic);
				return Err(err!(EIO));
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let addr = self.cg_addr(i as u64);
			let cg: CylGroup = self.file.decode_at(addr)?;
			if cg.magic != CG_MAGIC {
				log::error!("CG{i} has invalid cg magic: {:x}", cg.magic);
				return Err(err!(EIO));
			}
		}
		log::info!("OK");
		Ok(())
	}

	fn cg_addr(&self, idx: u64) -> u64 {
		let sb = &self.superblock;
		let fpg = sb.fpg as u64;
		let cblkno = sb.cblkno as u64;
		let fs = sb.fsize as u64;

		(idx * fpg + cblkno) * fs
	}

	fn update_sb(&mut self, f: impl FnOnce(&mut Superblock)) -> IoResult<()> {
		// Only update the first superblock, because we're lazy.
		f(&mut self.superblock);
		self.file.encode_at(SBLOCK_UFS2 as u64, &self.superblock)?;
		Ok(())
	}
}

fn check_name_is_legal(name: &[u8], allow_special: bool) -> IoResult<()> {
	let x = name.contains(&b'/') ||
		(name == b"." && !allow_special) ||
		(name == b".." && !allow_special) ||
		name.contains(&b'\0');

	if x {
		Err(err!(EINVAL))
	} else {
		Ok(())
	}
}

fn until_nul(b: &[u8]) -> Vec<u8> {
	b.iter()
		.position(|&c| c == 0)
		.map_or(b, |n| &b[..n])
		.to_vec()
}

/// A small file system in memory, for tests.
#[cfg(test)]
mod testfs {
	use super::*;

	/// The whole file system of [`superblock()`].
	const IMGSIZE: usize = 256 * 4096;

	/// A valid superblock: one cylinder group of 256 fragments of 4096 bytes,
	/// laid out as newfs does, with 256 inodes.
	pub(super) fn superblock() -> Superblock {
		let mut sb: Superblock = Decoder::new(Cursor::new([0u8; SBLOCKSIZE]), Config::little())
			.decode()
			.unwrap();
		sb.magic = FS_UFS2_MAGIC;
		sb.ncg = 1;
		sb.bsize = 32768;
		sb.fsize = 4096;
		sb.frag = 8;
		sb.bshift = 15;
		sb.fshift = 12;
		sb.fragshift = 3;
		sb.bmask = !(sb.bsize - 1);
		sb.fmask = !(sb.fsize - 1);
		sb.sbsize = 4096;
		sb.nindir = 4096;
		sb.inopb = 128;
		sb.ipg = 256;
		sb.fpg = 256;
		sb.sblkno = 24;
		sb.cblkno = 32;
		sb.iblkno = 40;
		sb.dblkno = 56;
		sb.size = 256;
		sb.dsize = 192;
		sb
	}

	/// An image with the superblock `sb` and nothing else.
	pub(super) fn image(sb: &Superblock) -> Vec<u8> {
		let mut img = vec![0u8; IMGSIZE];
		Decoder::new(Cursor::new(&mut img[..]), Config::little())
			.encode_at(SBLOCK_UFS2 as u64, sb)
			.unwrap();
		img
	}

	/// Write `ino` as inode `inr` into `img`, an image of [`superblock()`].
	pub(super) fn put_inode(img: &mut [u8], inr: InodeNum, ino: &Inode) {
		let off = superblock().ino_to_fso(inr);
		Decoder::new(Cursor::new(img), Config::little())
			.encode_at(off, ino)
			.unwrap();
	}

	/// Open `img` read-only.
	pub(super) fn open(img: Vec<u8>) -> IoResult<Ufs<Cursor<Vec<u8>>>> {
		// Evaluate the arguments of every log call, as with a logger installed.
		log::set_max_level(log::LevelFilter::Trace);
		Ufs::new(BlockReader::new(Cursor::new(img), 4096, false))
	}
}

#[cfg(test)]
mod t {
	use super::{testfs::*, *};

	/// The errno of opening a file system with the superblock `sb`.
	fn open_errno(sb: &Superblock) -> Option<i32> {
		open(image(sb)).err().and_then(|e| e.raw_os_error())
	}

	#[test]
	fn valid_superblock() {
		let ufs = open(image(&superblock())).unwrap();
		assert_eq!(ufs.info().frags, 256);
	}

	#[test]
	fn negative_fpg() {
		let mut sb = superblock();
		sb.fpg = -1;
		assert_eq!(open_errno(&sb), Some(libc::EIO));
	}

	/// The inode blocks of a cylinder group must fit inside it.
	#[test]
	fn inode_area() {
		let cases: [fn(&mut Superblock); 4] = [
			|sb| sb.inopb = 64,
			|sb| sb.ipg = 200,
			|sb| sb.dblkno = 48,
			|sb| {
				sb.ipg = 4096;
				sb.dblkno = 40 + 256;
			},
		];
		for (i, f) in cases.into_iter().enumerate() {
			let mut sb = superblock();
			f(&mut sb);
			assert_eq!(open_errno(&sb), Some(libc::EIO), "case {i}");
		}
	}
}
