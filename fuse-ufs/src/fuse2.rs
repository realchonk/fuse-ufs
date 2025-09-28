use std::{
	ffi::{CString, OsStr},
	io::Result,
	os::unix::ffi::OsStrExt,
	path::Path,
	time::SystemTime,
};

use fuse2rs::*;
use rufs::{InodeNum, InodeType};

use crate::{consts::*, Fs};

fn path_split(path: &Path) -> Result<(&Path, &OsStr)> {
	path.parent().zip(path.file_name()).ok_or(err!(EINVAL))
}

impl Fs {
	fn lookup(&mut self, path: &Path) -> Result<InodeNum> {
		if !path.is_absolute() {
			return Err(err!(EINVAL));
		}

		let mut inr = InodeNum::ROOT;
		for comp in path.components().skip(1) {
			inr = self.ufs.dir_lookup(inr, comp.as_os_str())?;
		}
		Ok(inr)
	}
}

impl Filesystem for Fs {
	fn getattr(&mut self, _req: &Request, path: &Path) -> Result<FileAttr> {
		let inr = self.lookup(path)?;
		let ino = self.ufs.inode_attr(inr)?;
		Ok(ino.into())
	}

	fn readdir(
		&mut self,
		_req: &Request,
		path: &Path,
		off: u64,
		filler: &mut DirFiller,
		_info: &FileInfo,
	) -> Result<()> {
		let pinr = self.lookup(path)?;

		// TODO
		if off != 0 {
			return Ok(());
		}

		self.ufs.dir_iter(pinr, |name, _inr, _kind| {
			let name = CString::new(name.as_bytes().to_vec()).unwrap();
			if filler.push(&name) {
				None
			} else {
				Some(())
			}
		})?;

		Ok(())
	}

	fn read(
		&mut self,
		_req: &Request,
		path: &Path,
		off: u64,
		buf: &mut [u8],
		_info: &FileInfo,
	) -> Result<usize> {
		let inr = self.lookup(path)?;
		let num = self.ufs.inode_read(inr, off, buf)?;
		Ok(num)
	}

	fn write(
		&mut self,
		_req: &Request,
		path: &Path,
		off: u64,
		buf: &[u8],
		_info: &FileInfo,
	) -> Result<usize> {
		let inr = self.lookup(path)?;
		let num = self.ufs.inode_write(inr, off, buf)?;
		Ok(num)
	}

	fn readlink(&mut self, _req: &Request, path: &Path, buf: &mut [u8]) -> Result<()> {
		let inr = self.lookup(path)?;
		let link = self.ufs.symlink_read(inr)?;

		let mut len = link.len();

		if len >= buf.len() {
			log::trace!(
				"readlink(): truncating link from {} to {}",
				link.len(),
				buf.len()
			);
			len = buf.len() - 1;
		}

		buf[0..len].copy_from_slice(&link[0..len]);
		buf[len] = b'\0';

		Ok(())
	}

	fn statfs(&mut self, _req: &Request, _path: &Path) -> Result<Statfs> {
		let info = self.ufs.info();

		Ok(Statfs {
			bsize:  info.bsize,
			frsize: info.fsize,
			blocks: info.blocks,
			bfree:  info.bfree,
			bavail: info.bfree,
			files:  info.files,
			ffree:  info.ffree,
			favail: info.ffree,
		})
	}

	fn chown(
		&mut self,
		_req: &Request,
		path: &Path,
		uid: Option<u32>,
		gid: Option<u32>,
	) -> Result<()> {
		let inr = self.lookup(path)?;
		self.ufs.inode_modify(inr, |mut attr| {
			if let Some(uid) = uid {
				attr.uid = uid;
			}
			if let Some(gid) = gid {
				attr.gid = gid;
			}
			attr
		})?;

		Ok(())
	}

	fn chmod(&mut self, _req: &Request, path: &Path, mode: u32) -> Result<()> {
		let inr = self.lookup(path)?;
		self.ufs.inode_modify(inr, |mut attr| {
			attr.perm = (mode & 0xffff) as u16;
			attr
		})?;

		Ok(())
	}

	fn utime(
		&mut self,
		_req: &Request,
		path: &Path,
		atime: SystemTime,
		mtime: SystemTime,
	) -> Result<()> {
		let inr = self.lookup(path)?;
		self.ufs.inode_modify(inr, |mut attr| {
			attr.atime = atime;
			attr.mtime = mtime;
			attr
		})?;

		Ok(())
	}

	fn unlink(&mut self, _req: &Request, path: &Path) -> Result<()> {
		let (dir, name) = path_split(path)?;
		let dinr = self.lookup(dir)?;
		self.ufs.unlink(dinr, name)?;
		Ok(())
	}

	fn rmdir(&mut self, _req: &Request, path: &Path) -> Result<()> {
		let (dir, name) = path_split(path)?;
		let dinr = self.lookup(dir)?;
		self.ufs.rmdir(dinr, name)?;
		Ok(())
	}

	fn truncate(&mut self, _req: &Request, path: &Path, size: u64) -> Result<()> {
		let inr = self.lookup(path)?;
		self.ufs.inode_truncate(inr, size)?;
		Ok(())
	}

	fn create(&mut self, req: &Request, path: &Path, mode: u32, _info: &FileInfo) -> Result<()> {
		self.mknod(req, path, mode, 0)
	}

	fn mknod(&mut self, req: &Request, path: &Path, mode: u32, _dev: u32) -> Result<()> {
		let (dir, name) = path_split(path)?;
		let kind = match mode & S_IFMT {
			S_IFREG => InodeType::RegularFile,
			S_IFDIR => InodeType::Directory,
			S_IFLNK => InodeType::Symlink,
			S_IFCHR => InodeType::CharDevice,
			S_IFBLK => InodeType::BlockDevice,
			S_IFSOCK => InodeType::Socket,
			S_IFIFO => InodeType::NamedPipe,
			_ => return Err(err!(EINVAL)),
		};

		let dinr = self.lookup(dir)?;
		let perm = mode & !S_IFMT;
		self.ufs
			.mknod(dinr, name, kind, perm as u16, req.uid, req.gid)?;
		Ok(())
	}

	fn symlink(&mut self, req: &Request, name1: &Path, name2: &Path) -> Result<()> {
		let (dir, name) = path_split(name2)?;
		let link = name1.as_os_str();
		let dinr = self.lookup(dir)?;
		self.ufs.symlink(dinr, name, link, req.uid, req.gid)?;
		Ok(())
	}

	fn mkdir(&mut self, req: &Request, path: &Path, mode: u32) -> Result<()> {
		let (dir, name) = path_split(path)?;
		let dinr = self.lookup(dir)?;
		let perm = mode & !S_IFMT;
		self.ufs.mkdir(dinr, name, perm as u16, req.uid, req.gid)?;
		Ok(())
	}
}
