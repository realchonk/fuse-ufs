# Mining FreeBSD UFS2: what was taken, and what was left behind

**FreeBSD's `sys/ufs/ffs/`, `sys/ufs/ufs/`, `sbin/fsck_ffs/` and `sbin/newfs/`
were used as a behavioural and architectural specification.**  No FreeBSD code
was copied.  Every Rust implementation in this repository was written from
scratch, with different types, different control flow, and its own
documentation; where the algorithms are the same, the comments explain *why* the
algorithm is the same, and where they differ the difference is called out.

FreeBSD's `sys/ufs` is BSD-3-Clause and its userland is BSD-2-Clause, so
referring to it is unproblematic in principle.  This repository is BSD-2-Clause
and new code here follows that.

---

## 1. Cylinder-group layout

FreeBSD `sys/ufs/ffs/fs.h`:

```c
#define	cgbase(fs, c)	(((ufs2_daddr_t)(fs)->fs_fpg) * (c))
#define	cgdmin(fs, c)	(cgstart(fs, c) + (fs)->fs_dblkno)
#define	cgimin(fs, c)	(cgstart(fs, c) + (fs)->fs_iblkno)
#define	cgdata(fs, c)	(cgdmin(fs, c) + (fs)->fs_metaspace)
#define	cgmeta(fs, c)	(cgdmin(fs, c))
```

and `sys/ufs/ffs/ffs_subr.c` for `blknum()`.

Implemented as `Superblock::cg_start` / `cg_inode_start` / `cg_inode_end` /
`cg_meta_start` / `cg_data_start` / `cg_end` in `rufs/src/geom.rs`.

**Divergences**

* FreeBSD's `cgstart()` has a UFS1 branch using `fs_old_cgoffset` and
  `fs_old_cgmask`.  This driver supports UFS2 only, so `cg_start(cg) == cg *
  fs_fpg` unconditionally.  Keeping the UFS1 arithmetic would mean an
  indirection that can only ever be wrong on the formats this driver rejects.
* FreeBSD's `blkroundup()` and `fragroundup()` macros are
  `(size + fs_qbmask) & fs_bmask` with `fs_qbmask == ~fs_bmask`, which evaluates
  to `size % fs_fsize`.  Despite their names and comments they are
  inclusive-end helpers used on pre-biased arguments, not rounding operations.
  This driver provides a true round-up under those names and does not
  reproduce the FreeBSD form; see the source comments in `geom.rs`.
* FreeBSD's `ino_to_fsba()` multiplies the inode-block index by `fs_frag`
  (`blkstofrags()`), because `fs_iblkno` is a *fragment* offset.  The
  transcription in `data.rs` had dropped that wrapper; it was harmless only
  because `ino_to_fso()` took a different route to the same byte offset.  The
  new code has one path and is correct on its own terms.

## 2. Metadata area

`fs_metaspace` blocks at the head of the data zone are reserved for metadata.
In UFS2 this is where later (second and third level) indirect blocks go, not
the area between the inode blocks and the first data block.

Rationale, as mined: by the time a file needs a double-indirect block its
`cgmeta()` region is mostly consumed and its data has spilled into other
cylinder groups; the blocks must still be findable without reading every
cylinder-group header.  The first indirect block is *not* in the metadata area
— see below.

Implemented as `BlockRole::Indirect { first }` in `rufs/src/policy.rs`.

## 3. Block placement: `ffs_blkpref_ufs2()`

`sys/ufs/ffs/ffs_alloc.c` divides a file into sections (the `UFS_NDADDR` direct
blocks, the `fs_nindir` blocks behind the first indirect, and one section per
further indirect level) and has three distinct behaviours:

1. **indirect blocks** — prefer `cgmeta()` of the inode's cylinder group, except
   that the *first* indirect block prefers to sit immediately after the last
   direct block, so that a file which just outgrew its 12 direct slots keeps one
   contiguous run;
2. **the first block behind the first indirect** — prefer the fragment after
   `indirect[0]`, but only if that indirect block was itself allocated in the
   *data* area *of the same cylinder group*, because otherwise "the next
   fragment" is meaningless;
3. **everything else** — prefer the fragment after the previous logical block,
   unless a new section has started (`lbn % fs_maxbpg == 0`) or the previous
   block is a hole; then directory data and extattr blocks go to `cgmeta()`,
   sections 0 and 1 go to `cgdata()` of the inode's cylinder group, and later
   sections search forward from `cgstart(ino_cg + lbn / fs_maxbpg)` for a
   cylinder group with at least the average number of free blocks, wrapping, and
   give up (return 0) if there is none.

