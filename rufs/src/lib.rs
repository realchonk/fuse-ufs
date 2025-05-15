#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod data;
mod decoder;
mod inode;
mod ufs;

/// Number of inodes to store in a cache.
const ICACHE_SIZE: usize = 1024;

/// Number of blocks cached.
const BCACHE_SIZE: usize = 16;

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
pub const ENOATTR: i32 = libc::ENOATTR;
#[cfg(target_os = "linux")]
pub const ENOATTR: i32 = libc::ENODATA;

pub use crate::{
	blockreader::{Backend, BlockReader},
	data::{InodeAttr, InodeNum, InodeType},
	ufs::{Info, Ufs},
};
