use std::fs::File;

use anyhow::Result;
use cfg_if::cfg_if;
use clap::Parser;
use rufs::Ufs;

use crate::cli::Cli;

#[allow(clippy::unnecessary_cast)]
mod consts {
	pub const S_IFMT: u32 = libc::S_IFMT as u32;
	pub const S_IFREG: u32 = libc::S_IFREG as u32;
	pub const S_IFDIR: u32 = libc::S_IFDIR as u32;
	pub const S_IFCHR: u32 = libc::S_IFCHR as u32;
	pub const S_IFBLK: u32 = libc::S_IFBLK as u32;
	pub const S_IFLNK: u32 = libc::S_IFLNK as u32;
	pub const S_IFIFO: u32 = libc::S_IFIFO as u32;
	pub const S_IFSOCK: u32 = libc::S_IFSOCK as u32;
}

macro_rules! err {
	($n:ident) => {
		std::io::Error::from_raw_os_error(libc::$n)
	};
}

mod cli;

#[cfg(feature = "fuse3")]
mod fuse3;

#[cfg(feature = "fuse2")]
mod fuse2;

struct Fs {
	ufs: Ufs<File>,
}

fn main() -> Result<()> {
	let cli = Cli::parse();

	env_logger::builder()
		.filter_level(cli.verbose.log_level_filter())
		.init();

	let (opts, rw) = cli.options()?;

	let fs = Fs {
		ufs: Ufs::open(&cli.device, rw)?,
	};

	let mp = &cli.mountpoint;
	cfg_if! {
		if #[cfg(all(feature = "fuse3", feature = "fuse2"))] {
			compile_error!("more than one FUSE backend selected")
		} else if #[cfg(feature = "fuse3")] {
			if cli.foreground {
				fuser::mount2(fs, mp, &opts)?;
			} else {
				daemonize::Daemonize::new()
					.working_directory(std::env::current_dir()?)
					.start()?;
				fuser::mount2(fs, mp, &opts)?;
			}
		} else if #[cfg(feature = "fuse2")] {
			fuse2rs::mount(mp, fs, opts)?;
		} else {
			compile_error!("no FUSE backend selected");
		}
	}

	Ok(())
}
