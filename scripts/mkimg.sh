#! /bin/sh -e

# Recreate the golden image used for the integration tests

# UFS has many options that affect on-disk format.  Initially this project will
# only support the most common options, but more may be added later.  See
# newfs(8) for the full list.

# The golden image should be as small as possible while still achieving good
# coverage, so as to minimize the size of data stored in git.

truncate -s 64m resources/ufs.img
MD=$(mdconfig -a -t vnode -f resources/ufs.img)
newfs $MD
MNTDIR=`mktemp -d`
mount -t ufs /dev/"$MD" "$MNTDIR"

# TODO: create files and directories
cd "$MNTDIR"
echo 'This is a simple file.' > file1
mkdir -p dir1/dir2/dir3
echo 'Hello World' > dir1/dir2/dir3/file2
jot $((1 << 16)) 0 | xargs printf '%015x\n' > file3
ln -sf dir1/dir2/dir3/file2 link1
ln -sf "$(yes ./ | head -n200 | tr -d '\n')/file1" long-link
cd -

umount "$MNTDIR"
rmdir "$MNTDIR"
mdconfig -d -u "$MD"

zstd -f resources/ufs.img
