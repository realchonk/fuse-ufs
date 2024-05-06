use anyhow::{bail, Context, Result};
use std::{ffi::c_int, fs::File, io::{Read, Seek, SeekFrom}, mem::{size_of, transmute_copy}, path::{Path, PathBuf}, thread::sleep, time::Duration};
use fuser::{Filesystem, KernelConfig, Request};

/**
 * UFS2 fast filesystem magic number
 */
const FS_UFS2_MAGIC: i32 = 0x19540119;

/**
 * Location of the superblock on UFS2.
 */
const SBLOCK_UFS2: usize = 65536;

/**
 * Size of a superblock
 */
const SBLOCKSIZE: usize = 8192;

/**
 * `ufs_time_t` on FreeBSD
 */
type UfsTime = i64;

/**
 * `ufs2_daddr_t` on FreeBSD
 */
type UfsDaddr = i64;

/*
 * The path name on which the filesystem is mounted is maintained
 * in fs_fsmnt. MAXMNTLEN defines the amount of space allocated in
 * the super block for this name.
 */
const MAXMNTLEN: usize = 468;

/*
 * The volume name for this filesystem is maintained in fs_volname.
 * MAXVOLLEN defines the length of the buffer allocated.
 */
const MAXVOLLEN: usize = 32;

/*
 * The maximum number of snapshot nodes that can be associated
 * with each filesystem. This limit affects only the number of
 * snapshot files that can be recorded within the superblock so
 * that they can be found when the filesystem is mounted. However,
 * maintaining too many will slow the filesystem performance, so
 * having this limit is a good idea.
 */
const FSMAXSNAP: usize = 20;

/*
 * There is a 128-byte region in the superblock reserved for in-core
 * pointers to summary information. Originally this included an array
 * of pointers to blocks of struct csum; now there are just a few
 * pointers and the remaining space is padded with fs_ocsp[].
 *
 * NOCSPTRS determines the size of this padding. Historically this
 * space was used to store pointers to structures that summaried
 * filesystem usage and layout information. However, these pointers
 * left various kernel pointers in the superblock which made otherwise
 * identical superblocks appear to have differences. So, all the
 * pointers in the superblock were moved to a fs_summary_info structure
 * reducing the superblock to having only a single pointer to this
 * structure. When writing the superblock to disk, this pointer is
 * temporarily NULL'ed out so that the kernel pointer will not appear
 * in the on-disk copy of the superblock.
 */
const NOCSPTRS: usize = (128 / size_of::<usize>()) - 1;

/**
 * Per cylinder group information; summarized in blocks allocated
 * from first cylinder group data blocks.  These blocks have to be
 * read in from fs_csaddr (size fs_cssize) in addition to the
 * super block.
 * `struct csum` in FreeBSD
 */
#[derive(Debug)]
#[allow(dead_code)]
#[repr(C)]
struct Csum {
	ndir: i32,					// number of directories
	nbfree: i32,				// number of free blocks
	nifree: i32,				// number of free inodes
	nffree: i32,				// number of free frags
}

/**
 * `struct csum_total` in FreeBSD
 */
#[derive(Debug)]
#[allow(dead_code)]
#[repr(C)]
struct CsumTotal {
	ndir: i64,					// number of directories
	nbfree: i64,				// number of free blocks
	nifree: i64,				// number of free inodes
	nffree: i64,				// number of free frags
	numclusters: i64,			// number of free clusters
	spare: [i64; 3],			// future expansion
}

/*
 * Super block for an FFS filesystem.
 * `struct fs` in FreeBSD
 */
