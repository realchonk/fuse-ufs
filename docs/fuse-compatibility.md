# FUSE compatibility: block mapping and seeks

This file records what the driver answers about a file's physical mapping, on
which platforms, and what the answers mean.  The implementation lives in two
places that this file keeps apart on purpose:

```
  FUSE BMAP / LSEEK                        fuse-ufs/src/fuse3.rs   FUSE ABI
              |
  UFS block runs and seeks                 rufs/src/ufs/runs.rs
                                           rufs/src/ufs/mapping.rs
              |
  live inode + indirect blocks            rufs/src/ufs/meta.rs    BufferCache
```

The rule the arrangement exists to keep: **FUSE compatibility is an adapter
around UFS semantics, not a second filesystem implementation.**  No Linux
constant, struct or flag appears in `rufs`, and no pointer arithmetic appears in
`fuse-ufs`.

## Matrix

| operation | Linux (`fuse3`) | FreeBSD (`fuse3`) | OpenBSD (`fuse2`) | notes |
|---|---|---|---|---|
| `bmap` | yes | yes | no | Linux only asks on a `blkdev` mount |
| `lseek(SEEK_DATA)` | yes | yes | no | |
| `lseek(SEEK_HOLE)` | yes | yes | no | |
| `FS_IOC_FIEMAP` | no | no | no | not implemented; see below |

`fuse2` (OpenBSD) is limited by the protocol itself: FUSE 2 has no `LSEEK`
message, and libfuse2's high-level interface has no `bmap`.

## `bmap`

The kernel sends a file-relative block index in **512-byte sectors** together
with the superblock's block size, and expects a sector on the underlying device.
A UFS block number is a *fragment* address, so:

```
  file offset = idx * 512
  device offset = ufs_block * fs_fsize
  reply = device offset / 512
```

The `blocksize` argument does not enter the computation.  Dividing by
`fs_bsize` anywhere in that chain is a factor-of-`fs_frag` error that still
produces a plausible-looking number, which is why the conversion is spelled out
in the Rustdoc of `Fs::bmap`.

A hole — and a block past the end of the file — replies `0`, the protocol's only
way to say "not mapped".  That is unambiguous here because UFS never allocates
block 0.

## `lseek(SEEK_DATA)` / `lseek(SEEK_HOLE)`

| situation | `SEEK_DATA` | `SEEK_HOLE` |
|---|---|---|
| offset inside an allocated run | the offset itself | the next hole |
| offset inside a hole | the start of the next run | the offset itself |
| offset at or past `i_size` | `ENXIO` | `i_size` |
| file with no data at or after the offset | `ENXIO` | the first hole, or `i_size` |
| any other `whence` | `EINVAL` | `EINVAL` |

Asking from inside a run returning the offset is what `lseek(SEEK_DATA)` means
by "the next location at or after the specified offset where data has been
written".  The asymmetry at the end of the file is deliberate: there is always a
hole at or after an offset inside a file, so `SEEK_HOLE` has an answer where
`SEEK_DATA` does not.

A block of zeroes that the inode points at is **data**, never a hole.  Only a
missing pointer is a hole.

### A kernel wart worth knowing

The FUSE lseek reply path in Linux has historically turned any error other than
`ENOSYS` into `EIO` before the caller sees it.  So a mount may report `EIO`
where the protocol specifies `ENXIO`.  The driver replies with the documented
code anyway: that is what the protocol asks for and what a kernel that preserves
it needs.

## Not implemented

`FS_IOC_FIEMAP` is **not** implemented, and neither is a general FUSE `ioctl`
handler: there is no `ioctl` callback in `Fs` at all, so any ioctl a caller
issues gets `ENOTTY`.

`BlockRun` is nevertheless the right internal shape for it.  The runs carry
everything a FIEMAP reply needs — logical offset, physical block, length, and
whether the run reaches the end of the file — so converting them to
`struct fiemap` later would be a self-contained adapter rather than a redesign.
Two things that conversion will have to get right, recorded here so they are not
rediscovered:

* `fe_physical` is a **byte offset**, while `BlockRun::physical` is a UFS block,
  so it needs multiplying by `fs_fsize`.  UFS has no extents, so there is no
  on-disk structure a FIEMAP extent could have come from: a run of *n*
  consecutive blocks is *n* pointers in `di_ext[]`/`di_extb[]`.
* Most of `FIEMAP_EXTENT_*` describe Linux delayed-allocation machinery this
  filesystem does not have.  `DELALLOC` and `UNWRITTEN` are the two that would be
  wrong to claim: Soft Updates' pending publication is an *ordering* constraint
  on an allocation that already happened, which is not the same thing as a block
  that has not been allocated yet.

## Cost, and what the queries observe

Both operations answer from the **live** metadata.  A file whose fourth
block was allocated a moment ago and whose inode has not been written back maps
that block, and asking does not force it out.

`SEEK_DATA` and `SEEK_HOLE` walk the pointer tree rather than the logical block
indices, so a zero pointer at single-indirect level ends the walk in one
comparison instead of 4096 reads.  A 1000-block hole behind one indirect block
is a single step; `SEEK_DATA` on it does not depend on how large the hole is.
The `a_whole_indirect_block_hole_is_one_run` test in `rufs/src/ufs/runs.rs`
pins that down.

Asking costs one metadata-block read per indirect block on the path, plus the
in-memory walk.  It does no writes and takes no locks beyond what `Ufs` already
holds.

## Tests

| level | where |
|---|---|
| single-block mapping | `rufs/src/ufs/mapping.rs` — `BlockMapping`, holes, the fragment tail, both byte orders, EOF, overflow |
| block-run traversal | `rufs/src/ufs/runs.rs` — data/hole/data, physical breaks, a whole indirect-block hole, resumption |
| `SEEK_DATA`/`SEEK_HOLE` | `rufs/src/ufs/mapping.rs` (`mod seek`) — every case in the table above, plus an oracle that cross-checks both against the single-offset mapping at every block boundary |
| FUSE ABI | `fuse-ufs/tests/integration.rs` — requires root and `/dev/fuse` |