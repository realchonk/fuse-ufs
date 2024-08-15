# FUSE driver for FreeBSD's UFSv2

## Features
- Read support for FreeBSD UFSv2
- Extended Attributes (no ACLs)

## Planned Features
- Full Read & Write Support for FreeBSD & Sun UFSv2
- Softupdates

## Packages
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
