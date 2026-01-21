# FUSE driver for FreeBSD UFS (UFSv1 and UFSv2)

## Supported Platforms

- Linux (64-bit)
- FreeBSD (64-bit)
- OpenBSD (64-bit)
- macOS (64-bit)

## Features

- **UFSv2**: Read and Write support for FreeBSD UFSv2
- **UFSv1**: Read-only support for UFSv1 filesystems (FreeBSD, Solaris/SunOS, NeXTStep, OpenStep, HP-UX variants)
- Extended Attributes (UFSv2 only, no ACLs)
- Bi-Endian support (eg. mounting big endian FS on little endian system)
- Cross-platform: Linux, FreeBSD, OpenBSD, and macOS

## Supported Platforms

- **Linux** (requires libfuse3 or libfuse2)
- **FreeBSD**
- **OpenBSD**
- **macOS** (requires macFUSE - install via `brew install macfuse`)

## Supported UFS Versions

- **UFSv1** (read-only) - FreeBSD, Solaris/SunOS, NeXTStep, OpenStep, HP-UX variants
- **UFSv2** (read-write experimental) - FreeBSD 5.x+, OpenBSD

## Planned Features

- Read & Write Support for Sun UFSv2
- Softupdates

## Packages

[![Packaging status](https://repology.org/badge/vertical-allrepos/fusefs:ufs.svg)](https://repology.org/project/fusefs:ufs/versions)

## Dependencies

- rust >= 1.85.0
- libfuse3 or libfuse2 (for Linux)
- macFUSE (for macOS - install via `brew install macfuse`)

## Building from source

```sh
$ git clone https://github.com/realchonk/fuse-ufs
$ cd fuse-ufs
$ make
# make install
```

## Example Usage

Note: replace `sdb1` with your FreeBSD's UFS partition.

```sh
fuse-ufs /dev/sdb1 /mnt
```

### Mounting via fstab (on Linux)

```fstab
/dev/sdb1   /mnt    fuse.fuse-ufs   ro 0 0
```

or

```fstab
/dev/sdb1   /mnt    ufs             ro 0 0
```

## Sponsorship

This project was sponsored as part of [Google Summer of Code 2024]($https://summerofcode.withgoogle.com/programs/2024/projects/mCAcivuH).
The final release during GSoC was [0.3.0](https://github.com/realchonk/fuse-ufs/releases/tag/0.3.0).
