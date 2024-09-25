use std::{
	ffi::{OsStr, OsString},
	fmt,
	fs::{self, File},
	io::{ErrorKind, Read, Seek, SeekFrom},
	os::{
		fd::AsRawFd,
		unix::{ffi::OsStringExt, fs::MetadataExt},
	},
	path::{Path, PathBuf},
	process::{Child, Command},
	thread::sleep,
	time::{Duration, Instant},
};

use assert_cmd::cargo::CommandCargoExt;
use cfg_if::cfg_if;
use cstr::cstr;
use lazy_static::lazy_static;
use nix::{
	fcntl::OFlag,
	sys::{stat::Mode, statvfs::FsFlags},
};
use rstest::rstest;
use rstest_reuse::{apply, template};
use tempfile::{tempdir, TempDir};
use xattr::FileExt;

fn errno() -> i32 {
	nix::errno::Errno::last_raw()
}

fn prepare_image(filename: &str) -> PathBuf {
	let mut zimg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	zimg.push("../resources");
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
		.arg("-f")
		.arg(img)
		.arg(d.path())
		.spawn()
		.unwrap();

	waitfor(Duration::from_secs(5), || {
		let s = nix::sys::statfs::statfs(d.path()).unwrap();
		cfg_if! {
			if #[cfg(any(target_os = "freebsd", target_os = "macos", target_os = "openbsd"))] {
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
		"xattrs",
		"sparse",
		"sparse2",
		"sparse3",
		"xattrs2",
		"xattrs3",
		"long-link",
	];

	expected.sort();

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

	assert_eq!(sfs.blocks(), 871);
	assert_eq!(sfs.blocks_free(), 430);
	assert_eq!(sfs.blocks_available(), 430);
	assert_eq!(sfs.files(), 1024);
	assert_eq!(sfs.files_free(), 1006);
	#[cfg(not(target_os = "macos"))]
	assert_eq!(sfs.maximum_name_length(), 255);

	#[cfg(target_os = "freebsd")]
	assert_eq!(sfs.block_size(), 4096);
}

#[apply(all_images)]
fn statvfs(#[case] harness: Harness) {
	let d = &harness.d;
	let svfs = nix::sys::statvfs::statvfs(d.path()).unwrap();

	assert_eq!(svfs.fragment_size(), 4096);
	assert_eq!(svfs.blocks(), 871);
	assert_eq!(svfs.files(), 1024);
	assert_eq!(svfs.files_free(), 1006);
	assert!(svfs.flags().contains(FsFlags::ST_RDONLY));
}

#[apply(all_images)]
fn non_existent(#[case] harness: Harness) {
	let d = &harness.d;

	let path = d.path().join("non-existent");

	assert_eq!(
		std::fs::metadata(path).unwrap_err().kind(),
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

// This checks, that issue #54 doesn't happen.
#[apply(all_images)]
fn sparse3_issue54(#[case] harness: Harness) {
	let d = &harness.d;

	let mut file = File::open(d.path().join("sparse3")).unwrap();
	file.seek(SeekFrom::Start(549883084800)).unwrap();
	let mut buf = [0u8; 128 * 1024];
	file.read_exact(&mut buf).unwrap();
	let expected = [0; 128 * 1024];
	assert_eq!(buf, expected);
}

#[apply(all_images)]
fn listxattr(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("xattrs")).unwrap();
	let xattrs = file.list_xattr().unwrap().collect::<Vec<_>>();
	let expected = [OsStr::new("user.test")];
	assert_eq!(xattrs, expected);
}

#[cfg(target_os = "freebsd")]
#[apply(all_images)]
fn listxattr_size(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("xattrs")).unwrap();
	let num = unsafe {
		libc::extattr_list_fd(
			file.as_raw_fd(),
			libc::EXTATTR_NAMESPACE_USER,
			std::ptr::null_mut(),
			0,
		)
	};
	assert_eq!(num, 5); // strlen("test\0")
}

#[apply(all_images)]
fn getxattr(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("xattrs")).unwrap();
	let data = file.get_xattr("user.test").unwrap().unwrap();
	let expected = b"testvalue";
	assert_eq!(data, expected);
}

#[cfg(target_os = "freebsd")]
#[apply(all_images)]
fn getxattr_size(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("xattrs")).unwrap();
	let expected = b"testvalue";

	// Can't use c"test" syntax, because the apply macro doesn't like it
	let name = cstr!(b"test");
	let num = unsafe {
		libc::extattr_get_fd(
			file.as_raw_fd(),
			libc::EXTATTR_NAMESPACE_USER,
			name.as_ptr(),
			std::ptr::null_mut(),
			0,
		)
	};
	assert_eq!(num, expected.len() as isize);
}

#[apply(all_images)]
fn noxattrs(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("file1")).unwrap();
	let xattrs = file.list_xattr().unwrap().collect::<Vec<_>>();
	assert_eq!(xattrs.len(), 0);
}

#[cfg(target_os = "freebsd")]
#[apply(all_images)]
fn noxattrs_list(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("file1")).unwrap();
	let num = unsafe {
		libc::extattr_list_fd(
			file.as_raw_fd(),
			libc::EXTATTR_NAMESPACE_USER,
			std::ptr::null_mut(),
			0,
		)
	};
	assert_eq!(num, 0);
}

#[cfg(target_os = "freebsd")]
#[apply(all_images)]
fn noxattrs_get(#[case] harness: Harness) {
	let d = &harness.d;

	let file = File::open(d.path().join("file1")).unwrap();
	let name = cstr!(b"test");
	let num = unsafe {
		libc::extattr_get_fd(
			file.as_raw_fd(),
			libc::EXTATTR_NAMESPACE_USER,
			name.as_ptr(),
			std::ptr::null_mut(),
			0,
		)
	};
	assert_eq!(num, -1);
	assert_eq!(errno(), libc::ENOATTR);
}

#[apply(all_images)]
fn many_xattrs(#[case] harness: Harness) {
	let d = &harness.d;
	let max = 2297;

	let file = File::open(d.path().join("xattrs2")).unwrap();
	let xattrs = file.list_xattr().unwrap().collect::<Vec<_>>();
	let expected = (1..=max)
		.map(|i| OsString::from(format!("user.attr{i}")))
		.collect::<Vec<_>>();
	assert_eq!(xattrs, expected);

	for i in 1..=max {
		let name = format!("user.attr{i}");
		let data = file.get_xattr(name).unwrap().unwrap();
		let expected = format!("value{i}");
		assert_eq!(data, expected.as_bytes());
	}
}

#[apply(all_images)]
fn big_xattr(#[case] harness: Harness) {
	use std::io::Write;
	let d = &harness.d;

	let file = File::open(d.path().join("xattrs3")).unwrap();
	let data = file.get_xattr("user.big").unwrap().unwrap();
	let mut expected = (0..4000).fold(Vec::new(), |mut s, i| {
		writeln!(&mut s, "{i:015x}").unwrap();
		s
	});
	expected.pop(); // remove the trailing '\n'

	// first check for the size, to avoid spamming the output
	assert_eq!(data.len(), expected.len());
	assert_eq!(data, expected);
}
