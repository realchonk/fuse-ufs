use std::{ffi::OsStr, io::{Error, ErrorKind, Result}, path::{Component, Path}};
use fuse2rs::{Request as XRequest, FileAttr as XFileAttr, FileInfo as XFileInfo, FileType as XFileType};
pub use fuse2rs::DirFiller;
use crate::{FileInfo, FileType, Filesystem, Inode, Request};


pub struct Wrapper<F: Filesystem + 'static> {
	fs: F,
}

impl FileInfo {
	fn new(info: &XFileInfo) -> Self {
		Self {
			flags: info.flags,
			fh: info.fh,
		}
	}
}

impl Request {
	fn new(req: &XRequest) -> Self {
		Self {
			uid: req.uid,
			gid: req.gid,
		}
	}
}

impl<F: Filesystem + 'static> Wrapper<F> {
	pub fn new(fs: F) -> Self {
		Self {
			fs,
		}
	}

	pub fn mount(self, mp: &Path) -> Result<()> {
		fuse2rs::mount(mp, self, vec![])
	}

	fn lookup(&mut self, req: &Request, path: &Path) -> Result<Inode> {
		let mut comps = path.components();
		let Some(Component::RootDir) = comps.next() else {
			return Err(Error::new(ErrorKind::InvalidData, "must be an absolute path"));
		};

		let mut ino = self.fs.root();

		for c in comps {
			let name = match c {
				Component::RootDir => unreachable!(),
				Component::Prefix(_) => unreachable!(),
				Component::CurDir => OsStr::new("."),
				Component::ParentDir => OsStr::new(".."),
				Component::Normal(name) => name,
			};
			ino = self.fs.lookup(req, ino, name)?;
		}

		Ok(ino)
	}
}

impl<F: Filesystem> fuse2rs::Filesystem for Wrapper<F> {
	fn init(&mut self, req: &XRequest) {
		self.fs.init(&Request::new(req));
	}
	
	fn getattr(
		&mut self,
		req: &XRequest,
		path: &Path,
	) -> Result<XFileAttr> {
		let req = Request::new(req);
		let ino = self.lookup(&req, path)?;
		let attr = self.fs.getattr(&req, ino)?;

		let kind = match attr.kind {
			FileType::RegularFile => XFileType::RegularFile,
			FileType::Directory => XFileType::Directory,
			FileType::NamedPipe => XFileType::NamedPipe,
			FileType::Socket => XFileType::Socket,
			FileType::CharDevice => XFileType::CharDevice,
			FileType::BlockDevice => XFileType::BlockDevice,
			FileType::Symlink => XFileType::Symlink,
		};
		let attr = XFileAttr {
			ino,
			size: attr.size,
			blocks: attr.blocks,
			atime: attr.atime,
			mtime: attr.mtime,
			ctime: attr.ctime,
			btime: attr.btime,
			kind,
			perm: attr.perm,
			uid: attr.uid,
			gid: attr.gid,
			rdev: attr.rdev,
			blksize: attr.blksize,
			flags: attr.flags,
			nlink: attr.nlink,
		};
		Ok(attr)
	}

	fn readdir(
		&mut self,
		req: &XRequest,
		path: &Path,
		off: u64,
		filler: &mut DirFiller,
		info: &XFileInfo,
	) -> Result<()> {
		let req = Request::new(req);
		let info = FileInfo::new(info);
		let ino = self.lookup(&req, path)?;
		self.fs.readdir(&req, ino, off, filler, &info)
	}

	fn read(
		&mut self,
		req: &XRequest,
		path: &Path,
		off: u64,
		buf: &mut [u8],
		info: &XFileInfo,
	) -> Result<usize> {
		let req = Request::new(req);
		let info = FileInfo::new(info);
		let ino = self.lookup(&req, path)?;
		self.fs.read(&req, ino, off, buf, &info)
	}
}
