#![cfg_attr(fuzzing, allow(dead_code, unused_imports, unused_mut))]

mod blockreader;
mod data;
mod decoder;
mod inode;
mod ufs;

pub use crate::{
	blockreader::BlockReader,
	data::{InodeNum, InodeAttr},
	ufs::{Info, Ufs},
};
