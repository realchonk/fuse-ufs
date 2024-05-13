use std::{mem::size_of, time::{Duration, SystemTime}};
use fuser::{FileAttr, FileType};

use crate::{UfsTime, UfsDaddr};

/**
 * External addresses in inode.
 */
const UFS_NXADDR: usize = 2;
/**
 * Direct addresses in inode.
 */
const UFS_NDADDR: usize = 12;
/**
 * Indirect addresses in inode.
 */
const UFS_NIADDR: usize = 3;

#[derive(Clone, Copy)]
#[allow(dead_code)]
#[repr(C)]
pub struct UfsInodeBlocks {
	pub direct: [UfsDaddr; UFS_NDADDR],
	pub indirect: [UfsDaddr; UFS_NIADDR],
}

#[allow(dead_code)]
#[repr(C)]
pub union UfsInodeData {
	pub blocks: UfsInodeBlocks,
	pub shortlink: [u8; (UFS_NDADDR + UFS_NIADDR) * size_of::<UfsDaddr>()],
}

#[allow(dead_code)]
#[repr(C)]
pub struct Inode {
	pub mode: u16,						//   0: IFMT, permissions; see below. 
	pub nlink: u16,						//   2: File link count. 
	pub uid: u32,						//   4: File owner. 
	pub gid: u32,						//   8: File group. 
	pub blksize: u32,					//  12: Inode blocksize. 
	pub size: u64,						//  16: File byte count. 
	pub blocks: u64,					//  24: Blocks actually held. 
	pub atime: UfsTime,					//  32: Last access time. 
	pub mtime: UfsTime,					//  40: Last modified time. 
	pub ctime: UfsTime,					//  48: Last inode change time. 
	pub birthtime: UfsTime,				//  56: Inode creation time. 
	pub mtimensec: u32,					//  64: Last modified time. 
	pub atimensec: u32,					//  68: Last access time. 
	pub ctimensec: u32,					//  72: Last inode change time. 
	pub birthnsec: u32,					//  76: Inode creation time. 
	pub gen: u32,						//  80: Generation number. 
	pub kernflags: u32,					//  84: Kernel flags. 
	pub flags: u32,						//  88: Status flags (chflags). 
	pub extsize: u32,					//  92: External attributes size. 
	pub extb: [UfsDaddr; UFS_NXADDR],	//  96: External attributes block. 
	pub data: UfsInodeData,				// XXX: Blocks
	pub modrev: u64,					// 232: i_modrev for NFSv4 
	pub ignored: u32,					// 240: (SUJ: Next unlinked inode) or (IFDIR: depth from root dir)
	pub ckhash: u32,					// 244: if CK_INODE, its check-hash 
	pub spare: [u32; 2],				// 248: Reserved; currently unused 
}

/// type of file mask
const S_IFMT: u16 = 0o170000;
/// named pipe (fifo)
const S_IFIFO: u16 = 0o010000;
/// character special
const S_IFCHR: u16 = 0o020000;
/// directory
const S_IFDIR: u16 = 0o040000;
/// block special
const S_IFBLK: u16 = 0o060000;
/// regular
const S_IFREG: u16 = 0o100000;
/// symbolic link
const S_IFLNK: u16 = 0o120000;
/// socket
const S_IFSOCK: u16 = 0o140000;

fn timetosys(mut s: UfsTime, ns: u32) -> SystemTime {
	let neg = s < 0;
	if neg {
		s = -s;
	}
	let dur = Duration::new(s as u64, ns);
	let mut time = SystemTime::UNIX_EPOCH;
	if neg {
		time -= dur;
	} else {
		time += dur;
	}
	time
}

impl Inode {
	pub fn atime(&self) -> SystemTime {
		timetosys(self.atime, self.atimensec)
	}
	pub fn mtime(&self) -> SystemTime {
		timetosys(self.mtime, self.mtimensec)
	}
	pub fn ctime(&self) -> SystemTime {
		timetosys(self.ctime, self.ctimensec)
	}
	pub fn btime(&self) -> SystemTime {
		timetosys(self.birthtime, self.birthnsec)
	}
	pub fn perm(&self) -> u16 {
		self.mode & 0o7777
	}
	pub fn kind(&self) -> FileType {
		let mode = self.mode & S_IFMT;
		match mode {
			S_IFIFO		=> FileType::NamedPipe,
			S_IFCHR		=> FileType::CharDevice,
			S_IFDIR		=> FileType::Directory,
			S_IFBLK		=> FileType::BlockDevice,
			S_IFREG		=> FileType::RegularFile,
			S_IFLNK		=> FileType::Symlink,
			S_IFSOCK	=> FileType::Socket,
			_			=> unreachable!("invalid file mode: {mode:o}"),
		}
	}

	pub fn as_fileattr(&self, ino: u64) -> FileAttr {
		FileAttr {
			ino,
			size: self.size,
			blocks: self.blocks,
			atime: self.atime(),
			mtime: self.mtime(),
			ctime: self.ctime(),
			crtime: self.btime(),
			kind: self.kind(),
			perm: self.perm(),
			nlink: self.nlink.into(),
			uid: self.uid,
			gid: self.gid,
			rdev: 0,
			blksize: self.blksize,
			flags: self.flags,
		}
	}
}

