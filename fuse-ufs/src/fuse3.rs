use std::{
	ffi::{c_int, OsStr},
	io::{Error as IoError, ErrorKind, Result as IoResult},
	time::{Duration, SystemTime},
};

use cfg_if::cfg_if;
use fuser::{
	FileAttr,
	Filesystem,
	KernelConfig,
	ReplyBmap,
	ReplyEmpty,
	ReplyLseek,
	Request,
	TimeOrNow,
};
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

	/// Drain before the filesystem goes away.
	///
	/// This is the hook the kernel sends on unmount, and it is the right place to
	/// drain: `Drop` is a backstop for the paths that never reach here, but the
	/// kernel has already told us the filesystem is about to become unreachable,
	/// and "clean unmount" means the last write happened before the unmount
	/// rather than after it.
	///
	/// There is no error to return, so a drain that cannot finish is logged
	/// rather than reported.  What must not happen is passing over it quietly.
	fn destroy(&mut self) {
		if !self.rw {
			return;
		}
		match self.ufs.shutdown() {
			Ok(st) if st.is_drained() => log::debug!("unmount: metadata drained"),
			Ok(st) => {
				log::error!(
					"unmount: metadata is still outstanding and has NOT been written: {st:?}"
				)
			}
			Err(e) => log::error!("unmount: drain failed, metadata may be incomplete: {e}"),
		}
	}

	/// `READDIRPLUS`: the same listing as [`Self::readdir`], with attributes.
	///
	/// Identical enumeration, so the two *cannot* disagree about which objects
	/// are in the directory or what they are called.  The only difference is what
	/// travels with each entry.
	///
	/// `fuser`'s default returns `ENOSYS`, which is what the driver did before
	/// this, and what the kernel falls back to when it gets it -- so enabling
	/// this changes no behaviour for a kernel that does not ask, and saves a
	/// `GETATTR` per entry for one that does.
	fn readdirplus(
		&mut self,
		_req: &Request<'_>,
		inr: u64,
		_fh: u64,
		offset: i64,
		mut reply: fuser::ReplyDirectoryPlus,
	) {
		let f = || {
			let inr = transino(inr)?;
			if offset < 0 {
				return Err(err!(EINVAL));
			}

			for e in self.ufs.dir_entries(inr)? {
				if e.offset <= offset {
					continue;
				}
				// The attributes are read here rather than collected by
				// `dir_iter` because reading an inode needs the filesystem
				// mutably, and `dir_iter` is holding it.  Same object, same
				// answer `getattr` would give -- it is the same call.
				//
				// A failure is logged and the entry skipped rather than failing
				// the listing: `READDIRPLUS` is asking for extra detail, and one
				// unreadable inode should not cost the caller the whole
				// directory.  `READDIR` would have listed the entry regardless.
				// The offset is already assigned and does not move, so skipping
				// cannot desynchronise a later continuation.
				let attr: FileAttr = match self.ufs.inode_attr(e.inr) {
					Ok(attr) => attr.into(),
					Err(err) => {
						log::error!("readdirplus: {} {}: {err}", e.inr, e.name.to_string_lossy());
						continue;
					}
				};
				if reply.add(e.inr.get64(), e.offset, &e.name, &MAX_CACHE, &attr, 0) {
					break;
				}
			}

			Ok(())
		};
		match run(f) {
			Ok(()) => reply.ok(),
			Err(e) => reply.error(e),
		}
	}

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
			if offset < 0 {
				return Err(err!(EINVAL));
			}

			// Continuation used to be refused outright: a non-zero offset
			// returned an empty reply, so a directory with more entries than fit
			// in one buffer silently ended there.  The offset is now a property of
			// the entry, so a second call resumes where the first stopped.
			for e in self.ufs.dir_entries(inr)? {
				if e.offset <= offset {
					continue;
				}
				if reply.add(e.inr.get64(), e.offset, e.kind.into(), &e.name) {
					break;
				}
			}

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
				self.ufs.truncate(inr, size)?;
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

	/// Map a block of a file to a block of the underlying image.
	///
	/// # The units, which are the whole difficulty
	///
	/// The kernel sends a file-relative block index in units of its `blocksize`
	/// field (which for a FUSE mount is whatever the mount option said -- *not*
	/// the superblock's block size), and expects the reply as a device block
	/// index in those same `blocksize` units.  A UFS block number is a *fragment*
	/// address, so the conversion is:
	///
	/// ```text
	/// file offset  = idx * blocksize
	/// device offset = ufs_block * fs_fsize + (offset % fs_bsize)
	/// reply         = device offset / blocksize
	/// ```
	///
	/// The `(offset % fs_bsize)` term is what makes a request in the middle of a
	/// 32 KiB block land on the right sector: without it, every sector in the
	/// block receives the block's first sector.  Hard-coding 512 for the sector
	/// size is correct only when `blocksize` is 512 and silently mis-maps every
	/// other case.
	///
	/// # Zero means "not mapped"
	///
	/// The protocol has no way to say "this is a hole", so 0 is the answer for
	/// one.  That is unambiguous here because UFS never allocates block 0.
	///
	/// # When this is called at all
	///
	/// Only for a `blkdev` mount, because the kernel only asks a filesystem to
	/// do bmap when there is a block device behind it.
	fn bmap(&mut self, _req: &Request<'_>, ino: u64, blocksize: u32, idx: u64, reply: ReplyBmap) {
		log::trace!("bmap(ino={ino:#x}, blocksize={blocksize}, idx={idx})");
		let f = || {
			let inr = transino(ino)?;
			let bs = blocksize as u64;
			if bs == 0 {
				return Err(IoError::from_raw_os_error(libc::EINVAL));
			}
			// `idx` is a block index in `blocksize` units, so the byte offset is
			// `idx * blocksize`, *not* `idx * 512`.  Using 512 is correct only
			// when blocksize is 512 and silently mis-maps every other case.
			let offset = idx
				.checked_mul(bs)
				.ok_or_else(|| IoError::from_raw_os_error(libc::EOVERFLOW))?;
			Ok(match self.ufs.inode_map_offset(inr, offset)? {
				rufs::BlockMapping::Data { block, .. } => {
					// The filesystem block is a fragment address: its byte start is
					// `block * fs_fsize`.  The requested offset lands
					// `offset % fs_bsize` bytes into that block, and that term is
					// what makes a sector inside a 32 KiB block return the right
					// sector instead of always the first one.
					let within = offset % self.ufs.bsize();
					let device = block
						.checked_mul(self.ufs.fsize())
						.and_then(|b| b.checked_add(within))
						.ok_or_else(|| IoError::from_raw_os_error(libc::EOVERFLOW))?;
					if device % bs != 0 {
						// The reply is in `blocksize` units, so the byte offset must
						// divide evenly.  A non-aligned fragment cannot be named in
						// the bmap protocol; for UFS2 `fs_fsize` is a multiple of the
						// usual 512-byte sector, so this rejects only genuinely
						// misaligned requests rather than silently rounding.
						return Err(IoError::from_raw_os_error(libc::EINVAL));
					}
					device / bs
				}
				// A hole, and end of file alike: both are "not mapped".
				_ => 0,
			})
		};
		match run(f) {
			Ok(block) => reply.bmap(block),
			Err(e) => reply.error(e),
		}
	}

	/// `lseek(SEEK_DATA)` and `lseek(SEEK_HOLE)`.
	///
	/// The kernel sends `whence` as `SEEK_DATA` (3) or `SEEK_HOLE` (4), which
	/// arrive unchanged as `FUSE_SEEK_DATA`/`FUSE_SEEK_HOLE`.  Anything else is
	/// `EINVAL`: ordinary `SEEK_SET`/`SEEK_CUR`/`SEEK_END` are VCS operations
	/// and never reach a filesystem.
	///
	/// # About ENXIO
	///
	/// `EINVAL` is what the ABI asks for here and `ENXIO` is what
	/// `rufs::Ufs::inode_seek_data` returns for "no data at or after this
	/// offset".  Note that the kernel's FUSE lseek path has historically turned
	/// any reply other than `ENOSYS` into `EIO` before handing it to the caller,
	/// so a user of a mount on such a kernel may observe `EIO` where the
	/// protocol says `ENXIO`.  Replying with the documented code is still the
	/// right thing: it is what the protocol specifies and what a kernel that
	/// preserves it needs.
	fn lseek(
		&mut self,
		_req: &Request<'_>,
		ino: u64,
		_fh: u64,
		offset: i64,
		whence: i32,
		reply: ReplyLseek,
	) {
		let f = || {
			let inr = transino(ino)?;
			if offset < 0 {
				return Err(IoError::from_raw_os_error(libc::EINVAL));
			}
			let offset = offset as u64;
			match whence as libc::c_int {
				libc::SEEK_DATA => self.ufs.inode_seek_data(inr, offset),
				libc::SEEK_HOLE => self.ufs.inode_seek_hole(inr, offset),
				_ => Err(IoError::from_raw_os_error(libc::EINVAL)),
			}
		};
		match run(f) {
			Ok(at) => reply.offset(at as i64),
			Err(e) => reply.error(e),
		}
	}
}
