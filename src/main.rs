use crate::data::*;
use anyhow::{bail, Context, Result};
use fuser::{Filesystem, KernelConfig, Request};
use std::{
	ffi::c_int,
	fs::File,
	io::{Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::{size_of, transmute_copy},
	path::{Path, PathBuf},
	process::Command,
	thread::sleep,
	time::Duration,
};

mod data;
mod inode;

fn howmany(x: usize, y: usize) -> usize {
	(x + (y - 1)) / y
}

impl Superblock {
	/// Calculate the size of a cylinder group.
	fn cgsize(&self) -> u64 {
		self.fpg as u64 * self.fsize as u64
	}
	/// Calculate the size of a cylinder group structure.
	fn cgsize_struct(&self) -> usize {
		size_of::<CylGroup>()
			+ howmany(self.fpg as usize, 8)
			+ howmany(self.ipg as usize, 8)
			+ size_of::<i32>()
			+ (if self.contigsumsize <= 0 {
				0usize
			} else {
				self.contigsumsize as usize * size_of::<i32>()
					+ howmany(self.fpg as usize >> (self.fshift as usize), 8)
			})
	}
}

pub struct Ufs {
	file: File,
	superblock: Superblock,
}

impl Ufs {
	pub fn open(path: PathBuf) -> Result<Self> {
		let mut file = File::options()
			.read(true)
			.write(false)
			.open(path)
			.context("failed to open device")?;
		let mut block = [0u8; SBLOCKSIZE];
		file.seek(SeekFrom::Start(SBLOCK_UFS2 as u64))
			.context("failed to seek to superblock")?;
		file.read_exact(&mut block)
			.context("failed to read superblock")?;
		let superblock: Superblock = unsafe { transmute_copy(&block) };
		if superblock.magic != FS_UFS2_MAGIC {
			bail!("invalid superblock magic number: {}", superblock.magic);
		}
		assert_eq!(superblock.cgsize, CGSIZE as i32);
		Ok(Self { file, superblock })
	}

	fn read(&mut self, off: u64, buf: &mut [u8]) -> IoResult<()> {
		let bs = self.superblock.fsize as u64;
		let blkno = off / bs;
		let blkoff = off % bs;
		let blkcnt = ((buf.len() as u64) + blkoff + bs - 1) / bs;
		let buflen = (blkcnt * bs) as usize;
		let mut buffer = Vec::with_capacity(buflen);
		buffer.resize(buflen, 0u8);

		self.file.seek(SeekFrom::Start(blkno * bs))?;
		self.file.read_exact(&mut buffer)?;

		let begin = blkoff as usize;
		let end = begin + buf.len();
		buf.copy_from_slice(&buffer[begin..end]);
		Ok(())
	}

	fn read_inode(&mut self, ino: u64) -> IoResult<Inode> {
		let sb = &self.superblock;
		let cg = ino / sb.ipg as u64;
		let cgoff = cg * sb.cgsize();
		let off = cgoff + (sb.iblkno as u64 * sb.fsize as u64) + (ino * size_of::<Inode>() as u64);
		let mut buffer = [0u8; size_of::<Inode>()];
		self.read(off, &mut buffer)?;
		let ino = unsafe { transmute_copy(&buffer) };

		Ok(ino)
	}
	fn read_file_block(&mut self, ino: u64, blkno: usize, buf: &mut [u8; 4096]) -> IoResult<()> {
		let bs = self.superblock.fsize as u64;
		let ino = self.read_inode(ino)?;

		if blkno >= ino.blocks as usize {
			return Err(IoError::new(ErrorKind::InvalidInput, "out of bounds"));
		}

		if blkno < UFS_NDADDR {
			let blkaddr = unsafe { ino.data.blocks.direct[blkno] } as u64;
			self.file.seek(SeekFrom::Start(blkaddr * bs))?;
			self.file.read_exact(buf)?;
			Ok(())
		} else {
			todo!("indirect block addressing is unsupported")
		}
	}
}

fn transino(ino: u64) -> u64 {
	return if ino == fuser::FUSE_ROOT_ID { 2 } else { ino };
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
			let addr = ((sb.fpg + sb.sblkno) * sb.fsize) as u64;
			let mut block = [0u8; SBLOCKSIZE];
			self.file.seek(SeekFrom::Start(addr)).unwrap();
			self.file.read_exact(&mut block).unwrap();
			let csb: Superblock = unsafe { transmute_copy(&block) };
			if csb.magic != FS_UFS2_MAGIC {
				eprintln!("CG{i} has invalid superblock magic: {:x}", csb.magic);
			}
		}

		// check that all cylgroups are ok.
		for i in 0..sb.ncg {
			let addr = ((sb.fpg + sb.cblkno) * sb.fsize) as u64;
			let mut block = [0u8; CGSIZE];
			self.file.seek(SeekFrom::Start(addr)).unwrap();
			self.file.read_exact(&mut block).unwrap();
			let cg: CylGroup = unsafe { transmute_copy(&block) };
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
			Err(e) => reply.error(e.raw_os_error().unwrap()),
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
	fn readdir(
		&mut self,
		_req: &Request<'_>,
		ino: u64,
		_fh: u64,
		_offset: i64,
		reply: fuser::ReplyDirectory,
	) {
		// TODO
		let _ino = transino(ino);
		reply.ok()
	}
}

fn shell(cmd: &str) {
	Command::new("sh")
		.args(&["-c", cmd])
		.spawn()
		.unwrap()
		.wait()
		.unwrap();
}

fn main() -> Result<()> {
	env_logger::init();

	assert_eq!(size_of::<Superblock>(), 1376);
	assert_eq!(size_of::<Inode>(), 256);
	let fs = Ufs::open(PathBuf::from("/dev/da0"))?;
	let mp = Path::new("mp");
	let options = &[];

	let mount = fuser::spawn_mount2(fs, mp, options)?;
	sleep(Duration::new(1, 0));
	shell("ls -ld mp");
	shell("ls -l mp");
	sleep(Duration::new(1, 0));
	drop(mount);

	Ok(())
}
