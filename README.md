# fuse-ufs

A [FUSE](https://github.com/libfuse/) driver for FreeBSD's **UFSv2** filesystem,
written in Rust. Mount FreeBSD UFSv2 volumes on Linux, FreeBSD, and OpenBSD —
read-only by default, with experimental write support — including bi-endian
access (e.g. mounting a big-endian filesystem on a little-endian host).

[![CI](https://github.com/realchonk/fuse-ufs/actions/workflows/ci.yml/badge.svg)](https://github.com/realchonk/fuse-ufs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rufs.svg?logo=rust)](https://crates.io/crates/rufs)
[![docs.rs](https://docs.rs/rufs/badge.svg)](https://docs.rs/rufs)
[![license](https://img.shields.io/crates/l/rufs.svg)](LICENSE.md)
[![Packaging status](https://repology.org/badge/vertical-allrepos/fusefs:ufs.svg)](https://repology.org/project/fusefs:ufs/versions)

## Features

- **Read & write** support for FreeBSD UFSv2 (write support is experimental)
- **Bi-endian** access — mount big-endian filesystems on little-endian hosts and vice versa
- **Extended attributes** (ACLs not yet supported)
- Cross-platform FUSE backends: `fuse3` (Linux, FreeBSD) and `fuse2` (OpenBSD)

## Planned

- Read & write support for Sun/Solaris UFSv2
- Soft updates

## Requirements

- Rust ≥ 1.90.0
- `libfuse3` (Linux, FreeBSD) or `libfuse2` (OpenBSD)
- A working FUSE kernel module (`fusefs` on FreeBSD, `fuse` on Linux/OpenBSD)

## Build from source

```sh
$ git clone https://github.com/realchonk/fuse-ufs
$ cd fuse-ufs
$ make
# make install
```

`make` produces the release binary `./fuse-ufs-bin`; `make install` installs the
binary, the `fuse-ufs(8)` manual page, and a `mount.ufs` symlink.

## Usage

> Replace `sdb1` with your FreeBSD UFS partition or image file.

```sh
$ fuse-ufs /dev/sdb1 /mnt                  # read-only (default)
$ fuse-ufs -o rw /dev/sdb1 /mnt            # experimental read-write
$ fuse-ufs -o allow_other /dev/sdb1 /mnt   # let other users access the mount
$ fuse-ufs -o rw,allow_other /dev/sdb1 /mnt   # options may be combined
```

See **fuse-ufs(8)** for the full list of options.

### Mounting via `/etc/fstab` (Linux)

```fstab
/dev/sdb1   /mnt    fuse.fuse-ufs   ro  0  0
```

or, via the `mount.ufs` symlink installed by `make install`:

```fstab
/dev/sdb1   /mnt    ufs             ro  0  0
```

## Project layout

This is a Cargo workspace with three crates:

| Crate       | Description                                       |
|-------------|---------------------------------------------------|
| [`rufs`]    | Core, platform-independent UFSv2 library.         |
| `fuse-ufs`  | The mountable FUSE driver binary.                 |
| `fuzz`      | `libfuzzer-sys` fuzzing harness.                  |

[`rufs`]: https://crates.io/crates/rufs

## License

Licensed under the [BSD-2-Clause](LICENSE.md) license.

## Acknowledgements

This project was sponsored as part of
[Google Summer of Code 2024](https://summerofcode.withgoogle.com/programs/2024/projects/mCAcivuH).
The final release during GSoC was [0.3.0](https://github.com/realchonk/fuse-ufs/releases/tag/0.3.0).
