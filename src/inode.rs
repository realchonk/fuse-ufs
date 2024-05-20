use std::time::{Duration, SystemTime};

use fuser::{FileAttr, FileType};

use crate::data::*;

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
			S_IFIFO => FileType::NamedPipe,
			S_IFCHR => FileType::CharDevice,
			S_IFDIR => FileType::Directory,
			S_IFBLK => FileType::BlockDevice,
			S_IFREG => FileType::RegularFile,
			S_IFLNK => FileType::Symlink,
			S_IFSOCK => FileType::Socket,
			_ => unreachable!("invalid file mode: {mode:o}"),
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
