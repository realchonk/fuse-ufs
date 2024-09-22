use std::{ffi::CString, io::{Error, Result}, os::unix::ffi::OsStrExt, path::Path};
use fuse2rs::*;
use rufs::InodeNum;

use crate::Fs;

impl Fs {
	fn lookup(&mut self, path: &Path) -> Result<InodeNum> {
		if !path.is_absolute() {
			return Err(Error::from_raw_os_error(libc::EINVAL));
		}

		let mut inr = InodeNum::ROOT;
		for comp in path.components().skip(1) {
			inr = self.ufs.dir_lookup(inr, comp.as_os_str())?;
		}
		Ok(inr)
	}
}

impl Filesystem for Fs {
	fn getattr(
		&mut self,
		_req: &Request,
		path: &Path,
	) -> Result<FileAttr> {
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
}

