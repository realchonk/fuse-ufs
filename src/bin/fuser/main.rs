use anyhow::Result;
use clap::Parser;

use fuse_ufs::Ufs;
use crate::cli::Cli;

mod cli;

fn main() -> Result<()> {
	let cli = Cli::parse();

	env_logger::builder()
		.filter_level(cli.verbose.log_level_filter())
		.init();

	let fs = Ufs::open(&cli.device)?;

	if cli.foreground {
		fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	} else {
		fuser::spawn_mount2(fs, &cli.mountpoint, &cli.options())?;
	}

	Ok(())
}
