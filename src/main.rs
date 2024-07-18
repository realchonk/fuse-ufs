use anyhow::Result;
use clap::Parser;
use log::LevelFilter;

use crate::{cli::Cli, decoder::Config, ufs::Ufs};

mod blockreader;
mod cli;
mod data;
mod decoder;
mod inode;
mod ufs;

fn main() -> Result<()> {
	env_logger::init();
	let cli = Cli::parse();

	if cli.verbose {
		log::set_max_level(LevelFilter::Trace);
	}

	let cfg = Config::new(cli.endian);

	// TODO: set log level to debug, if cli.verbose
	let fs = Ufs::open(&cli.device, cfg)?;

	fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	Ok(())
}
