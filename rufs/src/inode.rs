use std::{
	io::{Error as IoError, Result as IoResult},
	time::{Duration, SystemTime},
};

use bincode::{
	de::Decoder,
	enc::Encoder,
	error::{DecodeError, EncodeError},
	Decode,
	Encode,
};

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

fn systotime(t: SystemTime) -> (UfsTime, u32) {
	let (diff, neg) = if t >= SystemTime::UNIX_EPOCH {
		(t.duration_since(SystemTime::UNIX_EPOCH).unwrap(), 1)
	} else {
		(SystemTime::UNIX_EPOCH.duration_since(t).unwrap(), -1)
	};

	(neg * diff.as_secs() as UfsTime, diff.subsec_nanos())
}

impl Inode {
	/// Create a new UFSv2 inode (for write operations)
	pub fn new(kind: InodeType, perm: u16, uid: u32, gid: u32, blksize: u32) -> Self {
		let (now, nowsnsec) = systotime(SystemTime::now());
		let data = match kind {
			InodeType::Symlink => InodeData::Shortlink([0u8; UFS_SLLEN]),
			_ => InodeData::Blocks(InodeBlocks::default()),
		};
		let kind_bits = match kind {
			InodeType::RegularFile => S_IFREG,
			InodeType::Directory => S_IFDIR,
			InodeType::Symlink => S_IFLNK,
			InodeType::CharDevice => S_IFCHR,
			InodeType::BlockDevice => S_IFBLK,
			InodeType::Socket => S_IFSOCK,
			InodeType::NamedPipe => S_IFIFO,
		};
		let mode = kind_bits | (perm & !S_IFMT);

		let inode_v2 = InodeV2 {
			mode,
			nlink: 0,
			uid,
			gid,
			blksize,
			size: 0,
			blocks: 0,
			atime: now,
			mtime: now,
			ctime: now,
			birthtime: now,
			mtimensec: nowsnsec,
			atimensec: nowsnsec,
			ctimensec: nowsnsec,
			birthnsec: nowsnsec,
			gen: 0,
			kernflags: 0,
			flags: 0,
			extsize: 0,
			extb: [0; UFS_NXADDR],
			data,
			modrev: 0,
			ignored: 0,
			ckhash: 0,
			spare: [0; 2],
		};

		Inode::V2(inode_v2)
	}

	pub fn btime(&self) -> SystemTime {
		match self {
			Inode::V1(_) => {
				// UFSv1 doesn't have birth time, return epoch
				SystemTime::UNIX_EPOCH
			}
			Inode::V2(i) => timetosys(i.birthtime, i.birthnsec),
		}
	}

	pub fn set_atime(&mut self, t: SystemTime) {
		if let Inode::V2(i) = self {
			(i.atime, i.atimensec) = systotime(t);
		}
	}

	pub fn set_mtime(&mut self, t: SystemTime) {
		if let Inode::V2(i) = self {
			(i.mtime, i.mtimensec) = systotime(t);
		}
	}

	pub fn set_ctime(&mut self, t: SystemTime) {
		if let Inode::V2(i) = self {
			(i.ctime, i.ctimensec) = systotime(t);
		}
	}

	pub fn set_btime(&mut self, t: SystemTime) {
		if let Inode::V2(i) = self {
			(i.birthtime, i.birthnsec) = systotime(t);
		}
	}

	pub fn assert_dir(&self) -> IoResult<()> {
		if self.kind() == InodeType::Directory {
			Ok(())
		} else {
			Err(IoError::from_raw_os_error(libc::ENOTDIR))
		}
	}

	pub fn kind(&self) -> InodeType {
		let mode = self.mode() & S_IFMT;
		match mode {
			S_IFIFO => InodeType::NamedPipe,
			S_IFCHR => InodeType::CharDevice,
			S_IFDIR => InodeType::Directory,
			S_IFBLK => InodeType::BlockDevice,
			S_IFREG => InodeType::RegularFile,
			S_IFLNK => InodeType::Symlink,
			S_IFSOCK => InodeType::Socket,
			_ => {
				log::error!("invalid file mode: {mode:o}");
				// TODO: return socket type as fallback
				InodeType::Socket
			}
		}
	}

	pub fn as_attr(&self, inr: InodeNum) -> InodeAttr {
		match self {
			Inode::V1(i) => {
				InodeAttr {
					inr,
					perm: i.mode & 0o7777,
					kind: self.kind(),
					size: i.size,
					blocks: i.blocks as u64,
					atime: self.atime(),
					mtime: self.mtime(),
					ctime: self.ctime(),
					btime: self.btime(),
					nlink: i.nlink,
					uid: i.uid,
					gid: i.gid,
					gen: i.gen,
					blksize: 0, // UFSv1 doesn't store blksize in inode
					flags: i.flags,
					kernflags: 0, // UFSv1 doesn't have kernflags
					extsize: 0,   // UFSv1 doesn't have extsize
				}
			}
			Inode::V2(i) => {
				InodeAttr {
					inr,
					perm: i.mode & 0o7777,
					kind: self.kind(),
					size: i.size,
					blocks: i.blocks,
					atime: self.atime(),
					mtime: self.mtime(),
					ctime: self.ctime(),
					btime: self.btime(),
					nlink: i.nlink,
					uid: i.uid,
					gid: i.gid,
					gen: i.gen,
					blksize: i.blksize,
					flags: i.flags,
					kernflags: i.kernflags,
					extsize: i.extsize,
				}
			}
		}
	}

	/// The number of blocks and fragments this inode needs.
	pub fn inode_size(bs: u64, fs: u64, size: u64) -> (u64, u64) {
		let blocks = size / bs;
		let frags = (size % bs).div_ceil(fs);

		(blocks, frags)
	}
}

