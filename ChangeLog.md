# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

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

[unreleased]: https://github.com/realchonk/fuse-ufs/compare/0.5.0...HEAD
[0.5.0]: https://github.com/realchonk/fuse-ufs/compare/0.4.4...0.5.0
[0.4.4]: https://github.com/realchonk/fuse-ufs/compare/0.4.3...0.4.4
[0.4.3]: https://github.com/realchonk/fuse-ufs/compare/0.4.2...0.4.3
[0.4.2]: https://github.com/realchonk/fuse-ufs/compare/0.4.1...0.4.2
[0.4.1]: https://github.com/realchonk/fuse-ufs/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/realchonk/fuse-ufs/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/realchonk/fuse-ufs/compare/0.2.1...0.3.0
[0.2.1]: https://github.com/realchonk/fuse-ufs/compare/0.2.0...0.2.1
[0.2.0]: https://github.com/realchonk/fuse-ufs/releases/tag/0.2.0
