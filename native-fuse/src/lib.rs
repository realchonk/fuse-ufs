use std::{ffi::OsStr, io::Result, path::Path};
pub use crate::data::*;

mod data;

pub trait Filesystem {
	fn root(&self) -> Inode;
	fn init(&mut self, _req: &Request);
	fn lookup(&mut self, _req: &Request, dino: Inode, name: &OsStr) -> Result<Inode>;
	fn read(&mut self, _req: &Request, ino: Inode, off: u64, buf: &mut [u8], _info: &FileInfo) -> Result<usize>;
	fn getattr(&mut self, _req: &Request, ino: Inode) -> Result<FileAttr>;
	fn readdir(&mut self, _req: &Request, ino: Inode, off: u64, filler: &mut DirFiller, _info: &FileInfo) -> Result<()>;
}

macro_rules! fuse {
	($mod:ident) => {
		mod $mod;
		pub use crate::$mod::{Wrapper, DirFiller};
	};
}

// fuse3 && fuse2
#[cfg(all(feature = "fuse3", feature = "fuse2"))]
compile_error!("Cannot build with both FUSE3 and FUSE2");

// fuse3 || (!fuse2 && (linux || freebsd))
#[cfg(any(feature = "fuse3", all(not(feature = "fuse2"), any(target_os = "linux", target_os = "freebsd"))))]
fuse!(fuse3);

// fuse2 || (!fuse3 && openbsd)
#[cfg(any(feature = "fuse2", all(not(feature = "fuse3"), target_os = "openbsd")))]
fuse!(fuse2);


pub fn mount(mp: &Path, fs: impl Filesystem + 'static) -> Result<()> {
	Wrapper::new(fs).mount(mp)
}
