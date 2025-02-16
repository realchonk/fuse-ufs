#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod data;
mod decoder;
mod inode;
mod ufs;

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
pub const ENOATTR: i32 = libc::ENOATTR;
#[cfg(target_os = "linux")]
pub const ENOATTR: i32 = libc::ENODATA;

pub use crate::{
	blockreader::BlockReader,
	data::{InodeAttr, InodeNum, InodeType},
	ufs::{Info, Ufs},
};
