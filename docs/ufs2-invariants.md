# UFS2 filesystem invariants

Every invariant this driver maintains, why it exists, who checks it, and which
part of the code is responsible for keeping it.  The list follows FreeBSD's
`sbin/fsck_ffs`: pass numbers refer to the pass in which `fsck_ffs` would
report a violation.

`Ufs::check_consistency()` (in `rufs/src/ufs/fsck.rs`) implements checks for
all of these and is used as the oracle by every mutating unit test.  It is a
read-only checker: it reports, it never repairs.

Throughout, "the golden images" means `resources/ufs-{little,big}.img.zst`,
produced by FreeBSD's `newfs(8)` and `scripts/mkimg.sh`.

---

## 1. Allocated inode ⇔ inode bitmap

*fsck pass 5*

An inode is allocated if and only if `cg_iused[]` says so.

FreeBSD allocates the tail block of a small file as a run of *fragments*, so
this invariant must be checked at fragment granularity too (see
`docs/freebsd-ufs2.md`).

*Kept by* `Ufs::alloc_cg_inode`, `Ufs::free_cg_inode`
(`rufs/src/ufs/balloc.rs`).  Both go through `read_inomap`/`write_inomap`, and
`free_cg_inode` refuses to clear a bit that is already clear.

*Checked by* `check_consistency` pass 5 (rebuilds the map) and pass 1 (an inode
marked allocated but with no file type is reported, because that is the
signature of a crash between clearing the bitmap bit and writing the inode).

*Deliberate exception:* UFS2 reserves inode 1 as `lost+found` and a `newfs`-ed
filesystem leaves it marked allocated with no file type until `fsck` or `mount`
populates it.  The checker skips inode 1 for the same reason `fsck_ffs` treats
`LOSTFOUNDINO` specially.

## 2. Referenced block ⇔ block bitmap

*fsck pass 1 and pass 5*

Every block reachable from an allocated inode has its bitmap bits clear, and
every block whose bitmap bits are clear is reachable from an allocated inode.

*Kept by* `Ufs::blk_alloc_for`, `Ufs::blk_free`; every allocation and free
updates the map, `cg_cs.cs_nbfree` and `fs_cstotal.cs_nbfree` in the same
operation.

*Checked by* `check_consistency` pass 5 (rebuilds the map) and pass 1
(reports `lbn N points at block B, which the bitmap reports as free`).

## 3. No live inode references a free block

*fsck pass 1*

The strongest form of (2) for data blocks: it is what makes a lost file
recoverable and what `fsck` phase 1 reports as a block that is simultaneously
allocated and owned by an inode.

*Kept by* the allocator: a block is reserved (bitmap cleared) before its
contents are written and before any pointer to it can become persistent.

*Checked by* `check_consistency` pass 1.

## 4. No two incompatible owners reference the same block

*fsck pass 1*

A block has exactly one owner: one inode's direct slot, one inode's indirect
entry, or nothing.  Two inodes claiming the same block means one of them will
lose data as soon as the other writes.

*Kept by* `BlkMap`: a block is handed out only when all `fs_frag` of its
fragments are free, and marking it allocated clears all of them, so no second
allocation can overlap.

*Checked by* `check_consistency` pass 1 (`owners` is keyed by block; anything
with more than one owner is reported).

## 5. Directory entries reference initialised inodes

*fsck pass 2*

Every non-`.`/`..` entry in a directory names an inode whose bitmap bit is set
and whose `mode` has a non-zero file type.  A directory entry pointing at an
inode that has never been written is the crash signature of "the entry became
persistent before the inode did".

*Kept by* `Ufs::mknod`/`Ufs::mkdir`: `inode_alloc()` calls `inode_setup()`
(which writes the inode, and refuses to reuse an inode whose `nlink` is non-zero)
before `dir_newlink()` makes the name reachable.

*Checked by* `check_consistency` pass 2.

## 6. Link counts agree with directory references

*fsck pass 4*

