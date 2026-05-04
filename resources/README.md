# Test Images

This directory contains compressed test images used for integration testing.

## Current Images

- `ufs-big.img.zst` - UFSv2 filesystem (big endian)
- `ufs-little.img.zst` - UFSv2 filesystem (little endian)

## Creating UFSv1 Test Images

To create UFSv1 test images (requires FreeBSD):

```sh
# Run on a FreeBSD system or VM
./scripts/mkimg-ufs1.sh
```

This will create `resources/ufs1-{big,little}.img.zst` depending on the host endianness.

### Custom Size

```sh
./scripts/mkimg-ufs1.sh -s 10m
```

## Recreating UFSv2 Images

To recreate the golden UFSv2 images:

```sh
./scripts/mkimg.sh
```

Or with a custom size:

```sh
./scripts/mkimg.sh -s 10m
```

## Notes

- Images are compressed with zstd to minimize repository size
- UFSv1 images are read-only in fuse-ufs
- UFSv2 images have read-write support (experimental)
- Both scripts populate images with test data including:
  - Regular files with various content
  - Directory hierarchies
  - Symbolic links (including long paths)
  - Sparse files
  - Extended attributes (UFSv2 only)
