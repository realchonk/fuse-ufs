#! /bin/sh

# Create UFSv1 test images for integration testing
# UFSv1 is read-only in fuse-ufs, so these images are used to test:
# - UFSv1 from FreeBSD
# - UFSv1 from Solaris/SunOS (different inode format)
# - UFSv1 from NeXTStep/OpenStep
# - UFSv1 from HP-UX
# - Bi-endian support

die() {
    echo "ERROR: $*" >&2
    exit 1
}

# $1: mountpoint
populate() {
    cd "$1" || die "failed to cd into '$1'"

    echo 'UFSv1 test file' > file1
    mkdir -p dir1/dir2
    echo 'Hello from UFSv1' > dir1/dir2/file2
    # Create a small file with numeric data
    jot 1000 0 | xargs printf '%d\n' > numbers.txt
    ln -s dir1/dir2/file2 symlink1
    # Create a longer symlink
    ln -s "$(yes '../' | head -n10 | tr -d '\n')file1" long-symlink
    # Create a sparse file (smaller for UFSv1)
    tr '\0' 'x' < /dev/zero | dd of=sparse bs=4096 seek=100 count=4 2>/dev/null

    cd - > /dev/null || die "failed to cd back"
}

# $1: name
# $2: size
# $@: args to newfs (after size)
create_ufs1() {
    name=$1
    path=resources/${name}.img
    size=$2
    shift 2

    truncate -s "$size" "$path" || die "$path: failed to allocate $size"
    dev=$(mdconfig -a -t vnode -f "$path") || die "$path: failed to create virtual device"

    # -O 1 specifies UFSv1 format
    newfs -O 1 "$@" "/dev/$dev" || die "$path: failed to newfs /dev/$dev"

    mnt=$(mktemp -d) || die "$path: failed to create tempdir"
    mount -t ufs "/dev/$dev" "$mnt" || die "$path: failed to mount '/dev/$dev' onto '$mnt'"

    populate "$mnt"

    # These may fail with only a warning:
    umount "$mnt"
    rmdir "$mnt"
    mdconfig -d -u "$dev"

    zstd -f -19 "$path" || die "$path: failed to compress with zstd"
    rm "$path"
}

# Determine system endianness
case "$(echo I | tr -d '[:space:]' | od -to2 | awk 'NR==1 {print substr($2, 6, 1)}')" in
    0)
        ENDIAN=big
        ;;
    1)
        ENDIAN=little
        ;;
    *)
        die "cannot determine endianness of system"
        ;;
esac

args=$(getopt 's:' "$@") || die "usage: ./scripts/mkimg-ufs1.sh [-s size]"
# shellcheck disable=SC2086
set -- $args

SIZE=4m

while true; do
    case "$1" in
        -s)
            SIZE=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
    esac
done

create_ufs1 "ufs1-${ENDIAN}" "${SIZE}"

echo "Created resources/ufs1-${ENDIAN}.img.zst"
