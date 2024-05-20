use std::{
	ffi::c_int,
	fs::File,
	io::{BufReader, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom},
	mem::size_of,
	path::PathBuf,
	time::Duration,
};

use anyhow::{bail, Context, Result};
use bincode::{config::{Configuration, Fixint, LittleEndian, NoLimit}, Decode};
use fuser::{Filesystem, KernelConfig, Request};

use crate::data::*;

pub struct Ufs {
	config: Configuration<LittleEndian, Fixint, NoLimit>,
	file:       BufReader<File>,
	superblock: Superblock,
}

impl Ufs {
	pub fn open(path: PathBuf) -> Result<Self> {
		let config = bincode::config::standard()
			.with_little_endian()
			.with_fixed_int_encoding();

		let file = File::options()
			.read(true)
			.write(false)
			.open(path)
			.context("failed to open device")?;

		let mut file = BufReader::new(file);

		file.seek(SeekFrom::Start(SBLOCK_UFS2 as u64))
			.context("failed to seek to superblock")?;

		let superblock: Superblock = bincode::decode_from_reader(&mut file, config)?;
		
		if superblock.magic != FS_UFS2_MAGIC {
			bail!("invalid superblock magic number: {}", superblock.magic);
		}
		assert_eq!(superblock.cgsize, CGSIZE as i32);
		Ok(Self { config, file, superblock })
	}

	fn decode<T: Decode>(&mut self, off: u64) -> Result<T> {
		self
			.file
			.seek(SeekFrom::Start(off))
			.context("failed to seek")?;
		let x = bincode::decode_from_reader(&mut self.file, self.config)
			.context("failed to decode")?;
		Ok(x)
	}

	// TODO: bincodify inode
	fn read_inode(&mut self, ino: u64) -> IoResult<Inode> {
		let sb = &self.superblock;
		let cg = ino / sb.ipg as u64;
		let cgoff = cg * sb.cgsize();
		let off = cgoff + (sb.iblkno as u64 * sb.fsize as u64) + (ino * size_of::<Inode>() as u64);

		let mut buffer = [0u8; size_of::<Inode>()];
		self.file.seek(SeekFrom::Start(off))?;
		self.file.read_exact(&mut buffer)?;

		Ok(unsafe { std::mem::transmute_copy(&buffer) })
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
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.sblkno) * sb.fsize) as u64;
			let csb: Superblock = self.decode(addr).unwrap();
			if csb.magic != FS_UFS2_MAGIC {
				eprintln!("CG{i} has invalid superblock magic: {:x}", csb.magic);
			}
		}

		// check that all cylgroups are ok.
		for i in 0..self.superblock.ncg {
			let sb = &self.superblock;
			let addr = ((sb.fpg + sb.cblkno) * sb.fsize) as u64;
			let cg: CylGroup = self.decode(addr).unwrap();
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
