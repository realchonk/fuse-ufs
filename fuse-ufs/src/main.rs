use std::fs::File;

use anyhow::Result;
use cfg_if::cfg_if;
use clap::Parser;
use rufs::Ufs;

use crate::cli::Cli;

mod cli;

#[cfg(feature = "fuse3")]
mod fuse3;

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

	cfg_if! {
		if #[cfg(feature = "fuse3")] {
			fuse3::mount(fs, &cli.mountpoint, &cli.options(), cli.foreground)?;
		} else {
			compile_error!("no FUSE backend selected");
		}
	}

	Ok(())
}