Implemented as `pref_block()` in `rufs/src/policy.rs`, as a pure function that
cannot fail and cannot touch a bitmap.

**Divergence — the allocator floor.**  `ffs_alloccgblk()` falls back to
`cgbase + cg_rotor + fs_frag` when the preference is 0.  In the *last* cylinder
group of a `newfs`-created image the first few boot fragments are left marked
free in the bitmap and `cg_rotor` starts at zero, so that start point is inside
the boot area.  Following FreeBSD literally would allocate a boot fragment and
destroy the ability to boot the filesystem.  This driver floors the scan at
`cgmeta()`.  This is the only place where the driver is *safer* than the
reference rather than merely different.

**Divergence — the map search.**  `ffs_mapsearch()` scans map bytes for the
`fs_frag`-bit pattern of a free run and cannot find a run that straddles two
bytes.  `BlkMap::find()` walks fragments and can.  This is a strict improvement
and is exercised by `balloc::t::frag_run_may_start_mid_byte`.

**Divergence — bitmap packing.**  FreeBSD's `blkmap()` reads
`map[loc / NBBY] >> (loc % NBBY)`, i.e. eight fragment bits per byte
regardless of `fs_frag`.  The transcription in this crate packed `fs_frag` bits
per byte, which needs a map twice as long for `fs_frag == 4` and eight times as
long for `fs_frag == 1` — i.e. it ran off the end of the space `newfs`
reserved.  The golden images use `fs_frag == 8`, so the bug was invisible; it is
now covered by `balloc::t::map_packs_eight_fragments_per_byte`.

## 4. Directory placement: `ffs_dirpref()`

FreeBSD calls the resulting changes **dirpref changes**, after the function
they live in, and that is the term used here as well.  `sbin/newfs` credits
Grigoriy Orlov, who designed the algorithm, and carries his comment:
`AVFILESIZ` (16384) and `AFPDIR` (64) are the tunables, and `fs_avgfilesize` /
`fs_avgfpdir` on disk are what `tunefs -a`/`tunefs -f` adjust.

The algorithm, as mined:

* search range `ncg >> i_dirdepth` cylinder groups centred on the parent's;
* the preferred point inside that range comes from the bit-reversal sequence
  `1/2, 1/4, 3/4, 1/8, 3/8, 5/8, 7/8, …`, indexed by `pip->i_effnlink - 1`;
* acceptance limits, all derived from filesystem-wide averages:
  `ndir < avgndir + 2^depth`, `nifree >= avgifree - avgifree/4`,
  `nbfree >= avgbfree - avgbfree/4`, and `contigdirs < maxcontigdirs`, where
  `maxcontigdirs` estimates how many directories' worth of data still fits in an
  average cylinder group given how many directories it already holds, capped by
  `ipg / avgfpdir`;
* the search runs forward from the preferred cylinder group to the end and then
  wraps to the start — forward first, so a nearly-full filesystem does not
  rescan the full cylinder groups nearest the preference on every request;
* fallbacks: "any cylinder group with at least the average number of free
  inodes", then cylinder group 0.  Placing a directory somewhere imperfect
  beats failing `mkdir` with `ENOSPC`.

Implemented as `pref_inode()` in `rufs/src/policy.rs`.

**`fs_contigdirs` is runtime-only.**  FreeBSD puts it in a `struct
fs_summary_info` reached through the `fs_si` pointer, alongside `si_csp`,
`si_maxcluster` and `si_active` — i.e. explicitly *not* in the serialised
superblock, which is why `struct fs` still has kernel pointers in `fs_ocsp[]`
that are NULLed out on write.  This driver keeps it in
`crate::geom::AllocationSummary`, which is never encoded.  The on-disk fields
that *are* maintained are `cg_rotor`, `cg_frotor` and `cg_irotor`, because they
are part of `struct cg`.

**Divergences**

* FreeBSD indexes the bit-reversal sequence by the in-core `i_effnlink`; this
  driver uses the on-disk `i_nlink`.  For a directory `i_effnlink` is derived
  from `i_nlink`; the two differ only for the unlinked snapshots
  `ufs_backgroundinode()` creates, which this driver does not implement.
* FreeBSD computes `range = ncg >> depth` with a C shift; this driver saturates
  at 63, so a corruptly deep `i_dirdepth` cannot produce a shift overflow.
  (`rufs::policy::t::absurd_depth_does_not_panic`.)