#[derive(Debug)]
#[allow(dead_code)]
#[repr(C)]
struct Superblock {
	firstfield: i32,			// historic filesystem linked list, 
	unused_1: i32,				// used for incore super blocks 
	sblkno: i32,				// offset of super-block in filesys 
	cblkno: i32,				// offset of cyl-block in filesys 
	iblkno: i32,				// offset of inode-blocks in filesys 
	dblkno: i32,				// offset of first data after cg 
	old_cgoffset: i32,			// cylinder group offset in cylinder 
	old_cgmask: i32,			// used to calc mod fs_ntrak 
	old_time: i32,				// last time written 
	old_size: i32,				// number of blocks in fs 
	old_dsize: i32,				// number of data blocks in fs 
	ncg: u32,					// number of cylinder groups 
	bsize: i32,					// size of basic blocks in fs 
	fsize: i32,					// size of frag blocks in fs 
	frag: i32,					// number of frags in a block in fs 
// these are configuration parameters 
	minfree: i32,				// minimum percentage of free blocks 
	old_rotdelay: i32,			// num of ms for optimal next block 
	old_rps: i32,				// disk revolutions per second 
// these fields can be computed from the others 
	bmask: i32,					// ``blkoff'' calc of blk offsets 
	fmask: i32,					// ``fragoff'' calc of frag offsets 
	bshift: i32,				// ``lblkno'' calc of logical blkno 
	fshift: i32,				// ``numfrags'' calc number of frags 
// these are configuration parameters 
	fs_maxcontig: i32,			// max number of contiguous blks 
		fs_maxbpg: i32,			// max number of blks per cyl group 
// these fields can be computed from the others 
	fragshift: i32,				// block to frag shift 
	fsbtodb: i32,				// fsbtodb and dbtofsb shift constant 
	sbsize: i32,				// actual size of super block 
	spare1: [i32; 2],			// old fs_csmask 
								// old fs_csshift 
	nindir: i32,				// value of NINDIR 
	inopb: u32,					// value of INOPB 
	old_nspf: i32,				// value of NSPF 
// yet another configuration parameter 
	optim: i32,					// optimization preference, see below 
	old_npsect: i32,			// # sectors/track including spares 
	old_interleave: i32,		// hardware sector interleave 
	old_trackskew: i32,			// sector 0 skew, per track 
	id: [i32; 2],				// unique filesystem id 
// sizes determined by number of cylinder groups and their sizes 
	old_csaddr: i32,			// blk addr of cyl grp summary area 
	cssize: i32,				// size of cyl grp summary area 
	cgsize: i32,				// cylinder group size 
	spare2: i32,				// old fs_ntrak 
	old_nsect: i32,				// sectors per track 
	old_spc: i32,				// sectors per cylinder 
	old_ncyl: i32,				// cylinders in filesystem 
	old_cpg: i32,				// cylinders per group 
	ipg: u32,					// inodes per group 
	fpg: i32,					// blocks per group * fs_frag 
// this data must be re-computed after crashes 
	old_cstotal: Csum,			// cylinder summary information 
// these fields are cleared at mount time 
	fmod: i8,					// super block modified flag 
	clean: i8,					// filesystem is clean flag 
	ronly: i8,					// mounted read-only flag 
	old_flags: i8,				// old FS_ flags 
	fsmnt: [u8; MAXMNTLEN],		// name mounted on 
	volname: [u8; MAXVOLLEN],	// volume name 
	swuid: u64,					// system-wide uid 
	pad: i32,					// due to alignment of fs_swuid 
// these fields retain the current block allocation info 
	cgrotor: i32,				// last cg searched 
	ocsp: [usize; NOCSPTRS],	// padding; was list of fs_cs buffers 
	si: usize,					// In-core pointer to summary info 
	old_cpc: i32,				// cyl per cycle in postbl 
	maxbsize: i32,				// maximum blocking factor permitted 
	unrefs: i64,				// number of unreferenced inodes 
	providersize: i64,			// size of underlying GEOM provider 
	metaspace: i64,				// size of area reserved for metadata 
	sparecon64: [i64; 13],		// old rotation block list head 
	sblockactualloc: i64,		// byte offset of this superblock 
	sblockloc: i64,				// byte offset of standard superblock 
	cstotal: CsumTotal,			// (u) cylinder summary information 
	time: UfsTime,				// last time written 
	size: i64,					// number of blocks in fs 
	dsize: i64,					// number of data blocks in fs 
	csaddr: UfsDaddr,			// blk addr of cyl grp summary area 
	pendingblocks: i64,			// (u) blocks being freed 
	pendinginodes: u32,			// (u) inodes being freed 
	snapinum: [u32; FSMAXSNAP],	// list of snapshot inode numbers 
	avgfilesize: u32,			// expected average file size 
	avgfpdir: u32,				// expected # of files per directory 
	save_cgsize: i32,			// save real cg size to use fs_bsize 
	mtime: UfsTime,				// Last mount or fsck time. 
	sujfree: i32,				// SUJ free list 
	sparecon32: [i32; 21],		// reserved for future constants 
	ckhash: u32,				// if CK_SUPERBLOCK, its check-hash 
	metackhash: u32,			// metadata check-hash, see CK_ below 
	flags: i32,					// see FS_ flags below 
	contigsumsize: i32,			// size of cluster summary array  
	maxsymlinklen: i32,			// max length of an internal symlink 
	old_inodefmt: i32,			// format of on-disk inodes 
	maxfilesize: u64,			// maximum representable file size 
	qbmask: i64,				// ~fs_bmask for use with 64-bit size 
	qfmask: i64,				// ~fs_fmask for use with 64-bit size 
	state: i32,					// validate fs_clean field 
	old_postblformat: i32,		// format of positional layout tables 
	old_nrpos: i32,				// number of rotational positions 
	spare5: [i32; 2],			// old fs_postbloff 
								// old fs_rotbloff 
	magic: i32,					// magic number 
}

pub struct Ufs {
	file: File,
	superblock: Superblock,
}

impl Ufs {
	pub fn open(path: PathBuf) -> Result<Self> {
		println!("sizeof(Superblock) = {}", size_of::<Superblock>());
		let mut file = File::options()
			.read(true)
			.write(false)
			.open(path)
			.context("failed to open device")?;
		let mut block = [0u8; SBLOCKSIZE];
		file.seek(SeekFrom::Start(SBLOCK_UFS2 as u64))
			.context("failed to seek to superblock")?;
		file.read_exact(&mut block)
			.context("failed to read superblock")?;
		let superblock: Superblock = unsafe { transmute_copy(&block) };
		if superblock.magic != FS_UFS2_MAGIC {
			bail!("invalid superblock magic number: {}", superblock.magic);
		}
		Ok(Self {
			file,
			superblock,
		})
	}
}

impl Filesystem for Ufs {
	fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
		println!("init()");
		println!("Superblock: {:#?}", self.superblock);
		Ok(())
	}
	fn destroy(&mut self) {
		println!("destroy()");
	}
}

fn main() -> Result<()> {
	let fs = Ufs::open(PathBuf::from("/dev/da1"))?;
	let mp = Path::new("mp");
	let options = &[];

	let mount = fuser::spawn_mount2(fs, mp, options)?;
	sleep(Duration::new(1, 0));
	drop(mount);

	Ok(())
}
