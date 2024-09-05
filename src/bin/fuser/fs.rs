use std::{
	ffi::{c_int, OsStr}, io::{Error as IoError, ErrorKind, Result as IoResult}, path::Path, time::Duration};
use fuse_ufs::Ufs;
use fuser::{Filesystem, KernelConfig, Request};

const MAX_CACHE: Duration = Duration::MAX;

fn run<T>(f: impl FnOnce() -> IoResult<T>) -> Result<T, c_int> {
	f().map_err(|e| {
		log::error!("Error: {e}");
		e.raw_os_error().unwrap_or(libc::EIO)
	})
}

fn transino(ino: u64) -> u64 {
	if ino == fuser::FUSE_ROOT_ID {
		2
	} else {
		ino
	}
}

pub struct Fs {
	ufs: Ufs,
}

impl Fs {
	pub fn open(path: &Path) -> anyhow::Result<Self> {
		Ok(Self {
			ufs: Ufs::open(path)?,
		})
	}
}

impl Filesystem for Fs {
	fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
		Ok(())
	}

	fn destroy(&mut self) {}

	fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: fuser::ReplyAttr) {
		let ino = transino(ino);
		match run(|| self.ufs.read_inode(ino)) {
			Ok(x) => reply.attr(&MAX_CACHE, &x.as_fileattr(ino)),
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
		let inr = transino(inr);
		let f = || {
			if offset != 0 {
				return Ok(());
			}

			let mut i = 0;

			self.ufs.dir_iter(inr, |name, inr, kind| {
				i += 1;
				if i > offset && reply.add(inr.into(), i, kind, name) {
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
		let pinr = transino(pinr);

		let mut f = || {
			let inr = self.ufs.dir_lookup(pinr, name)?;
			let ino = self.ufs.read_inode(inr)?;
			Ok::<_, IoError>((ino.as_fileattr(inr), ino.gen))
		};

		match f() {
			Ok((attr, gen)) => reply.entry(&Duration::ZERO, &attr, gen.into()),
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
		let inr = transino(inr);

		let f = || {
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
		let inr = transino(inr);
		match run(|| self.ufs.symlink_read(inr)) {
			Ok(x) => reply.data(&x),
			Err(e) => reply.error(e),
		}
	}

	fn listxattr(&mut self, _req: &Request<'_>, inr: u64, size: u32, reply: fuser::ReplyXattr) {
		let inr = transino(inr);

		enum R {
			Len(u32),
			Data(Vec<u8>),
		}

		let f = || {
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
		let inr = transino(inr);

		enum R {
			Data(Vec<u8>),
			TooShort,
			Len(u32),
		}

		let f = || {
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
}