* A directory's `i_nlink` is `2 + (number of entries in it that name a
  directory)`.  The two are `.` and the implied `..`; a child's `..` is *not* an
  additional link, because it is exactly the link the parent's entry already
  accounts for.
* A non-directory's `i_nlink` is its hard-link count, i.e. the number of
  directory entries that name it.

*Kept by* `Ufs::inode_bump`, `Ufs::inode_free`, `Ufs::dir_newlink`,
`Ufs::ino_try_unlink`, `Ufs::mkdir`, `Ufs::rmdir`.

*Checked by* `check_consistency` pass 4, which rebuilds the counts by walking
the tree from the root.

## 7. Indirect block trees agree with inode size

*fsck pass 1*

The highest logical block index with a non-zero pointer must not exceed the
number of blocks the inode's size requires, and the indirect blocks reachable
from the inode must themselves be allocated.

*Kept by* `Ufs::inode_set_block`: an indirect block is allocated (and zeroed)
before the entry that points at it is written.

*Checked by* `check_consistency` pass 1, walking the block map structurally
rather than by `i_size` — see `docs/freebsd-ufs2.md` for why that distinction
is not cosmetic.

## 8. Cylinder-group summaries agree with their bitmaps

*fsck pass 5*

`cg_cs.cs_nbfree` is the number of fully free blocks in `cg_blksfree[]`, and
`cg_cs.cs_nifree` is the number of clear bits in `cg_iused[]`.

A `cg_cs` of `{-1, -1, -1, -1}` is UFS2's "this cylinder group has never been
initialised" sentinel and is exempt.

*Kept by* `Ufs::write_cg`, the single choke point every cylinder-group mutation
goes through.

*Checked by* `check_consistency` pass 5.

`cg_cs.cs_nffree` and `cg_frsum[]` are *not* maintained or checked: this
implementation allocates whole blocks only (see the module documentation in
`rufs/src/ufs/balloc.rs`), so they stay zero, which is self-consistent and what
`fsck_ffs` pass 5 expects.

## 9. Global summaries agree with cylinder-group summaries

*fsck pass 5*

`fs_cstotal.cs_nbfree`, `.cs_nifree` and `.cs_ndir` are the sums of the
per-cylinder-group `cg_cs` values (and, for `cs_ndir`, the number of allocated
directory inodes).

*Kept by* `Ufs::update_sb` in the same operation that adjusts `cg_cs`.

*Checked by* `check_consistency` pass 5.

## 10. Directory depth agrees with parent relationships

*UFS2 specific; not a classic `fsck_ffs` check*

A directory's `i_dirdepth` (the on-disk `di_ignored` word) must be exactly its
parent's depth plus one, and the root's must be 0.

The field exists because the dirpref placement policy needs it: the search
for a new directory halves with depth.  An image created by a driver that never
wrote it reads as depth 0 for every directory, which the policy treats as
"untracked" and answers with the parent's cylinder group — a degradation, not a
corruption.

*Kept by* `Ufs::inode_alloc`, via `Inode::set_dir_depth`.

*Checked by* `check_consistency`, once for the root and once per directory
against its `..` entry.

*Note:* the word is shared with the soft-update journal's "next unlinked inode"
pointer.  `Inode::dir_depth()` returns `None` for non-directories rather than
exposing the journal field, and `set_dir_depth()` panics on a non-directory
because there is no correct value to write there.

## 11. Metadata and data placement follow the UFS2 allocation policy

*soft*; not checked by `fsck_ffs`*

| block kind | zone |
|---|---|
| ordinary file data | data zone (`cgdata()..cgend()`) |
| directory data | metadata zone (`cgmeta()..cgdata()`) |
| indirect blocks, except the first | metadata zone |
| the first indirect block | immediately after the last direct block |
| the first data block behind the first indirect block | immediately after that indirect block |
| extended-attribute blocks | metadata zone |

The metadata zone is a *preference*, not a fence: `fs_metaspace` reserves only a
few blocks per cylinder group and UFS2 spills metadata into the data zone when
it runs out.  What is never acceptable is a block below `cgmeta()`, which holds
the disk label, the boot blocks, the backup superblock, the cylinder-group
struct and the inode blocks.

New *directories* are placed by the dirpref scheme (spread when shallow,
clustered when deep), new *files* follow their parent directory.

*Kept by* `rufs/src/policy.rs` and `rufs/src/ufs/balloc.rs`.

*Checked by* `ufs::alloctest` (`new_directory_uses_metadata_zone`,
`new_file_uses_data_zone_and_is_contiguous`,
`directory_growth_stays_in_its_cylinder_group`, `directories_are_spread_across_cgs`,
`files_follow_their_directory`, `allocation_overflows_to_another_cg`) and, for
the reserved area, by `alloc_cg_block`'s floor.

---

## Invariants that are *not* enforced

| invariant | status |
|---|---|
| `i_blocks` equals the space actually held | not checked.  FreeBSD's accounting for fragment-run tail blocks and for a new directory's first block is subtle (see `docs/freebsd-ufs2.md`), and a subtly wrong check turns every unrelated test red.  Tracked as a follow-up together with fragment allocation. |
| `ckhash` / `metackhash` | not implemented.  The fields are read and written verbatim; they are not computed or validated. |
| `fs_pendingblocks`, `fs_pendinginodes` | not used.  They exist for Soft Updates recovery. |
| snapshot consistency | not checked and not supported.  The golden images contain a `.snap` directory; `newfs` also leaves inode 1 allocated-but-empty. |
| the superblock mirror copies at `SBLOCK_UFS2` and `SBLOCK_UFS2 + SBLOCKSIZE` | only the first copy is written (`update_sb`).  Pre-existing. |

## Adding an invariant

Add the check to `Ufs::check_consistency()` with the pass number it belongs to,
name the operation that maintains it, and add a unit test that breaks the
invariant and asserts that the checker reports it.  A checker that cannot fail
is worse than no checker, because it is believed.