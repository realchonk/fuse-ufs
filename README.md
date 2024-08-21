# FUSE driver for FreeBSD's UFSv2

## Features
- Read support for FreeBSD UFSv2
- Extended Attributes (no ACLs)
- Bi-Endian support (eg. mounting big endian FS on little endian system)

## Planned Features
- Full Read & Write Support for FreeBSD & Sun UFSv2
- Softupdates

## Packages
[![Packaging status](https://repology.org/badge/vertical-allrepos/fusefs:ufs.svg)](https://repology.org/project/fusefs:ufs/versions)

- [Arch Linux AUR](https://aur.archlinux.org/packages/fuse-ufs)
- FreeBSD (TODO)
- [crates.io](https://crates.io/crates/fuse-ufs)

## Dependencies
- rust >= 1.74.0
- libfuse3

## Building from source
```sh
$ git clone https://github.com/realchonk/fuse-ufs
$ cd fuse-ufs
$ make
# make install
```

## Sponsorship
This project was sponsored as part of [Google Summer of Code 2024]($https://summerofcode.withgoogle.com/programs/2024/projects/mCAcivuH).
The final release during GSoC was [0.3.0](https://github.com/realchonk/fuse-ufs/releases/tag/0.3.0).
