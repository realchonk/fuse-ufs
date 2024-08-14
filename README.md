# FUSE UFS driver for FreeBSD

## Features
- Read support for FreeBSD UFSv2
- Extended Attributes (no ACLs)

## Planned Features
- Full Read & Write Support for FreeBSD & Sun UFSv2
- Softupdates

## Packages
- [Arch Linux AUR](https://aur.archlinux.org/packages/fuse-ufs)
- FreeBSD (TODO)

## Dependencies
- rust >= 1.74.0
- libfuse3

## Installation
```sh
$ git clone https://github.com/realchonk/fuse-ufs
$ cd fuse-ufs
$ make
# make install
```
