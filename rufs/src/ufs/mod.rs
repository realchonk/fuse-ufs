use std::{
	ffi::{OsStr, OsString},
	fs::File,
	io::{Cursor, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	num::NonZeroU64,
	os::unix::ffi::{OsStrExt, OsStringExt},
	path::Path,
};

#[cfg(test)]
mod alloctest;
mod balloc;
mod dir;
pub mod fsck;
mod ialloc;
mod inode;
mod symlink;
mod xattr;

pub use dir::DirEntry;

use crate::{
	blockreader::{Backend, BlockReader},
	buf::BufferCache,
	data::*,
	decoder::{Config, Decoder},
	geom::{AllocationSummary, CgNum},
	policy::CgSums,
	softdep::{DeferredQueue, DependencyEngine},
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

/// Build the metadata buffer cache for `sb`.
///
/// [`BufferCache`] asserts its geometry rather than returning an error, which
/// is the right behaviour for a constructor and the wrong one for a mount: a
/// corrupt superblock must be rejected with `EINVAL`, not panic the process.
/// So the two fields the cache depends on are checked here first, where the
/// error can still be turned into one.
fn metadata_cache(sb: &Superblock) -> IoResult<BufferCache> {
	let (bsize, fsize) = (sb.bsize(), sb.fsize());
	if bsize == 0 || fsize == 0 || bsize % fsize != 0 {
		iobail!(
			ErrorKind::InvalidInput,
			"invalid geometry for the metadata cache: fs_bsize={bsize}, fs_fsize={fsize}"
		);
	}
	Ok(BufferCache::new(bsize, fsize))
}

pub mod mapping;
pub mod runs;

pub mod meta;

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

	/// Runtime-only allocation bookkeeping.
	///
	/// Never serialized: see [`AllocationSummary`].  Keeping it here rather
	/// than in the on-disk superblock is what allows a read-only mount and
	/// keeps the image byte-identical to what `newfs` produces.
	alloc: AllocationSummary,

	/// Cached per-cylinder-group `csum` values.
	///
	/// The allocation policies need to compare *every* cylinder group's free
	/// counts, and re-reading a 32 KiB cylinder-group superblock per
	/// comparison would be absurd.  This cache is only ever updated by
	/// [`Ufs::write_cg`], so it cannot drift from the on-disk structures.
	cg_sums: CgSums,

	/// Cached UFS metadata, addressed in whole `fs_bsize` blocks.
	///
	/// This is a *metadata* cache and deliberately not a second file-data
	/// cache: ordinary file data is the FUSE kernel cache's business, and
	/// routing it through here as well would turn every page writeback into an
	/// ordering question for no benefit.  What lives here is the state Soft
	/// Updates is about — cylinder-group structs, bitmaps, inode blocks,
	/// indirect blocks and directory blocks.
	///
	/// Holding a block here does not mean it is on the disk.  A buffer is
	/// dirty as soon as it is modified and writable only once every range
	/// gated on it has opened, and [`crate::buf::BufferCache::write_back`] is
	/// the only thing that reaches the device.  All access goes through
	/// `Ufs::metadata_read` and friends, in [`mod@meta`].
	buf: BufferCache,

	/// Inode buffers held back until a directory entry is on the disk.
	///
	/// `(inode buffer block, the directory block holding the entry, the parent)`.
	/// See [`Self::block_inode_on_dir`].
	blocked_inodes: Vec<(u64, u64, InodeNum)>,

	/// Filesystem operations waiting for a dependency to resolve.
	///
	/// Distinct from [`Self::blocked_inodes`]: that holds *metadata* back, this
	/// holds *operations* back.
	deferred: DeferredQueue,

	/// Soft Updates dependency graph.
	///
	/// Owns the gates: byte ranges of cached buffers that may not be persisted
	/// yet, and the events that release them.  It is the reason the cache
	/// above holds dirty buffers at all — without it every metadata write
	/// would have to be flushed immediately to keep the image consistent, which
	/// is the behaviour this series exists to remove.  See
	/// [`crate::softdep`].
	softdep: DependencyEngine,
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
		let buf = metadata_cache(&superblock)?;
		let mut s = Self {
			file,
			superblock,
			alloc: AllocationSummary::new(0),
			cg_sums: CgSums::default(),
			buf,
			blocked_inodes: Vec::new(),
			deferred: DeferredQueue::new(),
			softdep: DependencyEngine::new(),
		};
		s.check()?;
		s.cg_sums = s.read_cg_sums()?;
		s.alloc = AllocationSummary::new(s.superblock.ncg);
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

		let fpg = sb.fpg as u64;
		let sblkno = sb.sblkno as u64;
		let fs = sb.fsize as u64;

		// check that all superblocks are ok.
		for i in 0..sb.ncg {
			let addr = (i as u64 * fpg + sblkno) * fs;
			let csb: Superblock = self.file.decode_at(addr).unwrap();
			if csb.magic != FS_UFS2_MAGIC {
				log::error!("CG{i} has invalid superblock magic: {:x}", csb.magic);
				return Err(err!(EIO));
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let addr = self.cg_addr(CgNum::new(i));
			let cg: CylGroup = self.file.decode_at(addr).unwrap();
			if cg.magic != CG_MAGIC {
				log::error!("CG{i} has invalid cg magic: {:x}", cg.magic);
				return Err(err!(EIO));
			}
		}
		log::info!("OK");
		Ok(())
	}

	fn cg_addr(&self, cg: CgNum) -> u64 {
		self.superblock.cg_addr(cg)
	}

	/// Gather the `csum` of every cylinder group.
	///
	/// Only needed at mount time, to seed the policy cache.  The cache itself
	/// lives in [`Self::cg_sums`] and is maintained by [`Ufs::write_cg`].
	///
	/// This reads through the metadata cache, for the same reason
	/// [`Ufs::read_cg`] does: comparing the policy cache against a *disk* view
	/// of the cylinder groups would report drift for every cylinder group that
	/// merely has an unflushed change.
	pub(super) fn read_cg_sums(&mut self) -> IoResult<CgSums> {
		let mut sums = Vec::with_capacity(self.superblock.ncg as usize);
		for i in 0..self.superblock.ncg {
			let cg = self.read_cg(CgNum::new(i))?;
			sums.push(cg.cs);
		}
		Ok(CgSums::new(&sums))
	}

	/// Runtime-only allocation bookkeeping.
	///
	/// Exposed for tests and for the eventual `tunefs`-style reporting; not
	/// part of the on-disk format.
	#[allow(dead_code)]
	pub(super) fn alloc_summary(&self) -> &AllocationSummary {
		&self.alloc
	}

	/// Record that an inode was allocated in `cg`.
	pub(super) fn note_inode_alloc(&mut self, cg: CgNum, is_dir: bool) {
		self.alloc.note_inode_alloc(cg, is_dir);
	}

	/// Update the primary superblock and stage it.
	///
	/// Staged like every other piece of metadata, which it did not used to be.
	/// `fs_cstotal` is a summary of the cylinder-group counters, so writing it
	/// straight to the device let the totals move while the cylinder groups it
	/// summarises were still sitting in a dirty buffer -- and a crash in between
	/// left `fs_cstotal` disagreeing with the bitmaps, which is the first thing
	/// the crash-point suite found.  `fsck` pass 5 reports it as
	/// "fs_cstotal.cs_nbfree is 48 but the cylinder-group bitmaps hold 49".
	///
	/// Staging alone is not the whole ordering -- the totals must not reach the
	/// disk *before* the cylinder groups they summarise -- but it puts both in
	/// the same write pass, in dirty order, and the CG block is always dirtied
	/// before this by every caller: `finish_alloc()` and `alloc_cg_inode()`
	/// stage the struct and bitmaps first and ask for the totals afterwards.
	fn update_sb(&mut self, f: impl FnOnce(&mut Superblock)) -> IoResult<()> {
		// Only update the first superblock, because we're lazy.
		f(&mut self.superblock);
		let sb = self.superblock.clone();
		self.metadata_write(SBLOCK_UFS2 as u64, &sb)?;
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
