use std::{io::{Error, Result}, path::Path};
use fuse2rs::*;

use crate::Fs;

impl Filesystem for Fs {
	fn getattr(
		&mut self,
		_req: &Request,
		path: &Path,
	) -> Result<FileAttr> {
		Err(Error::from_raw_os_error(libc::ENOSYS))
	}

	fn readdir(
		&mut self,
		_req: &Request,
		path: &Path,
		off: u64,
		filler: &mut DirFiller,
		_info: &FileInfo,
	) -> Result<()> {
		Err(Error::from_raw_os_error(libc::ENOSYS))
	}

	fn read(
		&mut self,
		_req: &Request,
		path: &Path,
		off: u64,
		buf: &mut [u8],
		_info: &FileInfo,
	) -> Result<usize> {
		Err(Error::from_raw_os_error(libc::ENOSYS))
	}
}

