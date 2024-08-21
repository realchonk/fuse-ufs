use std::path::PathBuf;

use clap::Parser;
use clap_verbosity_flag::{Verbosity, WarnLevel};
use fuser::MountOption;

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
	/// Mount options to pass to the kernel
	#[arg(short, long, value_delimiter(','))]
	pub options: Vec<String>,

	/// Path to the device
	pub device:     PathBuf,
	/// Path to the mount point
	pub mountpoint: PathBuf,

	#[command(flatten)]
	pub verbose: Verbosity<WarnLevel>,
}

impl Cli {
	pub fn options(&self) -> Vec<MountOption> {
		let mut opts = vec![
			MountOption::FSName("fusefs".into()),
			MountOption::Subtype("ufs".into()),
			MountOption::DefaultPermissions,
			MountOption::RO,
		];

		for opt in &self.options {
			let opt = match opt.as_str() {
				"allow_other" => MountOption::AllowOther,
				"allow_root" => MountOption::AllowRoot,
				"async" => MountOption::Async,
				"atime" => MountOption::Atime,
				"auto_unmount" => MountOption::AutoUnmount,
				"default_permissions" => continue,
				"dev" => MountOption::Dev,
				"dirsync" => MountOption::DirSync,
				"exec" => MountOption::Exec,
				"noatime" => MountOption::NoAtime,
				"nodev" => MountOption::NoDev,
				"noexec" => MountOption::NoExec,
				"nosuid" => MountOption::NoSuid,
				"ro" => continue,
				"rw" => panic!("rw is not yet supported"),
				"suid" => MountOption::Suid,
				"sync" => MountOption::Sync,
				custom => MountOption::CUSTOM(custom.into()),
			};
			opts.push(opt);
		}

		opts
	}
}
