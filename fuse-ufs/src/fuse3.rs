use std::{
	ffi::{c_int, OsStr},
	io::{Error as IoError, ErrorKind, Result as IoResult},
	time::{Duration, SystemTime},
};

use cfg_if::cfg_if;
use fuser::{FileAttr, Filesystem, KernelConfig, ReplyEmpty, Request, TimeOrNow};
use rufs::{InodeAttr, InodeNum, InodeType};

use crate::{consts::*, Fs};

const MAX_CACHE: Duration = Duration::MAX;

fn run<T>(f: impl FnOnce() -> IoResult<T>) -> Result<T, c_int> {
	f().map_err(|e| {
		// suppress xattr errors
		if e.raw_os_error() != Some(rufs::ENOATTR) {
			log::error!("Error: {e}");
		}
		e.raw_os_error().unwrap_or(libc::EIO)
	})
}

fn transino(inr: u64) -> IoResult<InodeNum> {
	if inr == fuser::FUSE_ROOT_ID {
		Ok(InodeNum::ROOT)
	} else {
		let inr = inr.try_into().map_err(|_| err!(EINVAL))?;
		Ok(unsafe { InodeNum::new(inr) })
	}
}

impl Filesystem for Fs {
	fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
		Ok(())
	}

	fn destroy(&mut self) {}

	fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: fuser::ReplyAttr) {
		let f = || {
			let inr = transino(ino)?;
			let st: FileAttr = self.ufs.inode_attr(inr)?.into();
			Ok(st)
		};
		match run(f) {
			Ok(x) => reply.attr(&MAX_CACHE, &x),
			Err(e) => reply.error(e),
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

	// TODO: use offset in a less stupid way
	fn readdir(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		mut reply: fuser::ReplyDirectory,
	) {
		let f = || {
			let inr = transino(inr)?;
			if offset != 0 {
				return Ok(());
			}

			let mut i = 0;

			self.ufs.dir_iter(inr, |name, inr, kind| {
				i += 1;
				if i > offset && reply.add(inr.get64(), i, kind.into(), name) {
					return Some(());
				}
				None
			})?;

			Ok(())
		};
		match run(f) {
			Ok(_) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}

	fn lookup(&mut self, _req: &Request<'_>, pinr: u64, name: &OsStr, reply: fuser::ReplyEntry) {
		let mut f = || {
			let pinr = transino(pinr)?;
			let inr = self.ufs.dir_lookup(pinr, name)?;
			let st = self.ufs.inode_attr(inr)?;
			Ok::<_, IoError>((st.gen, st.into()))
		};

		match f() {
			Ok((gen, st)) => reply.entry(&Duration::ZERO, &st, gen.into()),
			Err(e) => {
				if e.kind() != ErrorKind::NotFound {
					log::error!("Error: {e}");
				}
				reply.error(e.raw_os_error().unwrap_or(libc::EIO))
			}
		}
	}

	fn read(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		size: u32,
		_flags: i32,
		_lock_owner: Option<u64>,
		reply: fuser::ReplyData,
	) {
		let f = || {
			let inr = transino(inr)?;
			let mut buffer = vec![0u8; size as usize];
			let n = self.ufs.inode_read(inr, offset as u64, &mut buffer)?;
			buffer.shrink_to(n);
			Ok(buffer)
		};

		match run(f) {
			Ok(buf) => reply.data(&buf),
			Err(e) => reply.error(e),
		}
	}

	fn write(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		data: &[u8],
		_write_flags: u32,
		_flags: i32,
		_lock_owner: Option<u64>,
		reply: fuser::ReplyWrite,
	) {
		let f = || {
			let inr = transino(inr)?;
			self.ufs.inode_write(inr, offset as u64, data)
		};

		match run(f) {
			Ok(n) => reply.written(n as u32),
			Err(e) => reply.error(e),
		}
	}

	fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
		let info = self.ufs.info();
		reply.statfs(
			info.blocks,
			info.bfree,
			info.bfree,
			info.files,
			info.ffree,
			info.bsize,
			255,
			info.fsize,
		)
	}

	fn readlink(&mut self, _req: &Request<'_>, inr: u64, reply: fuser::ReplyData) {
		let f = || {
			let inr = transino(inr)?;
			self.ufs.symlink_read(inr)
		};
		match run(f) {
			Ok(x) => reply.data(&x),
			Err(e) => reply.error(e),
		}
	}

	fn listxattr(&mut self, _req: &Request<'_>, inr: u64, size: u32, reply: fuser::ReplyXattr) {
		enum R {
			Len(u32),
			Data(Vec<u8>),
		}

		let f = || {
			let inr = transino(inr)?;
			if size == 0 {
				let len = self.ufs.xattr_list_len(inr)?;
				Ok(R::Len(len))
			} else {
				let data = self.ufs.xattr_list(inr)?;
				Ok(R::Data(data))
			}
		};

		match run(f) {
			Ok(R::Data(data)) => reply.data(&data),
			Ok(R::Len(len)) => reply.size(len),
			Err(e) => reply.error(e),
		}
	}

	fn getxattr(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		name: &OsStr,
		size: u32,
		reply: fuser::ReplyXattr,
	) {
		enum R {
			Data(Vec<u8>),
			TooShort,
			Len(u32),
		}

		let f = || {
			let inr = transino(inr)?;
			if size == 0 {
				let len = self.ufs.xattr_len(inr, name)?;
				Ok(R::Len(len))
			} else {
				let data = self.ufs.xattr_read(inr, name)?;
				if (size as usize) >= data.len() {
					Ok(R::Data(data))
				} else {
					Ok(R::TooShort)
				}
			}
		};

		match run(f) {
			Ok(R::Data(x)) => reply.data(&x),
			Ok(R::TooShort) => reply.error(libc::ERANGE),
			Ok(R::Len(l)) => reply.size(l),
			Err(e) => reply.error(e),
		}
	}

	fn unlink(&mut self, _req: &Request<'_>, pinr: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
		let f = || {
			let pinr = transino(pinr)?;
			self.ufs.unlink(pinr, name)?;
			Ok(())
		};

		match run(f) {
			Ok(()) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}

	fn rmdir(&mut self, _req: &Request<'_>, pinr: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
		let f = || {
			let pinr = transino(pinr)?;
			self.ufs.rmdir(pinr, name)?;
			Ok(())
		};

		match run(f) {
			Ok(()) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}

	fn setattr(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		mode: Option<u32>,
		uid: Option<u32>,
		gid: Option<u32>,
		size: Option<u64>,
		atime: Option<TimeOrNow>,
		mtime: Option<TimeOrNow>,
		ctime: Option<SystemTime>,
		_fh: Option<u64>,
		btime: Option<SystemTime>,
		_chgtime: Option<SystemTime>,
		_bkuptime: Option<SystemTime>,
		flags: Option<u32>,
		reply: fuser::ReplyAttr,
	) {
		fn cvtime(t: TimeOrNow) -> SystemTime {
			match t {
				TimeOrNow::SpecificTime(t) => t,
				TimeOrNow::Now => SystemTime::now(),
			}
		}

		let f = || {
			let inr = transino(inr)?;

			if let Some(size) = size {
				self.ufs.inode_truncate(inr, size)?;
			}

			let f = |mut attr: InodeAttr| {
				if let Some(mode) = mode {
					attr.perm = (mode & 0xffff) as u16;
				}

				if let Some(uid) = uid {
					attr.uid = uid;
				}

				if let Some(gid) = gid {
					attr.gid = gid;
				}

				if let Some(atime) = atime {
					attr.atime = cvtime(atime);
				}

				if let Some(mtime) = mtime {
					attr.mtime = cvtime(mtime);
				}

				if let Some(ctime) = ctime {
					attr.ctime = ctime;
				}

				if let Some(btime) = btime {
					attr.btime = btime;
				}

				if let Some(flags) = flags {
					attr.flags = flags;
				}

				attr
			};

			self.ufs.inode_modify(inr, f)?;

			let st = self.ufs.inode_attr(inr)?;
			Ok(st)
		};

		match run(f) {
			Ok(st) => reply.attr(&MAX_CACHE, &st.into()),
			Err(e) => reply.error(e),
		}
	}

	fn mknod(
		&mut self,
		req: &Request<'_>,
		parent: u64,
		name: &OsStr,
		mode: u32,
		umask: u32,
		_rdev: u32,
		reply: fuser::ReplyEntry,
	) {
		let f = || {
			let dinr = transino(parent)?;
			let kind = match mode & S_IFMT {
				S_IFREG => InodeType::RegularFile,
				S_IFDIR => InodeType::Directory,
				S_IFLNK => InodeType::Symlink,
				S_IFCHR => InodeType::CharDevice,
				S_IFBLK => InodeType::BlockDevice,
				S_IFSOCK => InodeType::Socket,
				S_IFIFO => InodeType::NamedPipe,
				_ => return Err(IoError::from_raw_os_error(libc::EINVAL)),
			};
			let perm = (mode & !S_IFMT) as u16;

			let attr = self.ufs.mknod(
				dinr,
				name,
				kind,
				perm & !(umask as u16),
				req.uid(),
				req.gid(),
			)?;
			Ok((attr.gen, attr))
		};

		match run(f) {
			Ok((g, a)) => reply.entry(&MAX_CACHE, &a.into(), g.into()),
			Err(e) => reply.error(e),
		}
	}

	fn create(
		&mut self,
		req: &Request<'_>,
		parent: u64,
		name: &OsStr,
		mode: u32,
		umask: u32,
		_flags: i32,
		reply: fuser::ReplyCreate,
	) {
		let f = || {
			let dinr = transino(parent)?;
			let kind = match mode & S_IFMT {
				S_IFREG => InodeType::RegularFile,
				S_IFDIR => InodeType::Directory,
				S_IFLNK => InodeType::Symlink,
				S_IFCHR => InodeType::CharDevice,
				S_IFBLK => InodeType::BlockDevice,
				S_IFSOCK => InodeType::Socket,
				S_IFIFO => InodeType::NamedPipe,
				_ => return Err(IoError::from_raw_os_error(libc::EINVAL)),
			};
			let perm = (mode & !S_IFMT) as u16;

			let attr = self.ufs.mknod(
				dinr,
				name,
				kind,
				perm & !(umask as u16),
				req.uid(),
				req.gid(),
			)?;
			Ok((attr.gen, attr))
		};

		match run(f) {
			Ok((g, a)) => reply.created(&MAX_CACHE, &a.into(), g.into(), 0, 0),
			Err(e) => reply.error(e),
		}
	}

	fn symlink(
		&mut self,
		req: &Request<'_>,
		parent: u64,
		link_name: &OsStr,
		target: &std::path::Path,
		reply: fuser::ReplyEntry,
	) {
		let f = || {
			let dinr = transino(parent)?;
			let attr =
				self.ufs
					.symlink(dinr, link_name, target.as_os_str(), req.uid(), req.gid())?;
			Ok((attr.gen, attr))
		};

		match run(f) {
			Ok((g, a)) => reply.entry(&MAX_CACHE, &a.into(), g.into()),
			Err(e) => reply.error(e),
		}
	}

	fn mkdir(
		&mut self,
		req: &Request<'_>,
		parent: u64,
		name: &OsStr,
		mode: u32,
		umask: u32,
		reply: fuser::ReplyEntry,
	) {
		let f = || {
			let dinr = transino(parent)?;
			let perm = mode & !S_IFMT & !umask;
			let attr = self
				.ufs
				.mkdir(dinr, name, perm as u16, req.uid(), req.gid())?;
			Ok((attr.gen, attr))
		};

		match run(f) {
			Ok((g, a)) => reply.entry(&MAX_CACHE, &a.into(), g.into()),
			Err(e) => reply.error(e),
		}
	}

	fn rename(
		&mut self,
		_req: &Request<'_>,
		parent: u64,
		name: &OsStr,
		newparent: u64,
		newname: &OsStr,
		#[allow(unused)] // Unused on some OSes but not all
		flags: u32,
		reply: ReplyEmpty,
	) {
		let f = || {
			let s_pinr = transino(parent)?;
			let d_pinr = transino(newparent)?;
			cfg_if! {
				if #[cfg(target_os = "linux")] {
					use libc::RENAME_NOREPLACE;

					let replace = (flags & RENAME_NOREPLACE) !=
						RENAME_NOREPLACE;
				} else {
					let replace = true;
				}
			}
			self.ufs.rename(d_pinr, newname, s_pinr, name, replace)?;
			Ok(())
		};

		match run(f) {
			Ok(()) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}
}
