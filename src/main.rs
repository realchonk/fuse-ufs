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
	let cli = Cli::parse();

        env_logger::builder()
            .filter_level(cli.verbose.log_level_filter())
            .init();

	let fs = Ufs::open(&cli.device)?;

	fuser::mount2(fs, &cli.mountpoint, &cli.options())?;
	Ok(())
}
