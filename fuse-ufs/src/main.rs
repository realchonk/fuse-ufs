use std::fs::File;

use anyhow::Result;
use cfg_if::cfg_if;
use clap::Parser;
use nix::unistd::daemon;
use rufs::Ufs;

use crate::cli::Cli;

#[allow(clippy::unnecessary_cast)]
mod consts {
	pub const S_IFMT: u32 = libc::S_IFMT as u32;
	pub const S_IFREG: u32 = libc::S_IFREG as u32;
	pub const S_IFDIR: u32 = libc::S_IFDIR as u32;
	pub const S_IFCHR: u32 = libc::S_IFCHR as u32;
	pub const S_IFBLK: u32 = libc::S_IFBLK as u32;
	pub const S_IFLNK: u32 = libc::S_IFLNK as u32;
	pub const S_IFIFO: u32 = libc::S_IFIFO as u32;
	pub const S_IFSOCK: u32 = libc::S_IFSOCK as u32;
}

macro_rules! err {
	($n:ident) => {
		std::io::Error::from_raw_os_error(libc::$n)
	};
}

mod cli;

#[cfg(feature = "fuse3")]
mod fuse3;

#[cfg(feature = "fuse2")]
mod fuse2;

pub(crate) struct Fs {
	pub(crate) ufs: Ufs<File>,
	/// Whether the mount was opened for writing.
	///
	/// A read-only mount has nothing to drain, and `Ufs::shutdown()` would say
	/// so; the flag only exists so the teardown can say why.
	pub(crate) rw:  bool,
}

/// Drain on the way out.
///
/// `Ufs` stages metadata changes and publishes them when a sync runs, and a
/// filesystem that is dropped without one loses whatever was still queued.  For
/// an ordinary unmount that is invisible -- everything was already flushed -- but
/// for a crash-free teardown after a `kill`, or for the FUSE session ending
/// with dirty metadata, it is silent data loss.
///
/// This cannot report an error: `Drop` has no way to return one, and refusing to
/// drop would be worse than dropping.  So it logs what it could not finish, which
/// is the only thing left that a user can act on.
impl Drop for Fs {
	fn drop(&mut self) {
		if !self.rw {
			return;
		}
		match self.ufs.shutdown() {
			Ok(st) if st.is_drained() => log::debug!("unmount: metadata drained"),
			Ok(st) => {
				log::error!(
					"unmount: metadata is still outstanding and has NOT been written: {st:?}"
				)
			}
			Err(e) => log::error!("unmount: drain failed, metadata may be incomplete: {e}"),
		}
	}
}

fn main() -> Result<()> {
	let cli = Cli::parse();

	env_logger::builder()
		.filter_level(cli.verbose.log_level_filter())
		.init();

	let (opts, rw) = cli.options()?;

	let fs = Fs {
		ufs: Ufs::open(&cli.device, rw)?,
		rw,
	};

	let mp = &cli.mountpoint;
	cfg_if! {
		if #[cfg(all(feature = "fuse3", feature = "fuse2"))] {
			compile_error!("more than one FUSE backend selected")
		} else if #[cfg(feature = "fuse3")] {
			if cli.foreground {
				fuser::mount2(fs, mp, &opts)?;
			} else {
				daemon(false, false)?;
				fuser::mount2(fs, mp, &opts)?;
			}
		} else if #[cfg(feature = "fuse2")] {
			fuse2rs::mount(mp, fs, opts)?;
		} else {
			compile_error!("no FUSE backend selected");
		}
	}

	Ok(())
}
