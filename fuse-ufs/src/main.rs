use std::fs::File;

use anyhow::Result;
use cfg_if::cfg_if;
use clap::Parser;
use rufs::Ufs;

use crate::cli::Cli;

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

	let fs = Fs {
		ufs: Ufs::open(&cli.device)?,
	};

	let mp = &cli.mountpoint;
	cfg_if! {
		if #[cfg(all(feature = "fuse3", feature = "fuse2"))] {
			compile_error!("more than one FUSE backend selected")
		} else if #[cfg(feature = "fuse3")] {
			let opts = cli.options();
			if cli.foreground {
				fuser::mount2(fs, mp, &opts)?;
			} else {
				fuser::spawn_mount2(fs, mp, &opts)?;
			}
		} else if #[cfg(feature = "fuse2")] {
			fuse2rs::mount(mp, fs, cli.options()?)?;
		} else {
			compile_error!("no FUSE backend selected");
		}
	}

	Ok(())
}
