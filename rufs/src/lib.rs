#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod data;
mod decoder;
mod inode;
mod ufs;

/// Number of inodes to store in a cache.
#[cfg(feature = "icache")]
const ICACHE_SIZE: usize = 1024;

/// Number of blocks cached.
#[cfg(feature = "bcache")]
const BCACHE_SIZE: usize = 16;

/// Number of directory entries to cache.
#[cfg(feature = "dcache")]
const DCACHE_SIZE: usize = 1024;

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
pub const ENOATTR: i32 = libc::ENOATTR;
#[cfg(target_os = "linux")]
pub const ENOATTR: i32 = libc::ENODATA;

#[cfg(feature = "lru")]
fn new_lru<K: std::hash::Hash + Eq, V>(size: usize) -> lru::LruCache<K, V> {
	lru::LruCache::new(std::num::NonZeroUsize::new(size).unwrap())
}

pub use crate::{
	blockreader::{Backend, BlockReader},
	data::{InodeAttr, InodeNum, InodeType},
	ufs::{Info, Ufs},
};
