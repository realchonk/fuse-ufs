use std::{
	mem::{size_of, transmute_copy},
	time::{Duration, SystemTime},
};

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
		let data;
		let mut ino = Self {
			mode:      u16::decode(d)?,
			nlink:     u16::decode(d)?,
			uid:       u32::decode(d)?,
			gid:       u32::decode(d)?,
			blksize:   u32::decode(d)?,
			size:      u64::decode(d)?,
			blocks:    u64::decode(d)?,
			atime:     UfsTime::decode(d)?,
			mtime:     UfsTime::decode(d)?,
			ctime:     UfsTime::decode(d)?,
			birthtime: UfsTime::decode(d)?,
			mtimensec: u32::decode(d)?,
			atimensec: u32::decode(d)?,
			ctimensec: u32::decode(d)?,
			birthnsec: u32::decode(d)?,
			gen:       u32::decode(d)?,
			kernflags: u32::decode(d)?,
			flags:     u32::decode(d)?,
			extsize:   u32::decode(d)?,
			extb:      <[UfsDaddr; UFS_NXADDR]>::decode(d)?,
			data:      {
				data = <[u8; UFS_SLLEN]>::decode(d)?;
				InodeData::Shortlink([0; UFS_SLLEN])
			},
			modrev:    u64::decode(d)?,
			ignored:   u32::decode(d)?,
			ckhash:    u32::decode(d)?,
			spare:     <[u32; 2]>::decode(d)?,
		};

		if (ino.mode & S_IFMT) == S_IFLNK && ino.blocks == 0 {
			ino.data = InodeData::Shortlink(data);
		} else {
			const SZ: usize = size_of::<UfsDaddr>();
			let mut direct = [0u8; UFS_NDADDR * SZ];
			let mut indirect = [0u8; UFS_NIADDR * SZ];
			let len = direct.len();
			direct.copy_from_slice(&data[0..len]);
			indirect.copy_from_slice(&data[len..]);

			ino.data = InodeData::Blocks {
				direct:   unsafe { transmute_copy(&direct) },
				indirect: unsafe { transmute_copy(&indirect) },
			};
		}

		Ok(ino)
	}
}
