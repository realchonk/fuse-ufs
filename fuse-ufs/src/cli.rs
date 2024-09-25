use std::path::PathBuf;

use clap::Parser;
use clap_verbosity_flag::{Verbosity, WarnLevel};

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

	/// Wait until the filesystem is unmounted.
	#[arg(short)]
	pub foreground: bool,
}

impl Cli {
	#[cfg(feature = "fuse3")]
	pub fn options(&self) -> Vec<fuser::MountOption> {
		use fuser::MountOption;
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

	#[cfg(feature = "fuse2")]
	pub fn options(&self) -> anyhow::Result<Vec<fuse2rs::MountOption>> {
		use std::ffi::CString;

		use fuse2rs::MountOption;

		let mut opts = vec![MountOption::DefaultPermissions, MountOption::Ro];

		if self.foreground {
			opts.push(MountOption::Foreground);
		}

		if self
			.verbose
			.log_level()
			.map_or(false, |l| l >= clap_verbosity_flag::Level::Debug)
		{
			opts.push(MountOption::Debug);
		}

		for opt in &self.options {
			let opt = match opt.as_str() {
				"debug" => MountOption::Debug,
				"allow_other" => MountOption::AllowOther,
				"async" => MountOption::Async,
				"atime" => MountOption::Atime,
				"default_permissions" => continue,
				"dev" => MountOption::Dev,
				"exec" => MountOption::Exec,
				"noatime" => MountOption::NoAtime,
				"nodev" => MountOption::NoDev,
				"noexec" => MountOption::NoExec,
				"nosuid" => MountOption::NoSuid,
				"ro" => continue,
				"rw" => panic!("rw is not yet supported"),
				"suid" => MountOption::Suid,
				"sync" => MountOption::Sync,
				custom => MountOption::Custom(CString::new(custom)?),
			};
			opts.push(opt);
		}

		Ok(opts)
	}
}
