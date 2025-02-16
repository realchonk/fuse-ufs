use std::{fs::File, hint::black_box, path::{Component, Path, PathBuf}, process::Command, time::Duration};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rufs::{InodeNum, InodeType, Ufs};

// This has around 1800 files.
const IMAGE: &str = "openbsd76-root";

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
	let zmtime = std::fs::metadata(&zimg).unwrap().modified().unwrap();
	let mtime = std::fs::metadata(&img);
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

fn lookup(ufs: &mut Ufs<File>, path: &Path) -> InodeNum {
	let mut comps = path.components();
	assert_eq!(comps.next(), Some(Component::RootDir));
	let mut inr = InodeNum::ROOT;
	for c in comps {
		match c {
			Component::Normal(name) => {
				inr = ufs.dir_lookup(inr, name).unwrap();
			},
			_ => unreachable!("unexpected path component: {c:?}")
		}
	}

	inr
}

/// just open the image
fn open(c: &mut Criterion) {
	let img = prepare_image(IMAGE);
	c.bench_function("open", |b| b.iter(|| {
		let ufs = Ufs::open(&img, false).unwrap();
		black_box(ufs);
	}));
}

fn read(c: &mut Criterion) {
	let img = prepare_image(IMAGE);
	let path = Path::new("/bsd");
	let mut group = c.benchmark_group("read");

	let mut ufs = Ufs::open(&img, false).unwrap();
	let inr = lookup(&mut ufs, path);
	let meta = ufs.inode_attr(inr).unwrap();

	group.measurement_time(Duration::from_secs(30));
	group.throughput(Throughput::Bytes(meta.size));
	group.sample_size(50);

	for bs in [1048576, 65536, 16384, 4096, 512] {
		let mut buf = vec![0u8; bs];
		let len = buf.len() as u64;
		let num = meta.size / len;
		group.bench_function(bs.to_string(), |b| b.iter(|| {
			for i in 0..num {
				let _ = black_box(ufs.inode_read(inr, black_box(i * len), black_box(&mut buf))).unwrap();
			}
		}));
	}

	group.finish();
}

fn find(c: &mut Criterion) {
	let img = prepare_image(IMAGE);
	let mut group = c.benchmark_group("find");


	// traverse through the entire filesystem, equivalent to running `find -x mp`
	group.measurement_time(Duration::from_secs(15));
	group.bench_function("direct", |b| b.iter(|| {
		fn traverse(ufs: &mut Ufs<File>, dinr: InodeNum) {
			let mut subdirs = Vec::new();
			let _ = ufs.dir_iter(dinr, |name, inr, kind| {
				let name = black_box(name);
				if name == "." || name == ".." {
					return None;
				}

				if black_box(kind) == InodeType::Directory {
					subdirs.push(black_box(inr));
				}

				None::<()>
			}).unwrap();

			for dir in subdirs {
				traverse(ufs, dir);
			}
		}
		let mut ufs = Ufs::open(&img, false).unwrap();
		traverse(&mut ufs, InodeNum::ROOT)
	}));


	// similar to `direct`, but perform path lookups instead
	group.measurement_time(Duration::from_secs(30));
	group.bench_function("lookup", |b| b.iter(|| {
		fn traverse(ufs: &mut Ufs<File>, path: &Path) {
			let mut subdirs = Vec::new();
			let dinr = black_box(lookup(ufs, path));
			let _ = ufs.dir_iter(dinr, |name, _inr, kind| {
				let name = black_box(name);
				if name == "." || name == ".." {
					return None;
				}

				if black_box(kind) == InodeType::Directory {
					subdirs.push(path.join(name));
				}

				None::<()>
			}).unwrap();

			for dir in subdirs {
				traverse(ufs, &dir);
			}
		}

		let mut ufs = Ufs::open(&img, false).unwrap();
		traverse(&mut ufs, Path::new("/"));
	}));

	// similar to `lookup`, but also perform stat() on files
	group.measurement_time(Duration::from_secs(30));
	group.sample_size(50);
	group.bench_function("lookup+stat", |b| b.iter(|| {
		fn traverse(ufs: &mut Ufs<File>, path: &Path) {
			let inr = black_box(lookup(ufs, path));
			let meta = black_box(ufs.inode_attr(inr).unwrap());

			match meta.kind {
				InodeType::Directory => {
					let mut children = Vec::new();
					let _ = ufs.dir_iter(inr, |name, _inr, _kind| {
						let name = black_box(name);
						if name != "." && name != ".." {
							children.push(path.join(name));
						}
						None::<()>
					}).unwrap();

					for child in children {
						traverse(ufs, &child);
					}
				},
				_ => {},
			}
		}

		let mut ufs = Ufs::open(&img, false).unwrap();
		traverse(&mut ufs, Path::new("/"));
	}));

	// similar to `lookup+stat`, but also read files and links
	group.measurement_time(Duration::from_secs(60));
	group.sample_size(10);
	for bs in [65536, 16384, 4096, 512] {
		let mut buf = vec![0u8; bs];
		group.bench_function(format!("lookup+stat+read-{bs}"), |b| b.iter(|| {
			fn traverse(ufs: &mut Ufs<File>, path: &Path, buf: &mut [u8]) {
				let inr = black_box(lookup(ufs, path));
				let meta = black_box(ufs.inode_attr(inr).unwrap());

				match meta.kind {
					InodeType::Directory => {
						let mut children = Vec::new();
						let _ = ufs.dir_iter(inr, |name, _inr, _kind| {
							let name = black_box(name);
							if name != "." && name != ".." {
								children.push(path.join(name));
							}
							None::<()>
						}).unwrap();

						for child in children {
							traverse(ufs, &child, buf);
						}
					},
					InodeType::Symlink => {
						let _ = black_box(ufs.symlink_read(inr).unwrap());
					},
					InodeType::RegularFile => {
						let len = buf.len() as u64;
						let num = meta.size / len;

						for i in 0..num {
							let _ = black_box(ufs.inode_read(inr, i * len, buf).unwrap());
						}
					},
					_ => {},
				}
			}

			let mut ufs = Ufs::open(&img, false).unwrap();
			traverse(&mut ufs, Path::new("/"), &mut buf);
		}));
	}

	group.finish();
}

criterion_group!(benches, open, read, find);
criterion_main!(benches);
