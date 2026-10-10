// Copyright (c) 2026 Pedro Giffuni
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this
//    list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! Test-only access to the golden UFS2 images.
//!
//! `resources/ufs-{little,big}.img.zst` are produced by `scripts/mkimg.sh` on
//! FreeBSD and are the only realistic UFS2 images available to this
//! repository's tests, so the unit tests use them exactly the way the FUSE
//! integration tests do: decompress with the system `unzstd`.
//!
//! Every call decompresses into a *fresh* temporary directory.  That is
//! deliberately wasteful — a few megabytes per test — because it removes both
//! the staleness check and the concurrent-decompression race that a shared
//! cache would need, and a unit test that flakes once because two threads
//! truncated the same file is not worth the bytes.

use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};

/// The compressed golden images shipped with the repository.
fn resources_dir() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../resources")
}

/// A decompressed copy of the named golden image, in a temporary directory
/// that is removed when the returned [`fs::DirGuard`] is dropped.
pub struct Image {
	_dir: tempfile::TempDir,
	path: PathBuf,
}

impl Image {
	/// Path to the image file.
	pub fn path(&self) -> &Path {
		&self.path
	}
}

/// Decompress `resources/{name}.img.zst` into a private temporary directory.
pub fn image(name: &str) -> Image {
	let dir = tempfile::tempdir().expect("failed to create temporary directory");
	let path = dir.path().join(format!("{name}.img"));

	let mut src = resources_dir();
	src.push(format!("{name}.img.zst"));
	assert!(
		src.exists(),
		"missing golden image {}; it is checked into the repository and \
		 compressed by scripts/mkimg.sh",
		src.display()
	);

	let out = Command::new("unzstd")
		.arg("-f")
		.arg("-o")
		.arg(&path)
		.arg(&src)
		.output()
		.expect("failed to run unzstd");
	assert!(
		out.status.success(),
		"unzstd failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	Image { _dir: dir, path }
}

/// Open a golden image read-only.
pub fn open_ro(name: &str) -> (Image, crate::Ufs<std::fs::File>) {
	let img = image(name);
	let fs = crate::Ufs::open(img.path(), false).expect("failed to open golden image");
	(img, fs)
}

/// Decompress a golden image into a private temporary directory and return it
/// for modification.
pub fn open_rw(name: &str) -> (Image, crate::Ufs<std::fs::File>) {
	let img = image(name);
	let fs = crate::Ufs::open(img.path(), true).expect("failed to open golden image (rw)");
	(img, fs)
}

/// Copy `src` to a fresh file in a temporary directory.
#[allow(dead_code)]
pub fn copy(src: &Path) -> (tempfile::TempDir, PathBuf) {
	let dir = tempfile::tempdir().expect("failed to create temporary directory");
	let dst = dir.path().join("image.img");
	fs::copy(src, &dst).expect("failed to copy image");
	(dir, dst)
}
