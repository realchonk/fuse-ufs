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

## Dependencies
- rust >= 1.74.0
- libfuse3 or libfuse2 (for fuse-ufs)

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
$ fuse-ufs /dev/sdb1 /mnt
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
