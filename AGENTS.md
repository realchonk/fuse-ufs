# AGENTS.md

Rust workspace implementing a FUSE driver for FreeBSD's UFSv2. The facts below are the operational ones agents tend to get wrong.

## Workspace

Three crates (`Cargo.toml`):
- **`rufs`** — core UFS2 library. Platform-independent; no FUSE dep by default.
- **`fuse-ufs`** — the mount binary; thin adapter over `rufs`.
- **`fuzz`** — `libfuzzer-sys` harness (`cargo +nightly fuzz run ufs`).

## Commands (use the Makefile targets, not raw cargo)

```sh
make            # release build -> ./fuse-ufs-bin
make prepare    # fmt + lint   (run before committing)
make fmt        # cargo +nightly fmt  + scripts/fmt-changelog.sh
make lint       # cargo clippy --all-targets -- -Dwarnings
make test       # rufs unit tests + fuse-ufs integration tests
make mount      # decompress little-endian image and run fuse-ufs on ./mp
```

Non-obvious flags the Makefile injects (`FUSE_UFS_FLAGS`): the fuse-ufs binary is built with `--ignore-rust-version --no-default-features -F $(uname)`. The `Linux`/`FreeBSD`/`OpenBSD` features select the FUSE backend (`fuse3`/`fuser` vs. `fuse2`/`fuse2rs`); exactly one backend must be enabled (`main.rs` `compile_error!`s otherwise).

## Toolchain / MSRV

- MSRV **1.90.0**. On newer toolchains cargo warns about rust-version, so the Makefile and CI pass `--ignore-rust-version` everywhere. Mirror that when invoking cargo directly.
- **Formatting requires nightly**: `rustfmt.toml` uses unstable options (`group_imports = "StdExternalCrate"`, `imports_granularity = "Crate"`, hard tabs, `max_width = 100`). Plain `cargo fmt` will fail/warn — always use `cargo +nightly fmt` (or `make fmt`).

## Testing gotchas

- **`rufs` unit tests need no privileges**: `cargo test -p rufs --ignore-rust-version [test_name]`.
- **`fuse-ufs` integration tests require root + FUSE**: they spawn the `fuse-ufs` binary, mount it, and exercise it via syscalls. Needs `/dev/fuse` access and `user_allow_other` in `/etc/fuse.conf`. The harness (`fuse-ufs/tests/integration.rs`) runs under `sudo`/`doas` unless already root; the `SUDO` env var selects which (defaults to `sudo`, `doas` on OpenBSD/musl). CI exports `SUDO=sudo`.
- Golden images live compressed in `resources/*.img.zst`. The harness decompresses them to `CARGO_TARGET_TMPDIR` on demand (re-decompresses if the `.zst` is newer). Read-only tests share the image; `harness_rw` copies it first. Both little- and big-endian images are exercised.
- Rebuild golden images with `scripts/mkimg.sh` (requires FreeBSD `newfs`/`mdconfig`/`setextattr`).
- `make fuz` needs nightly and decompresses both corpus images.

## Architecture essentials

`rufs` layer stack:

```
Ufs<R>                        high-level fs ops (inode, dir, xattr, balloc, ialloc)
  └─ Decoder<BlockReader<R>>  runtime little/big-endian via bincode-next; endian from superblock magic
       └─ BlockReader<R>      block-aligned buffered I/O, single-block write-through cache
            └─ R: Backend      Read + Write + Seek
```

- `BlockReader` panics on `write()` if opened read-only — check `write_enabled()` first. Calling `refill()` on a dirty block also panics.
- `Ufs::open` validates every cylinder-group superblock and `Cgx` magic; failures return `EIO` via the local `sbassert!` macro (`rufs/src/ufs/mod.rs`).
- `transino()` (`fuse-ufs/src/fuse3.rs`, `fuse2.rs`) maps FUSE inode numbers to `InodeNum`: FUSE root `1` (`FUSE_ROOT_ID`) → UFS root `2` (`InodeNum::ROOT`).
- `run()` wraps each FUSE op, turning `IoResult<T>` into a `c_int` errno; it suppresses logging for `ENOATTR`.
- Error-construction macros (defined in `rufs/src/ufs/mod.rs`, re-exported): `err!(EINVAL)` → `IoError` from errno; `iobail!(kind, "..")` → early-return `IoError::new`. (A separate local `err!` exists in `fuse-ufs/src/main.rs`.)
- All on-disk structs in `rufs/src/data.rs` derive `bincode_next::{Decode, Encode}`. **Field order must match on-disk layout exactly.** The file-level `#[allow(dead_code)]` is intentional (constants reserved for future use) — don't remove it to "clean up".

## CI

`.github/workflows/ci.yml` runs: FreeBSD VMs (MSRV + nightly), Linux MSRV, nightly `fmt --check`, and `cargo audit`. Sourcehut builds (`.builds/`) test Alpine (libfuse3) and OpenBSD (libfuse2 + `LIBCLANG_PATH=/usr/local/llvm21/lib`). `make test` must pass under `sudo` on all of these.

## Release flow

`scripts/release` bumps versions in `rufs/Cargo.toml` + `fuse-ufs/Cargo.toml`, patches the workspace `rufs = { version = ... }` line, stamps `ChangeLog.md`, then runs `cargo update` + tests. It uses **`got`** (Game of Trees), not git, and finishes with `cargo publish -p rufs` and `cargo publish -p fuse-ufs`. `scripts/fmt-changelog.sh` rewrites `(#NN)` refs into Markdown PR links — run it after editing `ChangeLog.md`.
