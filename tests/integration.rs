use std::{
	ffi::OsString,
	fmt,
	fs,
	os::unix::ffi::OsStringExt,
	path::{Path, PathBuf},
	process::{Child, Command},
	thread::sleep,
	time::{Duration, Instant},
};

use assert_cmd::cargo::CommandCargoExt;
use cfg_if::cfg_if;
use lazy_static::lazy_static;
use nix::{
	fcntl::OFlag,
	sys::{stat::Mode, statvfs::FsFlags},
};
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
	// TODO: GOLDEN_BIG and other configs, like 64K/8K, 4K/4k, etc.
	pub static ref GOLDEN_LE: PathBuf = prepare_image("ufs-little.img");
	//pub static ref GOLDEN_BE: PathBuf = prepare_image("ufs-big.img");
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
		.arg(GOLDEN_LE.as_path())
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

// TODO: find all files recursively
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

	let mut expected = [
		".",
		"..",
		".snap",
		"dir1",
		"file1",
		"file3",
		"link1",
		"long-link",
	];

	expected.sort();

	assert_eq!(entries, expected);
}

#[rstest]
fn read_direct(harness: Harness) {
	let d = &harness.d;

	let file = std::fs::read_to_string(d.path().join("file1")).unwrap();
	assert_eq!(&file, "This is a simple file.\n");
}

#[rstest]
fn read_indir1(harness: Harness) {
	let d = &harness.d;

	let file = std::fs::read_to_string(d.path().join("file3")).unwrap();
	file.lines().enumerate().for_each(|(i, l)| {
		let l = &l[0..15];
		assert_eq!(l, format!("{i:015x}"));
	});
}

#[rstest]
fn readlink_short(harness: Harness) {
	let d = &harness.d;

	let link = std::fs::read_link(d.path().join("link1")).unwrap();
	assert_eq!(&link, Path::new("dir1/dir2/dir3/file2"));
}

#[rstest]
fn readlink_long(harness: Harness) {
	let d = &harness.d;

	let link = std::fs::read_link(d.path().join("long-link")).unwrap();
	let expected = (0..508).map(|_| "./").fold(String::new(), |a, x| a + x) + "//file1";

	assert_eq!(link, Path::new(&expected));
}

#[rstest]
fn statfs(harness: Harness) {
	let d = &harness.d;
	let sfs = nix::sys::statfs::statfs(d.path()).unwrap();

	assert_eq!(sfs.blocks(), 15751);
	assert_eq!(sfs.files(), 8704);
	assert_eq!(sfs.files_free(), 8692);
}

#[rstest]
fn statvfs(harness: Harness) {
	let d = &harness.d;
	let svfs = nix::sys::statvfs::statvfs(d.path()).unwrap();

	//assert_eq!(svfs.block_size(), 32768);
	assert_eq!(svfs.fragment_size(), 4096);
	assert_eq!(svfs.blocks(), 15751);
	assert_eq!(svfs.files(), 8704);
	assert_eq!(svfs.files_free(), 8692);
	assert!(svfs.flags().contains(FsFlags::ST_RDONLY));
}
