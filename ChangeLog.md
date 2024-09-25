# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

## [0.4.0] - unreleased

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

[0.4.0]: https://github.com/realchonk/fuse-ufs/compare/0.3.0...HEAD
[0.3.0]: https://github.com/realchonk/fuse-ufs/compare/0.2.1...0.3.0
[0.2.1]: https://github.com/realchonk/fuse-ufs/compare/0.2.0...0.2.1
[0.2.0]: https://github.com/realchonk/fuse-ufs/releases/tag/0.2.0
