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

static mut CONFIG: Option<Config> = None;

pub fn config() -> Config {
	// SAFETY: it's only ever written to in main()
	unsafe { CONFIG.expect("decoder configuration not set") }
}

fn main() -> Result<()> {
	env_logger::init();
	let cli = Cli::parse();

	if cli.verbose {
		log::set_max_level(LevelFilter::Trace);
	}

	let cfg = Config::new(cli.endian);

	// SAFETY: it's written to only once.
	unsafe { CONFIG = Some(cfg) };

	// TODO: set log level to debug, if cli.verbose
	let fs = Ufs::open(&cli.device)?;

	fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	Ok(())
}
