use std::{
	ffi::{CString, OsStr, OsString},
	fmt,
	fs::{self, File},
	io::{ErrorKind, Read, Seek, SeekFrom},
	os::unix::{ffi::OsStringExt, fs::MetadataExt},
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
use rstest::rstest;
use rstest_reuse::{apply, template};
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
	pub static ref GOLDEN_BE: PathBuf = prepare_image("ufs-big.img");
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

fn harness(img: &Path) -> Harness {
	let d = tempdir().unwrap();
	let child = Command::cargo_bin("fuse-ufs")
		.unwrap()
		.arg(img)
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

#[template]
#[rstest]
#[case::le(harness(GOLDEN_LE.as_path()))]
#[case::be(harness(GOLDEN_BE.as_path()))]
fn all_images(harness: Harness) {}

/// Mount and unmount the golden image
#[apply(all_images)]
fn mount(#[case] harness: Harness) {
	drop(harness);
}

// TODO: find all files recursively
#[apply(all_images)]
fn contents(#[case] harness: Harness) {
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
		"large",
		"sparse",
		"sparse2",
		"sparse3",
		"long-link",
	];

	expected.sort();

	assert_eq!(entries, expected);
}

#[apply(all_images)]
fn readdir_large(#[case] harness: Harness) {
	let d = &harness.d;

	let mut dir = nix::dir::Dir::open(
		&d.path().join("large"),
		OFlag::O_DIRECTORY | OFlag::O_RDONLY,
		Mode::empty(),
	)
	.unwrap();

	let mut entries = dir
		.iter()
		.map(Result::unwrap)
		.map(|e| e.file_name().to_owned())
		.filter(|x| x.to_bytes()[0] != b'.')
		.collect::<Vec<_>>();

	entries.sort();

	let expected = (0..2049)
		.map(|x| format!("{x:08x}"))
		.map(|s| CString::new(s).unwrap())
		.collect::<Vec<_>>();

	assert_eq!(entries, expected);
}

#[apply(all_images)]
fn read_direct(#[case] harness: Harness) {
	let d = &harness.d;

	let file = std::fs::read_to_string(d.path().join("file1")).unwrap();
	assert_eq!(&file, "This is a simple file.\n");
}

#[apply(all_images)]
fn read_indir1(#[case] harness: Harness) {
	let d = &harness.d;

	let file = std::fs::read_to_string(d.path().join("file3")).unwrap();
	file.lines().enumerate().for_each(|(i, l)| {
		let l = &l[0..15];
		assert_eq!(l, format!("{i:015x}"));
	});
}

// TODO: read_indir{2,3} pending #29

#[apply(all_images)]
fn readlink_short(#[case] harness: Harness) {
	let d = &harness.d;

	let link = std::fs::read_link(d.path().join("link1")).unwrap();
	assert_eq!(&link, Path::new("dir1/dir2/dir3/file2"));
}

#[apply(all_images)]
fn readlink_long(#[case] harness: Harness) {
	let d = &harness.d;

	let link = std::fs::read_link(d.path().join("long-link")).unwrap();
	let expected = (0..508).map(|_| "./").fold(String::new(), |a, x| a + x) + "//file1";

	assert_eq!(link, Path::new(&expected));
}

#[apply(all_images)]
fn statfs(#[case] harness: Harness) {
	let d = &harness.d;
	let sfs = nix::sys::statfs::statfs(d.path()).unwrap();

	assert_eq!(sfs.blocks(), 3847);
	assert_eq!(sfs.blocks_free(), 1379);
	assert_eq!(sfs.blocks_available(), 1379);
	assert_eq!(sfs.files(), 2560);
	assert_eq!(sfs.files_free(), 495);
	assert_eq!(sfs.maximum_name_length(), 255);

	#[cfg(target_os = "freebsd")]
	assert_eq!(sfs.block_size(), 4096);
}

#[apply(all_images)]
fn statvfs(#[case] harness: Harness) {
	let d = &harness.d;
	let svfs = nix::sys::statvfs::statvfs(d.path()).unwrap();

	assert_eq!(svfs.fragment_size(), 4096);
	assert_eq!(svfs.blocks(), 3847);
	assert_eq!(svfs.files(), 2560);
	assert_eq!(svfs.files_free(), 495);
	assert!(svfs.flags().contains(FsFlags::ST_RDONLY));
}

#[apply(all_images)]
fn non_existent(#[case] harness: Harness) {
	let d = &harness.d;

	let path = d.path().join("non-existent");

	assert_eq!(
		std::fs::metadata(&path).unwrap_err().kind(),
		ErrorKind::NotFound
	);
}

// This tests both sparse files and 2nd level indirect block addressing
#[apply(all_images)]
fn sparse(#[case] harness: Harness) {
	let d = &harness.d;

	let mut file = File::open(d.path().join("sparse")).unwrap();
	let st = file.metadata().unwrap();

	assert_eq!(st.blocks(), 320);
	assert_eq!(st.size(), 134643712);

	file.seek(SeekFrom::Start((12 + 4096) * 32768)).unwrap();
	let mut buf = [0u8; 32768];
	file.read_exact(&mut buf).unwrap();
	let expected = [b'x'; 32768];
	assert_eq!(buf, expected);
}

#[apply(all_images)]
fn sparse_hole(#[case] harness: Harness) {
	let d = &harness.d;

	let mut file = File::open(d.path().join("sparse")).unwrap();
	file.seek(SeekFrom::Start((12 + 5) * 32768)).unwrap();
	let mut buf = [0u8; 32768];
	file.read_exact(&mut buf).unwrap();
	let expected = [0; 32768];
	assert_eq!(buf, expected);
}

// A sparse file with only a single fragment of data at the end
#[apply(all_images)]
fn sparse2(#[case] harness: Harness) {
	let d = &harness.d;

	let mut file = File::open(d.path().join("sparse2")).unwrap();
	let st = file.metadata().unwrap();

	assert_eq!(st.blocks(), 320);
	assert_eq!(st.size(), 134615040);

	file.seek(SeekFrom::Start((12 + 4096) * 32768)).unwrap();
	let mut buf = [0u8; 4096];
	file.read_exact(&mut buf).unwrap();
	let expected = [b'x'; 4096];
	assert_eq!(buf, expected);
}

// A sparse so large, it needs third level indirect block addressing.
#[apply(all_images)]
fn sparse3(#[case] harness: Harness) {
	let d = &harness.d;

	let mut file = File::open(d.path().join("sparse3")).unwrap();
	let st = file.metadata().unwrap();

	assert_eq!(st.blocks(), 448);
	assert_eq!(st.size(), 549890457600);

	file.seek(SeekFrom::Start((12 + 4096 + 4096 * 4096) * 32768))
		.unwrap();
	let mut buf = [0u8; 32768];
	file.read_exact(&mut buf).unwrap();
	let expected = [b'x'; 32768];
	assert_eq!(buf, expected);
}
