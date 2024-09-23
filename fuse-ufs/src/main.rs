use anyhow::Result;
use clap::Parser;

use crate::{cli::Cli, fs::Fs};

mod cli;
mod fs;

fn main() -> Result<()> {
	let cli = Cli::parse();

	env_logger::builder()
		.filter_level(cli.verbose.log_level_filter())
		.init();

	let fs = Fs::open(&cli.device)?;

	if cli.foreground {
		fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	} else {
		fuser::spawn_mount2(fs, &cli.mountpoint, &cli.options())?;
	}

	Ok(())
}
