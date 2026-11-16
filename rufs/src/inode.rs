use std::{
	io::Error,
	time::{Duration, SystemTime},
};

use bincode_next::{
	de::Decoder,
	enc::Encoder,
	error::{DecodeError, EncodeError},
	Decode,
	Encode,
};

use crate::data::*;

/// `ns` is never negative on disk. Times `SystemTime` cannot hold become the epoch.
fn timetosys(s: UfsTime, ns: u32) -> SystemTime {
	let secs = Duration::from_secs(s.unsigned_abs());
	let base = if s < 0 {
		SystemTime::UNIX_EPOCH.checked_sub(secs)
	} else {
		SystemTime::UNIX_EPOCH.checked_add(secs)
	};
	base.and_then(|t| t.checked_add(Duration::from_nanos(ns.into())))
		.unwrap_or(SystemTime::UNIX_EPOCH)
}

fn systotime(t: SystemTime) -> (UfsTime, u32) {
	match t.duration_since(SystemTime::UNIX_EPOCH) {
		Ok(d) => (d.as_secs() as UfsTime, d.subsec_nanos()),
		Err(e) => {
			// Round the seconds down, so that the nanoseconds stay positive.
			let d = e.duration();
			let s = -(d.as_secs() as UfsTime);
			match d.subsec_nanos() {
				0 => (s, 0),
				ns => (s - 1, 1_000_000_000 - ns),
			}
		}
	}
}

impl Inode {
	pub fn new(kind: InodeType, perm: u16, uid: u32, gid: u32, blksize: u32) -> Self {
		let (now, nowsnsec) = systotime(SystemTime::now());
		let data = match kind {
			InodeType::Symlink => InodeData::Shortlink([0u8; UFS_SLLEN]),
			_ => InodeData::Blocks(InodeBlocks::default()),
		};
		let kind = match kind {
			InodeType::RegularFile => S_IFREG,
			InodeType::Directory => S_IFDIR,
			InodeType::Symlink => S_IFLNK,
			InodeType::CharDevice => S_IFCHR,
			InodeType::BlockDevice => S_IFBLK,
			InodeType::Socket => S_IFSOCK,
			InodeType::NamedPipe => S_IFIFO,
		};
		let mode = kind | (perm & !S_IFMT);
		Self {
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
			atimensec: nowsnsec,
			mtimensec: nowsnsec,
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
		}
	}

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

	pub fn set_atime(&mut self, t: SystemTime) {
		(self.atime, self.atimensec) = systotime(t);
	}

	pub fn set_mtime(&mut self, t: SystemTime) {
		(self.mtime, self.mtimensec) = systotime(t);
	}

	pub fn set_ctime(&mut self, t: SystemTime) {
		(self.ctime, self.ctimensec) = systotime(t);
	}

	pub fn set_btime(&mut self, t: SystemTime) {
		(self.birthtime, self.birthnsec) = systotime(t);
	}

	pub fn assert_dir(&self) -> Result<(), Error> {
		if self.kind() == InodeType::Directory {
			Ok(())
		} else {
			Err(Error::from_raw_os_error(libc::ENOTDIR))
		}
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

	/// Device nodes, fifos and sockets have no data, whatever di_size says.
	pub fn has_data(&self) -> bool {
		matches!(
			self.kind(),
			InodeType::RegularFile | InodeType::Symlink | InodeType::Directory
		)
	}

	pub fn size(&self, bs: u64, fs: u64) -> (u64, u64) {
		let size = if self.has_data() { self.size } else { 0 };
		Self::inode_size(bs, fs, size)
	}

	/// The number of blocks and fragments this inode needs.
	pub fn inode_size(bs: u64, fs: u64, size: u64) -> (u64, u64) {
		let blocks = size / bs;
		let frags = (size % bs).div_ceil(fs);

		(blocks, frags)
	}
}

impl<Context> Decode<Context> for Inode {
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
		let data = if (mode & S_IFMT) == S_IFLNK && size < UFS_SLLEN as u64 {
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
	fn timetosys_extremes() {
		use std::time::{Duration, SystemTime};

		let t = super::timetosys;
		let epoch = SystemTime::UNIX_EPOCH;
		assert_eq!(t(-1, 0), epoch - Duration::from_secs(1));
		assert_eq!(t(-1, 500_000_000), epoch - Duration::from_millis(500));
		// Must not panic.
		t(i64::MIN, u32::MAX);
		t(i64::MAX, u32::MAX);
	}

	#[test]
	fn systotime_before_epoch() {
		use std::time::{Duration, SystemTime};

		let epoch = SystemTime::UNIX_EPOCH;
		let half = epoch - Duration::from_millis(500);
		assert_eq!(super::systotime(half), (-1, 500_000_000));
		for t in [
			epoch - Duration::new(5, 100),
			epoch,
			epoch + Duration::new(9, 900),
		] {
			let (s, ns) = super::systotime(t);
			assert_eq!(super::timetosys(s, ns), t);
		}
	}

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
