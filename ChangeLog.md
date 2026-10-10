# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

## [Unreleased] - ReleaseDAte

### General

- The MSRV is now 1.90.0.

### Add

- `rufs`: a metadata `BufferCache`, so that UFS metadata is staged rather than
  written through.  Cylinder-group structs, both bitmaps, inodes and indirect
  blocks all go through it now; `Ufs::sync_metadata()` is the only thing that
  persists them.
- `rufs`: `BlockMapping`, `BlockRun` and the `inode_block_runs` iterator —
  canonical logical-to-physical mapping for a UFS file, walking the block map so
  a sparse region is skipped in one step.
- `rufs`: `inode_seek_data()` and `inode_seek_hole()`.
- `fuse-ufs`: `bmap`, and `lseek` for `SEEK_DATA`/`SEEK_HOLE`.
- `fuse-ufs`: `READDIRPLUS`, on the FUSE3 backend.  A kernel that asks for it
  gets a directory listing with attributes attached, instead of `ENOSYS` and a
  `GETATTR` per entry.
- `rufs`: `Ufs::dir_entries()`, a collecting form of `dir_iter` that numbers
  each entry by its position.  `READDIR` and `READDIRPLUS` are both built from
  it, so the two cannot disagree about what a directory contains.

### Fix

- A directory this driver created was not laid out the way FreeBSD lays one
  out, and the entries in it were silently misordered.  `newdir()` gave `..` a
  reclen covering the rest of the block -- which is what marks a record as the
  last entry in a block, and the last entry is the one split to make room for
  a new one.  So the first file added to a new directory was written *into*
  `..`, pushing it out of position 12, and the directory then listed `.` twice
  with `..` in the middle.  `..` now keeps its natural size and the first real
  entry packs into the space after it, which is the layout
  `resources/ufs-little.img` has.
- `Header::new()` sized a record from the length of the 256-byte name buffer
  rather than the name in it, so every record it created got the same oversized
  reclen regardless of how long its name was.
- A directory with more entries than fit in one reply was silently truncated:
  `readdir` answered any non-zero offset with an empty listing, so the second
  and subsequent requests -- exactly the ones a kernel makes to read past a
  full buffer -- returned nothing.  Offsets are now a property of the entry, so
  a read resumes where the last one stopped.
- A cylinder-group struct and its bitmaps share one `fs_bsize` block.  With the
  struct staged through the cache and the bitmaps still written straight to the
  device, the next write-back of that block rolled the bitmaps back: an
  allocation whose counter survived and whose bit did not.

## [0.6.0] - 2025-09-27

### General

- Update dependencies
- rufs: more logging

### Add

- `truncate()`
- `rename()`

### Fix

- free block accounting
- fuse2: `readlink()` when buffer is too small

## [0.5.0] - 2025-05-15

### Added 

- fuse-ufs: add cargo features for specific platforms ([#84](https://github.com/realchonk/fuse-ufs/pull/84))
- write support ([#84](https://github.com/realchonk/fuse-ufs/pull/84))
- support for FUSE2 on Linux and FreeBSD (through fuse2rs) ([#84](https://github.com/realchonk/fuse-ufs/pull/84))

## [0.4.4] - 2025-02-16

### Add

- A little more documentation
- allow other configurations of UFS ([#93](https://github.com/realchonk/fuse-ufs/pull/93))

### Fix

- calculation of fragment blocks ([#89](https://github.com/realchonk/fuse-ufs/pull/89))
- reading directories ([#87](https://github.com/realchonk/fuse-ufs/pull/87))
- inode reading ([#92](https://github.com/realchonk/fuse-ufs/pull/92))
- update dependencies

## [0.4.3] - 2024-10-25

### Fix

- fuse-ufs: mounting without `-f` on FUSE3
- scripts/release: run test suite before publishing

## [0.4.2] - 2024-10-25

### Fix

- another fix of publishing

## [0.4.1] - 2024-10-25

### Fix

- minor publishing fix

## [0.4.0] - 2024-10-25

### Added

- basic fuzzing framework ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- pre-mount checks for verifying the filesystem ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- support for OpenBSD/FUSE2 via fuse2rs ([#79](https://github.com/realchonk/fuse-ufs/pull/79))

### Changed

- split project into binary and library part ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- make fuser optional ([#79](https://github.com/realchonk/fuse-ufs/pull/79))

## [0.3.0] - 2024-08-22

This was the final release as part of [Google Summer of Code 2024](https://summerofcode.withgoogle.com/programs/2024/projects/mCAcivuH).

### Added

- ChangeLog ([#62](https://github.com/realchonk/fuse-ufs/pull/62))
- man page ([#57](https://github.com/realchonk/fuse-ufs/pull/57))
- `-v` and `-q` flags ([#66](https://github.com/realchonk/fuse-ufs/pull/66))
- `-f` option ([#68](https://github.com/realchonk/fuse-ufs/pull/68))
- the ability to mount via `/etc/fstab` on Linux ([#69](https://github.com/realchonk/fuse-ufs/pull/69))

### Changed

- fuse-ufs now starts in the background by default ([#68](https://github.com/realchonk/fuse-ufs/pull/68))

### Fixed

- indirect block addressing ([#63](https://github.com/realchonk/fuse-ufs/pull/63))

### [0.2.1] - 2024-08-15

### Changed

- README
- Fix Cargo.toml for publishing

### [0.2.0] - 2024-08-11

This was the first formal release of fuse-ufs.

[unreleased]: https://github.com/realchonk/fuse-ufs/compare/0.6.0...HEAD
[0.6.0]: https://github.com/realchonk/fuse-ufs/compare/0.6.0...0.6.0
[0.6.0]: https://github.com/realchonk/fuse-ufs/compare/0.5.0...0.6.0
[0.5.0]: https://github.com/realchonk/fuse-ufs/compare/0.4.4...0.5.0
[0.4.4]: https://github.com/realchonk/fuse-ufs/compare/0.4.3...0.4.4
[0.4.3]: https://github.com/realchonk/fuse-ufs/compare/0.4.2...0.4.3
[0.4.2]: https://github.com/realchonk/fuse-ufs/compare/0.4.1...0.4.2
[0.4.1]: https://github.com/realchonk/fuse-ufs/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/realchonk/fuse-ufs/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/realchonk/fuse-ufs/compare/0.2.1...0.3.0
[0.2.1]: https://github.com/realchonk/fuse-ufs/compare/0.2.0...0.2.1
[0.2.0]: https://github.com/realchonk/fuse-ufs/releases/tag/0.2.0
