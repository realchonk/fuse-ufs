use std::{
	ffi::OsString,
	fmt,
	fs,
	os::unix::ffi::OsStringExt,
	path::PathBuf,
	process::{Child, Command},
	thread::sleep,
	time::{Duration, Instant},
};

use assert_cmd::cargo::CommandCargoExt;
use cfg_if::cfg_if;
use lazy_static::lazy_static;
use nix::{fcntl::OFlag, sys::stat::Mode};
use rstest::{fixture, rstest};
use tempfile::{tempdir, TempDir};

fn prepare_image(filename: &str) -> PathBuf {
	let mut zimg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	zimg.push("resources");
	zimg.push(filename);
	zimg.set_extension("img.zst");
	let mut img = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
	img.push(filename);

	// If the golden image doesn't exist, or is out of date, rebuild it
	// Note: we can't accurately compare the two timestamps with less than 1
	// second granularity due to a zstd bug.
	// https://github.com/facebook/zstd/issues/3748
	let zmtime = fs::metadata(&zimg).unwrap().modified().unwrap();
	let mtime = fs::metadata(&img);
	if mtime.is_err() || (mtime.unwrap().modified().unwrap() + Duration::from_secs(1)) < zmtime {
		Command::new("unzstd")
			.arg("-f")
			.arg("-o")
			.arg(&img)
			.arg(&zimg)
			.output()
			.expect("Uncompressing golden image failed");
	}
	img
}

lazy_static! {
	pub static ref GOLDEN: PathBuf = prepare_image("ufs.img");
}

#[derive(Clone, Copy, Debug)]
pub struct WaitForError;

impl fmt::Display for WaitForError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "timeout waiting for condition")
	}
}

impl std::error::Error for WaitForError {}

/// Wait for a limited amount of time for the given condition to be true.
pub fn waitfor<C>(timeout: Duration, condition: C) -> Result<(), WaitForError>
where
	C: Fn() -> bool,
{
	let start = Instant::now();
	loop {
		if condition() {
			break Ok(());
		}
		if start.elapsed() > timeout {
			break (Err(WaitForError));
		}
		sleep(Duration::from_millis(50));
	}
}

struct Harness {
	d:     TempDir,
	child: Child,
}

#[fixture]
fn harness() -> Harness {
	let d = tempdir().unwrap();
	let child = Command::cargo_bin("fuse-ufs")
		.unwrap()
		.arg(GOLDEN.as_path())
		.arg(d.path())
		.spawn()
		.unwrap();

	waitfor(Duration::from_secs(5), || {
		let s = nix::sys::statfs::statfs(d.path()).unwrap();
		cfg_if! {
			if #[cfg(target_os = "freebsd")] {
				s.filesystem_type_name() == "fusefs.ufs"
			} else if #[cfg(target_os = "linux")] {
				s.filesystem_type() == nix::sys::statfs::FUSE_SUPER_MAGIC
			}
		}
	})
	.unwrap();

	Harness { d, child }
}

impl Drop for Harness {
	#[allow(clippy::if_same_then_else)]
	fn drop(&mut self) {
		loop {
			let cmd = Command::new("umount").arg(self.d.path()).output();
			match cmd {
				Err(e) => {
					eprintln!("Executing umount failed: {}", e);
					if std::thread::panicking() {
						// Can't double panic
						return;
					}
					panic!("Executing umount failed");
				}
				Ok(output) => {
					let errmsg = OsString::from_vec(output.stderr).into_string().unwrap();
					if output.status.success() {
						break;
					} else if errmsg.contains("not a file system root directory") {
						// The daemon probably crashed.
						break;
					} else if errmsg.contains("Device busy") {
						println!("{}", errmsg);
					} else {
						if std::thread::panicking() {
							// Can't double panic
							println!("{}", errmsg);
							return;
						}
						panic!("{}", errmsg);
					}
				}
			}
			sleep(Duration::from_millis(50));
		}
		let _ = self.child.wait();
	}
}

/// Mount and unmount the golden image
#[rstest]
fn mount(harness: Harness) {
	drop(harness);
}

#[rstest]
fn contents(harness: Harness) {
	let d = &harness.d;
	let mut dir = nix::dir::Dir::open(
		d.path(),
		OFlag::O_DIRECTORY | OFlag::O_RDONLY,
		Mode::empty(),
	)
	.unwrap();

	let mut entries = dir
		.iter()
		.map(|x| x.unwrap())
		.map(|e| String::from_utf8(e.file_name().to_bytes().to_vec()).unwrap())
		.collect::<Vec<_>>();

	entries.sort();

	let mut expected = [".", "..", ".snap", "dir1", "file1", "file3"];

	expected.sort();

	assert_eq!(entries, expected);

	let file1 = std::fs::read_to_string(d.path().join("file1")).unwrap();
	assert_eq!(&file1, "This is a simple file.\n");

	let file3 = std::fs::read_to_string(d.path().join("file3")).unwrap();
	file3.lines().enumerate().for_each(|(i, l)| {
		let l = &l[0..15];
		assert_eq!(l, format!("{i:015x}"));
	});
}
