use std::time::{Duration, SystemTime};

use bincode::{de::Decoder, error::DecodeError, Decode};
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

impl Decode for Inode {
	fn decode<D: Decoder>(d: &mut D) -> Result<Self, DecodeError> {
		let mode = u16::decode(d)?;
		let nlink = u16::decode(d)?;
		let uid = u32::decode(d)?;
		let gid = u32::decode(d)?;
		let blksize = u32::decode(d)?;
		let size = u64::decode(d)?;
		let blocks = u64::decode(d)?;
		let atime = UfsTime::decode(d)?;
		let mtime = UfsTime::decode(d)?;
		let ctime = UfsTime::decode(d)?;
		let birthtime = UfsTime::decode(d)?;
		let mtimensec = u32::decode(d)?;
		let atimensec = u32::decode(d)?;
		let ctimensec = u32::decode(d)?;
		let birthnsec = u32::decode(d)?;
		let gen = u32::decode(d)?;
		let kernflags = u32::decode(d)?;
		let flags = u32::decode(d)?;
		let extsize = u32::decode(d)?;
		let extb = <[UfsDaddr; UFS_NXADDR]>::decode(d)?;
		let data = if (mode & S_IFMT) == S_IFLNK && blocks == 0 {
			InodeData::Shortlink(Decode::decode(d)?)
		} else {
			InodeData::Blocks(InodeBlocks::decode(d)?)
		};

		let ino = Self {
			mode,
			nlink,
			uid,
			gid,
			blksize,
			size,
			blocks,
			atime,
			mtime,
			ctime,
			birthtime,
			mtimensec,
			atimensec,
			ctimensec,
			birthnsec,
			gen,
			kernflags,
			flags,
			extsize,
			extb,
			data,
			modrev: u64::decode(d)?,
			ignored: u32::decode(d)?,
			ckhash: u32::decode(d)?,
			spare: <[u32; 2]>::decode(d)?,
		};

		Ok(ino)
	}
}
