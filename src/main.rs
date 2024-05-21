use anyhow::Result;
use clap::Parser;

use crate::{cli::Cli, ufs::Ufs};

mod blockreader;
mod cli;
mod data;
mod decoder;
mod inode;
mod ufs;

fn main() -> Result<()> {
	env_logger::init();
	let cli = Cli::parse();
	// TODO: set log level to debug, if cli.verbose
	let fs = Ufs::open(&cli.device)?;

	fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	Ok(())
}
