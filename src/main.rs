use std::{
	mem::size_of,
	path::{Path, PathBuf},
	process::Command,
	thread::sleep,
	time::Duration,
};

use anyhow::Result;

use crate::{data::*, ufs::Ufs};

mod data;
mod decoder;
mod inode;
mod ufs;

fn shell(cmd: &str) {
	Command::new("sh")
		.args(&["-c", cmd])
		.spawn()
		.unwrap()
		.wait()
		.unwrap();
}

fn main() -> Result<()> {
	env_logger::init();

	assert_eq!(size_of::<Superblock>(), 1376);
	assert_eq!(size_of::<Inode>(), 256);
	let fs = Ufs::open(PathBuf::from("/dev/da0"))?;
	let mp = Path::new("mp");
	let options = &[];

	let mount = fuser::spawn_mount2(fs, mp, options)?;
	sleep(Duration::new(1, 0));
	shell("ls -ld mp");
	shell("ls -la mp");
	sleep(Duration::new(1, 0));
	drop(mount);

	Ok(())
}