* FreeBSD's `blksize()` gives the tail block of a small file a size of
  `fragroundup(fs, blkoff(fs, i_size))`, which is *zero* for a file smaller than
  one block.  This driver allocates and frees whole blocks only, so the tail
  block of a file wastes up to `fs_bsize - 1` bytes.  See
  `docs/ufs2-invariants.md` for why `i_blocks` is consequently not cross-checked.

## 5. Directory depth

`i_dirdepth` lives in the on-disk `di_ignored` word, shared with the
soft-update journal's "next unlinked inode" pointer.  FreeBSD sets it in
`ffs_makeinode()` (`ip->i_dirdepth = pip->i_dirdepth + 1`) and `ffs_adjust_depth`
(`FFS_ADJ_DEPTH`) adjusts existing subtrees.

Implemented as `Inode::dir_depth()` / `Inode::set_dir_depth()` and maintained in
`Ufs::inode_alloc`.  `dir_depth()` returns `Option<u32>` so a non-directory can
never expose the journal word, and `set_dir_depth()` panics on a non-directory
because there is no correct value to write there.

## 6. Allocation policy versus allocation implementation

FreeBSD separates them by function: `ffs_blkpref_ufs2()` and `ffs_dirpref()`
return a hint and touch nothing but in-core counters; `ffs_alloc()`,
`ffs_hashalloc()`, `ffs_alloccgblk()` and `ffs_nodealloccg()` mutate the
bitmaps.

This driver makes the boundary a *type* boundary.  `rufs/src/policy.rs` is a
pure function of the superblock, a snapshot of the per-cylinder-group counters
(`CgSums`), the runtime hints (`AllocationSummary`) and a description of the
request.  It cannot fail, cannot report "no space", and has no way to modify an
allocation bitmap.  `rufs/src/ufs/balloc.rs` is the only place that mutates a
bitmap, and it takes a `BlockPref` it may or may not be able to satisfy.

The payoff is testability: the interesting behaviour of both `ffs_blkpref_ufs2`
and `ffs_dirpref` lives in their *fallback* ladders, and a fallback ladder that
has to be exercised against a real image is a fallback ladder that is not
exercised at all.  All 24 policy tests are pure and run in under a millisecond.

## 7. Cylinder-overflow search

`ffs_hashalloc()`'s three steps — preferred cylinder group, quadratic rehash
(`+1, +2, +4, +8, …`), brute force from `preferred + 2` — are reproduced in
`hash_alloc_block()` and `hash_alloc_inode()`.  The order is not an
optimisation: on a nearly-full filesystem the cylinder groups nearest the
preference are the most likely to be full as well, and a nearby-first search
would rescan the same dead cylinder groups on every allocation.  The order is
pinned by `balloc::t::hashalloc_visits_cgs_in_documented_order`.

`ffs_nodealloccg()`'s order is also reproduced: the requested offset if free,
then a forward sweep from `cg_irotor`, then a wrap.

## 8. `fsck_ffs` as a specification

The five passes are the specification of what a UFS2 filesystem may look like.
`rufs/src/ufs/fsck.rs` re-derives the checks that allocation and ordering bugs
can violate, and is used as the oracle for every mutating test.

| pass | `fsck_ffs` | `check_consistency` |
|---|---|---|
| 1 | block ownership, inode block sizes, direct/indirect structure | yes, walking the block map structurally |
| 2 | directory format, `.`/`..`, entry targets | yes, plus `i_dirdepth` against `..` |
| 3 | connectivity to the root | yes |
| 4 | link counts, unreferenced inodes | yes |
| 5 | rebuild every bitmap and summary | yes |

Two things the checker learned the hard way, both of which are properties of the
*format* rather than of this driver:

* **Walk block maps, not `i_size`.**  UFS2 supports sparse files.  The golden
  images contain files created with `dd seek=`, one of them 550 GB long in a
  4 MiB filesystem.  `fsck_ffs` pass 1 walks the block map for the same reason
  `fsck_ffs` pass 1 is fast on a sparse filesystem.
* **Fragment granularity.**  A block is allocated if all `fs_frag` of its
  fragments are free, but a pointer may name a single fragment, so "is this
  block allocated" has to be asked per fragment.

The checker is not a substitute for `fsck_ffs`: no cluster accounting, extended
attributes, snapshots, quotas, ACLs or `ckhash` validation, and no repair.

---

## See also

* [`docs/ufs2-invariants.md`](ufs2-invariants.md) — the invariants and who keeps
  them.
* [`docs/soft-updates.md`](soft-updates.md) — the dependency architecture this
  mining is preparing for, and which FreeBSD concepts it comes from.