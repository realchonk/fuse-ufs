mod blockreader;
mod data;
mod decoder;
mod inode;
mod ufs;

pub use crate::{
	data::{Inode, InodeNum},
	ufs::{Info, Ufs},
};