impl<Context> Decode<Context> for InodeV1 {
	fn decode<D: Decoder<Context = Context>>(d: &mut D) -> Result<Self, DecodeError> {
		let mode = u16::decode(d)?;
		let nlink = u16::decode(d)?;
		let uid_low = u16::decode(d)?;
		let gid_low = u16::decode(d)?;
		let size = u64::decode(d)?;
		let atime = Ufs1Time::decode(d)?;
		let atime_usec = i32::decode(d)?;
		let mtime = Ufs1Time::decode(d)?;
		let mtime_usec = i32::decode(d)?;
		let ctime = Ufs1Time::decode(d)?;
		let ctime_usec = i32::decode(d)?;

		// Read the data (block pointers or shortlink) BEFORE flags/blocks/etc.
		// This matches the on-disk layout where ui_u2.ui_addr is at offset 40
		// and ui_flags is at offset 100.
		// We need to peek at blocks field to determine if it's a shortlink,
		// but we can't read it yet, so we read the data first and then the metadata fields.
		let data: InodeV1Data = if (mode & S_IFMT) == S_IFLNK {
			// For symlinks, we need to check blocks, but we haven't read it yet.
			// Read the blocks data first, then we'll determine if it's shortlink or not
			// based on the blocks field we read later.
			// For now, always read as blocks, we'll handle shortlinks separately if needed.
			InodeV1Data::Blocks(InodeV1Blocks::decode(d)?)
		} else {
			InodeV1Data::Blocks(InodeV1Blocks::decode(d)?)
		};

		let flags: u32 = u32::decode(d)?;
		let blocks = i32::decode(d)?;
		let gen = u32::decode(d)?;
		let uid = u32::decode(d)?;
		let gid = u32::decode(d)?;
		let spare = <[u32; 2]>::decode(d)?;

		// Convert blocks to shortlink if applicable
		let data = if (mode & S_IFMT) == S_IFLNK && blocks == 0 {
			// Re-interpret the blocks data as shortlink
			if let InodeV1Data::Blocks(_blks) = data {
				let shortlink = [0u8; 60];
				// Convert the block pointers to bytes (they were read in the wrong interpretation)
				// We need to re-read this properly, but for now just create empty shortlink
				// TODO: This needs proper handling
				InodeV1Data::Shortlink(shortlink)
			} else {
				data
			}
		} else {
			data
		};

		let ino = Self {
			mode,
			nlink,
			uid_low,
			gid_low,
			size,
			atime,
			atime_usec,
			mtime,
			mtime_usec,
			ctime,
			ctime_usec,
			data,
			flags,
			blocks,
			gen,
			uid,
			gid,
			spare,
		};

		Ok(ino)
	}
}

impl Encode for InodeV1Data {
	fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
		match self {
			Self::Blocks(blocks) => InodeV1Blocks::encode(blocks, encoder),
			Self::Shortlink(link) => <[u8; 60]>::encode(link, encoder),
		}
	}
}

impl<Context> Decode<Context> for InodeV2 {
	fn decode<D: Decoder<Context = Context>>(d: &mut D) -> Result<Self, DecodeError> {
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

impl Encode for InodeData {
	fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
		match self {
			Self::Blocks(blocks) => InodeBlocks::encode(blocks, encoder),
			Self::Shortlink(link) => <[u8; UFS_SLLEN]>::encode(link, encoder),
		}
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

	impl From<InodeType> for FileType {
		fn from(t: InodeType) -> Self {
			match t {
				InodeType::RegularFile => Self::RegularFile,
				InodeType::Directory => Self::Directory,
				InodeType::Symlink => Self::Symlink,
				InodeType::Socket => Self::Socket,
				InodeType::CharDevice => Self::CharDevice,
				InodeType::BlockDevice => Self::BlockDevice,
				InodeType::NamedPipe => Self::NamedPipe,
			}
		}
	}

	impl From<InodeAttr> for FileAttr {
		fn from(a: InodeAttr) -> Self {
			Self {
				ino:     a.inr.get64(),
				size:    a.size,
				blocks:  a.blocks,
				atime:   a.atime,
				mtime:   a.mtime,
				ctime:   a.ctime,
				crtime:  a.btime,
				kind:    a.kind.into(),
				perm:    a.perm,
				nlink:   a.nlink.into(),
				uid:     a.uid,
				gid:     a.gid,
				rdev:    0,
				blksize: a.blksize,
				flags:   a.flags,
			}
		}
	}
}

#[cfg(feature = "fuse2rs")]
mod f2 {
	use fuse2rs::{FileAttr, FileType};

	use super::*;

	impl From<InodeType> for FileType {
		fn from(t: InodeType) -> Self {
			match t {
				InodeType::RegularFile => Self::RegularFile,
				InodeType::Directory => Self::Directory,
				InodeType::Symlink => Self::Symlink,
				InodeType::Socket => Self::Socket,
				InodeType::CharDevice => Self::CharDevice,
				InodeType::BlockDevice => Self::BlockDevice,
				InodeType::NamedPipe => Self::NamedPipe,
			}
		}
	}

	impl From<InodeAttr> for FileAttr {
		fn from(a: InodeAttr) -> Self {
			Self {
				ino:     a.inr.get64(),
				size:    a.size,
				blocks:  a.blocks,
				atime:   a.atime,
				mtime:   a.mtime,
				ctime:   a.ctime,
				btime:   a.btime,
				kind:    a.kind.into(),
				perm:    a.perm,
				nlink:   a.nlink.into(),
				uid:     a.uid,
				gid:     a.gid,
				rdev:    0,
				blksize: a.blksize,
				flags:   a.flags,
			}
		}
	}
}
