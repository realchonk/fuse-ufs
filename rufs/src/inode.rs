use std::time::{Duration, SystemTime};

use bincode::{de::Decoder, error::DecodeError, Decode};

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

	pub fn kind(&self) -> InodeType {
		let mode = self.mode & S_IFMT;
		match mode {
			S_IFIFO => InodeType::NamedPipe,
			S_IFCHR => InodeType::CharDevice,
			S_IFDIR => InodeType::Directory,
			S_IFBLK => InodeType::BlockDevice,
			S_IFREG => InodeType::RegularFile,
			S_IFLNK => InodeType::Symlink,
			S_IFSOCK => InodeType::Socket,
			_ => unreachable!("invalid file mode: {mode:o}"),
		}
	}

	pub fn as_attr(&self, inr: InodeNum) -> InodeAttr {
		InodeAttr {
			inr,
			perm: self.mode & 0o7777,
			kind: self.kind(),
			size: self.size,
			blocks: self.blocks,
			atime: self.atime(),
			mtime: self.mtime(),
			ctime: self.ctime(),
			btime: self.btime(),
			nlink: self.nlink,
			uid: self.uid,
			gid: self.gid,
			gen: self.gen,
			blksize: self.blksize,
			flags: self.flags,
			kernflags: self.kernflags,
			extsize: self.extsize,
		}
	}

	pub fn size(&self, bs: u64, fs: u64) -> (u64, u64) {
		let size = match self.kind() {
			InodeType::Directory => self.blocks * fs,
			InodeType::RegularFile | InodeType::Symlink => self.size,
			kind => todo!("Inode::size() is undefined for {kind:?}"),
		};
		Self::inode_size(bs, fs, size)
	}

	/// The number of blocks and fragments this inode needs.
	fn inode_size(bs: u64, fs: u64, size: u64) -> (u64, u64) {
		let blocks = size / bs;
		let frags = (size % bs).div_ceil(fs);

		(blocks, frags)
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

mod test {
	#[test]
	fn inode_size() {
		let bs = 32768;
		let fs = 4096;

		let isz = |sz| super::Inode::inode_size(bs, fs, sz);

		assert_eq!(isz(0), (0, 0));
		assert_eq!(isz(1), (0, 1));
		assert_eq!(isz(fs), (0, 1));
		assert_eq!(isz(bs), (1, 0));
		assert_eq!(isz(bs + 2 * fs), (1, 2));
		assert_eq!(isz(100 * bs + 7 * fs), (100, 7));
	}
}

#[cfg(feature = "fuser")]
mod f {
	use fuser::{FileAttr, FileType};

	use super::*;

	impl Into<FileType> for InodeType {
		fn into(self) -> FileType {
			match self {
				Self::RegularFile => FileType::RegularFile,
				Self::Directory => FileType::Directory,
				Self::Symlink => FileType::Symlink,
				Self::Socket => FileType::Socket,
				Self::CharDevice => FileType::CharDevice,
				Self::BlockDevice => FileType::BlockDevice,
				Self::NamedPipe => FileType::NamedPipe,
			}
		}
	}

	impl Into<FileAttr> for InodeAttr {
		fn into(self) -> FileAttr {
			FileAttr {
				ino:     self.inr.get64(),
				size:    self.size,
				blocks:  self.blocks,
				atime:   self.atime,
				mtime:   self.mtime,
				ctime:   self.ctime,
				crtime:  self.atime,
				kind:    self.kind.into(),
				perm:    self.perm,
				nlink:   self.nlink.into(),
				uid:     self.uid,
				gid:     self.gid,
				rdev:    0,
				blksize: self.blksize,
				flags:   self.flags,
			}
		}
	}
}
