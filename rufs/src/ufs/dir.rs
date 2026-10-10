use std::io::{BufRead, Write};

use super::*;
use crate::{err, InodeNum};

/// The size of `direntry::di_ino`, the inode number a directory entry names.
///
/// `direntry` is `{ u_int32_t di_ino; u_int16_t di_reclen; u_int8_t di_type;
/// u_int8_t di_namlen; char di_name[]; }`, so the number is 4 bytes even though
/// UFS block addresses are `ufs2_daddr_t` and therefore 8.  Gating eight bytes
/// here would also zero `di_reclen`, and the rest of the record would then be
/// unparseable -- a crash would turn a half-written entry into a directory that
/// `fsck` cannot walk past.
const DIRENT_INR_LEN: u64 = 4;

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
	/// The inode this entry names.
	pub inr:    InodeNum,
	/// Its type, as recorded in `direntry::di_type`.
	pub kind:   InodeType,
	/// Its name, owned: `dir_iter` lends names out of a block it reuses.
	pub name:   std::ffi::OsString,
	/// This entry's position in the listing, counting from 1.
	///
	/// Not an arbitrary cursor: it counts `.` and `..`, and it is stable for a
	/// given directory, which is what lets a caller that filled one reply buffer
	/// ask for the rest without having to re-derive where it stopped.  Assigning
	/// it while enumerating, rather than after, is what keeps it from shifting
	/// under a continuation.
	pub offset: i64,
}

#[derive(Debug, Clone, Copy)]
struct Header {
	inr:     InodeNum,
	reclen:  u16,
	kind:    Option<InodeType>,
	namelen: u8,
	name:    [u8; UFS_MAXNAMELEN + 1],
}

impl Header {
	fn new(inr: InodeNum, kind: InodeType, dname: &OsStr) -> Self {
		assert!(dname.len() <= UFS_MAXNAMELEN);
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		name[0..dname.len()].copy_from_slice(dname.as_bytes());
		// Sized from `dname`, not from `name`: the buffer is always
		// `UFS_MAXNAMELEN + 1` bytes whatever the entry is called, so using it
		// here gave every record the same oversized reclen.
		let reclen = ((4 + 2 + 1 + 1 + dname.len() + 3) & !3) as u16;
		Self {
			inr,
			reclen,
			kind: Some(kind),
			name,
			namelen: dname.len() as u8,
		}
	}

	fn parse<T: BufRead + Seek>(file: &mut Decoder<T>) -> IoResult<Option<Header>> {
		let inr: InodeNum = file.decode()?;
		let reclen: u16 = file.decode()?;
		if reclen == 0 {
			return Ok(None);
		}
		let kind: u8 = file.decode()?;
		let namelen: u8 = file.decode()?;
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		file.read(&mut name[0..namelen.into()])?;

		// skip remaining bytes of record, if any
		let off = reclen - (namelen as u16) - 8;
		file.seek_relative(off as i64)?;

		if inr.get() == 0 {
			return Ok(None);
		}

		log::trace!("Header::read(): {{ inr={inr}, reclen={reclen}, namelen={namelen}, name={:?}, kind={kind} }}",
					unsafe { OsStr::from_encoded_bytes_unchecked(&name[0..namelen.into()]) });

		let kind = match kind {
			DT_FIFO => Some(InodeType::NamedPipe),
			DT_CHR => Some(InodeType::CharDevice),
			DT_DIR => Some(InodeType::Directory),
			DT_BLK => Some(InodeType::BlockDevice),
			DT_REG => Some(InodeType::RegularFile),
			DT_LNK => Some(InodeType::Symlink),
			DT_SOCK => Some(InodeType::Socket),
			DT_WHT => None,
			DT_UNKNOWN => todo!("DT_UNKNOWN: {inr}"),
			_ => panic!("invalid filetype: {kind}"),
		};

		Ok(Some(Self {
			inr,
			reclen,
			kind,
			namelen,
			name,
		}))
	}

	fn write<T: Read + Write + Seek>(&self, file: &mut Decoder<T>) -> IoResult<()> {
		let kind: u8 = match self.kind {
			Some(InodeType::NamedPipe) => DT_FIFO,
			Some(InodeType::CharDevice) => DT_CHR,
			Some(InodeType::Directory) => DT_DIR,
			Some(InodeType::BlockDevice) => DT_BLK,
			Some(InodeType::RegularFile) => DT_REG,
			Some(InodeType::Symlink) => DT_LNK,
			Some(InodeType::Socket) => DT_SOCK,
			None => DT_WHT,
		};
		log::trace!(
			"Header::write(inr={}, reclen={}, namelen={}, name={:?}, kind={kind})",
			self.inr,
			self.reclen,
			self.namelen,
			self.name()
		);
		file.encode(&self.inr)?;
		file.encode(&self.reclen)?;
		file.encode(&kind)?;
		file.encode(&self.namelen)?;
		file.write(&self.name[0..self.namelen.into()])?;

		// fill the rest of the record with zeros
		file.fill(0u8, (self.reclen - (self.namelen as u16) - 8).into())?;

		Ok(())
	}

	fn name(&self) -> &OsStr {
		unsafe { OsStr::from_encoded_bytes_unchecked(&self.name[0..self.namelen.into()]) }
	}

	fn minlen(&self) -> u16 {
		(4 + 2 + 1 + 1 + (self.namelen as u16) + 3) & !3
	}
}

fn readdir_block<T>(
	inr: InodeNum,
	block: &[u8],
	config: Config,
	mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut file = Decoder::new(Cursor::new(block), config);

	while let Ok(Some(hdr)) = Header::parse(&mut file) {
		if hdr.inr.get() == 0 {
			break;
		}

		let Some(kind) = hdr.kind else {
			log::warn!(
				"readdir_block({inr}): encountered a whiteout entry: {:?}",
				hdr.name()
			);
			continue;
		};

		let res = f(hdr.name(), hdr.inr, kind);
		if res.is_some() {
			return Ok(res);
		}
	}

	Ok(None)
}

/// Where `newlink_block` put a new entry.
///
/// The offset is needed because a Soft Updates gate is a byte range, and the
/// range that matters is the entry's inode number -- four bytes -- rather than
/// the whole entry.  Gating the name and the type would delay them for no
/// reason; gating nothing would let the entry persist before the inode it
/// names.
fn newlink_block(block: &mut [u8], mut entry: Header, config: Config) -> IoResult<Option<u64>> {
	let mut file = Decoder::new(Cursor::new(block), config);

	loop {
		let pos = file.pos()?;
		let Ok(Some(mut hdr)) = Header::parse(&mut file) else {
			// End of the entries in this block: the rest is free.  Taking it is
			// what lets the first entry land at the offset after `..` instead of
			// forcing the directory to grow while most of the block is empty,
			// and it gives the new entry the whole tail, so a later insert can
			// split it -- which is how FreeBSD packs a block.
			let rem = (DIRBLKSIZE as u64).saturating_sub(pos) as u16;
			if rem < entry.minlen() {
				return Ok(None);
			}

			file.seek(pos)?;
			entry.reclen = rem;
			entry.write(&mut file)?;
			return Ok(Some(pos));
		};
		let minlen = hdr.minlen();
		let rem = hdr.reclen - minlen;
		if rem < entry.minlen() {
			continue;
		}

		hdr.reclen = minlen;
		entry.reclen = rem;

		file.seek(pos)?;
		hdr.write(&mut file)?;
		entry.write(&mut file)?;
		return Ok(Some(pos));
	}
}

/// Remove the entry naming `name` from a directory block.
///
/// Returns the inode it named, whether the record could be reclaimed (`has`),
/// and the offset the removed record occupied.  The offset is what
/// `dir_unlink()` needs to tell the dependency engine *where* the entry was, and
/// it cannot be found afterwards: a removed record parses as end-of-directory,
/// so a second scan would stop at it and never reach the one being asked about.
fn unlink_block(
	dinr: InodeNum,
	block: &mut [u8],
	name: &OsStr,
	config: Config,
) -> IoResult<Option<(InodeNum, bool, u64)>> {
	let mut file = Decoder::new(Cursor::new(block), config);
	let mut prevpos = 0;

	loop {
		let pos = file.pos()?;
		let Ok(Some(hdr)) = Header::parse(&mut file) else {
			break;
		};

		if hdr.name() != name {
			prevpos = pos;
			continue;
		}

		log::trace!("unlink_block({dinr}, {name:?}): pos={pos}, hdr={hdr:?}");

		let has;
		if pos == 0 {
			match Header::parse(&mut file) {
				Ok(Some(next)) => {
					log::trace!("unlink_block({dinr}, {name:?}): next={next:?}");
					let new = Header {
						reclen: hdr.reclen + next.reclen,
						..next
					};
					file.seek(pos)?;
					new.write(&mut file)?;
					has = true;
				}
				_ => {
					log::trace!("unlink_block({dinr}, {name:?}): no next");
					has = false;
				}
			}
		} else {
			file.seek(prevpos)?;
			match Header::parse(&mut file)? {
				Some(mut prev) => {
					prev.reclen += hdr.reclen;
					file.seek(prevpos)?;
					prev.write(&mut file)?;
					has = true;
				}
				None => {
					log::error!(
						"unlink_block({dinr}): previous entry is bad: prevpos={prevpos}, pos={pos}"
					);
					return Err(err!(EIO));
				}
			}
		}
		return Ok(Some((hdr.inr, has, pos)));
	}

	Ok(None)
}

/// The offset of the `..` entry in a directory block, if it is in this block.
///
/// `..` is written second by [`newdir`], so it is at a fixed offset in the
/// first block, but scanning for it rather than assuming is what keeps this
/// honest if the record layout ever changes.
fn dotdot_entry(block: &[u8], config: Config) -> IoResult<Option<u64>> {
	let mut file = Decoder::new(Cursor::new(block), config);
	loop {
		let pos = file.pos()?;
		let Ok(Some(hdr)) = Header::parse(&mut file) else {
			return Ok(None);
		};
		if hdr.name[..hdr.namelen as usize] == *b".." {
			return Ok(Some(pos));
		}
	}
}

fn newdir(dinr: InodeNum, inr: InodeNum, config: Config) -> IoResult<[u8; DIRBLKSIZE]> {
	let mut block = [0u8; DIRBLKSIZE];
	let mut file = Decoder::new(Cursor::new(&mut block as &mut [u8]), config);

	let h_self = Header::new(inr, InodeType::Directory, OsStr::new("."));
	let h_parent = Header::new(dinr, InodeType::Directory, OsStr::new(".."));
	// Both keep their natural size.  Giving `..` the whole remaining block --
	// which is what this used to do -- makes it the last entry in the block,
	// and the last entry in a block is the one `newlink_block()` splits to make
	// room.  The first file added to a new directory would therefore be written
	// *into* `..`, pushing it out of position 12 and leaving two `.`/`..`
	// records in the listing.
	//
	// FreeBSD's `newdir()` keeps `.` at offset 0 and `..` immediately after it
	// at their natural sizes, and packs the first real entry into the space
	// that follows; `resources/ufs-little.img` has exactly that layout.
	h_self.write(&mut file)?;
	h_parent.write(&mut file)?;

	Ok(block)
}

impl<R: Backend> Ufs<R> {
	/// Find a file named `name` in the directory referenced by `pinr`.
	pub fn dir_lookup(&mut self, pinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
		log::trace!("dir_lookup({pinr}, {name:?});");
		self.dir_iter(
			pinr,
			|name2, inr, _kind| {
				if name == name2 {
					Some(inr)
				} else {
					None
				}
			},
		)?
		.ok_or(err!(ENOENT))
	}

	/// Collect the whole listing of the directory `inr`.
	///
	/// The collecting form of [`Ufs::dir_iter`], for callers that cannot read a
	/// directory entry and act on it within one `&mut self` borrow -- fetching an
	/// inode's attributes while `dir_iter` holds the filesystem is the case that
	/// motivates this.
	///
	/// The listing is built the same way for every caller, so callers that
	/// differ only in what they report about each entry cannot disagree about
	/// which entries exist.
	pub fn dir_entries(&mut self, inr: InodeNum) -> IoResult<Vec<DirEntry>> {
		let mut out = Vec::new();
		self.dir_iter(inr, |name, inr, kind| -> Option<()> {
			out.push(DirEntry {
				inr,
				kind,
				// `name` borrows a block `dir_iter` is about to reuse.
				name: name.to_os_string(),
				offset: out.len() as i64 + 1,
			});
			None
		})?;
		Ok(out)
	}

	/// Iterate through a directory referenced by `inr`, and call `f` for each entry.
	pub fn dir_iter<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let ino = self.read_inode(inr)?;
		ino.assert_dir()?;
		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < ino.size {
			let n = self.inode_read(inr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);
			if let Some(x) = readdir_block(inr, &block, self.file.config(), &mut f)? {
				return Ok(Some(x));
			}

			pos += DIRBLKSIZE as u64;
		}
		Ok(None)
	}

	/// Remove a directory entry.
	///
	/// Returns the inode it named and the cache block it was removed from.  The
	/// block matters to `rmdir`: the parent's link count is not allowed to reach
	/// the disk before the removal that justifies it does, and that is a
	/// statement about one specific block.
	pub(super) fn dir_unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<(InodeNum, u64)> {
		log::trace!("dir_unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;

		let mut block = vec![0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			let n = self.inode_read(dinr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);

			if let Some((inr, has, at)) = unlink_block(dinr, &mut block, name, self.file.config())?
			{
				let blk = self.dirent_block(dinr, pos)?.ok_or_else(|| err!(EIO))?;
				if has {
					// The entry's inode number is already zero in `block`.  Record
					// where it was, so the inode's reclamation can wait for the
					// disk to agree the entry is gone.
					self.inode_write(dinr, pos, &block)?;
					self.note_dirent_removed(dinr, pos, at, inr)?;
				} else {
					let n =
						self.inode_copy_range(dinr, &dino, (pos + DIRBLKSIZE as u64).., pos..)?;
					assert_eq!(n, dino.size - pos - DIRBLKSIZE as u64);
					self.inode_truncate(dinr, dino.size - DIRBLKSIZE as u64)?;
				}
				return Ok((inr, blk));
			}

			pos += DIRBLKSIZE as u64;
		}

		Err(err!(ENOENT))
	}

	/// Add a directory entry, and report the cache block it went into.
	///
	/// The block matters to `rename_dir`, which has to hold a parent's link count
	/// back until the entry that justifies it is on the disk -- and that is a
	/// statement about one specific block.
	pub(super) fn dir_newlink(
		&mut self,
		dinr: InodeNum,
		inr: InodeNum,
		name: &OsStr,
		kind: InodeType,
	) -> IoResult<u64> {
		log::trace!("dir_newlink({dinr}, {inr}, {name:?}, {kind:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;

		let mut entry = Header::new(inr, kind, name);

		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			let n = self.inode_read(dinr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);

			if let Some(at) = newlink_block(&mut block, entry, self.file.config())? {
				self.inode_write(dinr, pos, &block)?;
				self.gate_dirent(dinr, pos, at, inr)?;
				return self
					.dirent_block(dinr, pos)
					.and_then(|b| b.ok_or_else(|| err!(EIO)));
			}

			pos += DIRBLKSIZE as u64;
		}

		log::trace!("dir_link({dinr}, {inr}, {name:?}, {kind:?}): extending directory for new entry: {entry:?}");
		self.inode_truncate(dinr, dino.size + DIRBLKSIZE as u64)?;
		entry.reclen = DIRBLKSIZE as u16;
		entry.write(&mut Decoder::new(
			Cursor::new(&mut block as &mut [u8]),
			self.file.config(),
		))?;
		self.inode_write(dinr, pos, &block)?;
		// A fresh block starts with the entry at offset zero.
		self.gate_dirent(dinr, pos, 0, inr)?;
		self.dirent_block(dinr, pos)
			.and_then(|b| b.ok_or_else(|| err!(EIO)))
	}

	/// Hold back the inode number of a directory entry until `inr` is on the
	/// disk.
	///
	/// Created by [`Self::dir_newlink`].  Protects invariants 5 and 6 in
	/// `docs/ufs2-invariants.md`.
	///
	/// # The ordering
	///
	/// A directory entry must not reach the disk before the inode it names is
	/// there.  The reverse order -- inode first, then the entry -- is fine: a
	/// directory entry that has not been created yet costs an allocated inode
	/// that nothing points at, which `fsck` pass 2 reclaims harmlessly.  The
	/// other order is the disaster: a crash leaves a directory entry naming an
	/// inode whose bitmap bit is clear and whose contents are zeroes, so the file
	/// appears to exist, reads back as empty, and `fsck` pass 2 removes the
	/// entry -- discarding a directory whose data was never lost, only
	/// unreachable.
	///
	/// # Why four bytes
	///
	/// Only `direntry::inr` is gated, not the name or the type.  The name and
	/// the type are consequences of the inode, not references to it, and
	/// holding them back would delay a lookup finding a file that is already
	/// perfectly readable.  The whole entry is not gated for the same reason:
	/// the block it lives in usually holds other entries that have nothing to
	/// do with this allocation, and the safe image exists precisely so that
	/// they need not be delayed.
	///
	/// # What resolves it
	///
	/// The inode's own write reaching the device.  There is no "inode safe"
	/// event to reuse -- an inode's block map is a pointer, and *that* is gated
	/// on the blocks it names -- so the event is signalled from
	/// `sync_metadata()` once the inode's block has actually been written.
	fn gate_dirent(
		&mut self,
		dinr: InodeNum,
		file_pos: u64,
		block_off: u64,
		inr: InodeNum,
	) -> IoResult<()> {
		let Some(blk) = self.dirent_block(dinr, file_pos)? else {
			// The block was freed underneath us; there is nothing to gate and
			// nothing that can reach the entry.
			return Ok(());
		};
		let off = block_off;
		log::trace!("gating {dinr}'s entry for {inr} at {blk}:{off}+{DIRENT_INR_LEN}");
		self.softdep.gate(
			&mut self.buf,
			crate::softdep::DepKind::DirectoryAdd,
			blk,
			off,
			DIRENT_INR_LEN,
			crate::softdep::Gate::InodeWritten(inr),
		)?;
		Ok(())
	}

	/// The cache block holding the directory block at file offset `pos` of
	/// `dinr`.
	pub(super) fn dirent_block(&mut self, dinr: InodeNum, pos: u64) -> IoResult<Option<u64>> {
		let ino = self.read_inode(dinr)?;
		let Some(info) = self.inode_locate(dinr, &ino, pos) else {
			return Ok(None);
		};
		Ok(self
			.inode_resolve_block(dinr, &ino, info.blkidx)?
			.map(|b| self.metadata_blk(b.get())))
	}

	pub fn unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		log::trace!("unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;

		if self.read_inode(inr)?.kind() == InodeType::Directory {
			// POSIX: `unlink` on a directory is `EISDIR` on Linux and `EPERM` on
			// the BSDs; `rmdir` is the operation for it.  Allowing it here did
			// not corrupt anything on its own -- the directory keeps its `.` and
			// `..` and merely ends up with a link count of one -- but it left the
			// filesystem reporting a link count that no longer matched anything
			// that could reach the directory, which the randomised test found as a
			// directory with no `.` at all.
			log::warn!("unlink({dinr}, {name:?}): {inr} is a directory; use rmdir");
			return Err(err!(EISDIR));
		}

		self.unlink_entry(dinr, name).map(|_| ())
	}

	/// Remove a directory entry and drop a link from whatever it named, with no
	/// restriction on what that is.
	///
	/// This is the primitive behind [`Self::unlink`], and it is also what
	/// `rmdir` uses for a directory's own `.` and `..` -- entries that *name*
	/// directories, which the public `unlink` correctly refuses.  Refusing them
	/// there is what stopped `rmdir` working at all.
	fn unlink_entry(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		let (inr, _blk) = self.dir_unlink(dinr, name)?;
		self.inode_free(inr)?;
		Ok(())
	}

	pub fn rename(
		&mut self,
		d_dinr: InodeNum,
		d_name: &OsStr,
		s_dinr: InodeNum,
		s_name: &OsStr,
		replace: bool,
	) -> IoResult<InodeNum> {
		log::trace!("rename({d_dinr}, {d_name:?} {s_dinr}, {s_name:?}, {replace});");
		self.assert_rw()?;

		let inr = self.dir_lookup(s_dinr, s_name)?;

		if !replace {
			// The user has requested an error if the destination already exists.
			//
			// The *destination*: this used to look the *source* up, which is a
			// tautology -- `dir_lookup(s_dinr, s_name)` above already succeeded,
			// or `rename` would not have got this far -- so `replace = false`
			// failed with EEXIST for every rename, including onto a name that did
			// not exist.  The `replace` branch below got it right.
			match self.dir_lookup(d_dinr, d_name) {
				Ok(_) => return Err(err!(EEXIST)),
				// TODO: Need raw OS error here?
				Err(e) if e.kind() == ErrorKind::NotFound => {}
				// TODO: Might want to handle not a directory etc specially.
				Err(e) => return Err(e),
			}
		} else {
			match self.dir_lookup(d_dinr, d_name) {
				// A destination directory has to go through `rmdir`, which checks
				// that it is empty and -- unlike `unlink` -- takes the link off its
				// parent.  Calling `unlink` on one destroyed it: `unlink` never
				// removes the `..` that is the link.
				Ok(old) if self.inode_attr(old)?.kind == InodeType::Directory => {
					self.rmdir(d_dinr, d_name)?;
				}
				Ok(_) => self.unlink(d_dinr, d_name)?,
				Err(e) if e.kind() == ErrorKind::NotFound => {}
				Err(e) => return Err(e),
			}
		}

		let kind = self.inode_attr(inr)?.kind;
		if kind == InodeType::Directory {
			return self.rename_dir(d_dinr, d_name, s_dinr, s_name, inr);
		}

		// `unlink` decrements the refcount, if it reaches 0 the file may get removed.
		// Bump the counter before to workaround.
		self.inode_bump(inr)?;
		self.dir_newlink(d_dinr, inr, d_name, kind)?;

		self.unlink(s_dinr, s_name)?;
		Ok(inr)
	}

	/// Move a directory from `(s_dinr, s_name)` to `(d_dinr, d_name)`.
	///
	/// A directory rename is three operations, not two, and the link counts are a
	/// consequence of doing all three rather than something to adjust
	/// separately:
	///
	/// 1. remove the entry from the source parent;
	/// 2. add it to the destination parent;
	/// 3. rewrite the moved directory's `..`.
	///
	/// Doing only the first two is what this used to do, and it leaves two
	/// things wrong.  Neither parent's `i_nlink` moves -- a link count is a
	/// count of entries, and moving one does not change either count -- so the
	/// source parent keeps counting a directory its tree no longer contains.
	/// And the moved directory's `..` still names the *old* parent, so anything
	/// that walks up from it arrives somewhere that has never heard of it.  The
	/// randomised crash test found the link count; it is the same defect.
	///
	/// The destination parent's count is also untouched, which is right: a move
	/// does not add a subdirectory, it relocates one.  It only changes when the
	/// rename *replaces* a directory, and that removal is `rmdir`'s job, above.
	fn rename_dir(
		&mut self,
		d_dinr: InodeNum,
		d_name: &OsStr,
		s_dinr: InodeNum,
		s_name: &OsStr,
		inr: InodeNum,
	) -> IoResult<InodeNum> {
		log::trace!("rename_dir({d_dinr}, {d_name:?} <- {s_dinr}, {s_name:?} = {inr});");

		// Moving a directory inside itself would make `..` a cycle, and `..` is
		// the only way anything finds a directory's parent.
		if self.is_descendant(inr, d_dinr)? {
			log::error!("rename_dir: {inr} contains {d_dinr}");
			return Err(err!(EINVAL));
		}

		// 1 and 2: move the entry.  `dir_unlink` rather than `unlink`, because
		// the directory must survive: `unlink` would take it to zero links and
		// free a directory that is being moved rather than removed.
		let (moved, s_dir_blk) = self.dir_unlink(s_dinr, s_name)?;
		debug_assert_eq!(moved, inr);
		let d_dir_blk = self.dir_newlink(d_dinr, inr, d_name, InodeType::Directory)?;

		// 3: the moved directory's `..` has to name its new parent, or every
		// walk up from it lands in a directory that does not contain it.
		if s_dinr != d_dinr {
			self.rewrite_dotdot(inr, d_dinr)?;

			// A directory's depth is *cached* in the inode, set from the parent's
			// at allocation.  Moving one leaves every descendant's cached value
			// stale, and `check_consistency()` compares a child's cached depth
			// against its parent's, so a stale value reads as a contradiction.
			//
			// The whole subtree moves, not just the directory: `..` is the only way
			// anything finds a directory's parent, and depth is the same kind of
			// derived-but-stored value.
			let was = self.read_inode(s_dinr)?.dir_depth().unwrap_or(0);
			let now = self.read_inode(d_dinr)?.dir_depth().unwrap_or(0);
			if now != was {
				self.shift_dir_depth(inr, now as i64 - was as i64, 0)?;
			}
		}

		// 4: the parents' counts.  A directory's `i_nlink` is 2 plus its
		// subdirectories, so from the source parent's side the move removes a
		// subdirectory and from the destination parent's side it adds one.
		//
		// I wrote in the comment above that a move "changes neither count", and
		// that was wrong in the way that matters: it is right of the *moved*
		// directory and wrong of both parents.  The randomised test found it --
		// seed 14 -- and the count that came out too low is the dangerous
		// direction, because a root whose `i_nlink` understates its
		// subdirectories looks removable to anything reasoning from the count.
		//
		// These are counters, so they cannot be byte-range gated: zeroing one
		// would be a wrong number rather than a stale one.  What holds them back
		// is the whole-buffer mechanism `MkdirParentDep` uses.  The device then
		// keeps the previous inode image, whose count was consistent with the
		// previous entry layout -- and that layout is self-consistent on its own.
		//
		// Both parents are held against *both* blocks, because either half of the
		// move -- the source entry gone and the destination entry not yet there,
		// or the reverse -- is a state in which one of the two counts is wrong.
		if s_dinr != d_dinr {
			let mut sdino = self.read_inode(s_dinr)?;
			sdino.nlink -= 1;
			self.write_inode(s_dinr, &sdino)?;

			let mut ddino = self.read_inode(d_dinr)?;
			ddino.nlink += 1;
			self.write_inode(d_dinr, &ddino)?;

			for parent in [s_dinr, d_dinr] {
				let pblk = self.metadata_blk(self.superblock.ino_to_fsba(parent));
				for dblk in [s_dir_blk, d_dir_blk] {
					self.block_inode_on_dir(parent, pblk, dblk);
				}
			}
		}

		Ok(inr)
	}

	/// Shift the cached depth of `inr`'s subtree by `delta`.
	///
	/// `hops` bounds the recursion.  `rename_dir` refuses to move a directory
	/// inside itself first, so a cycle cannot be created here -- but the image
	/// could already be cyclic, and an unbounded walk over one turns an error
	/// into a hang.
	fn shift_dir_depth(&mut self, inr: InodeNum, delta: i64, hops: u32) -> IoResult<()> {
		if hops > 4096 {
			log::error!("shift_dir_depth({inr}): no root after 4096 levels");
			return Err(err!(EIO));
		}
		let mut ino = self.read_inode(inr)?;
		if ino.kind() != InodeType::Directory {
			return Ok(());
		}
		let depth = ino.dir_depth().unwrap_or(0);
		let shifted = depth as i64 + delta;
		if shifted < 0 {
			log::error!("shift_dir_depth({inr}): depth {depth} + {delta} underflows");
			return Err(err!(EIO));
		}
		ino.set_dir_depth(shifted as u32);
		self.write_inode(inr, &ino)?;
		log::trace!("shift_dir_depth({inr}): {depth} -> {shifted}");

		let mut kids = Vec::new();
		self.dir_iter(inr, |name, inr2, kind| {
			if kind == InodeType::Directory && name != OsStr::new(".") && name != OsStr::new("..") {
				kids.push(inr2);
			}
			None::<u8>
		})?;
		for k in kids {
			self.shift_dir_depth(k, delta, hops + 1)?;
		}
		Ok(())
	}

	/// Whether `ancestor` is `start` or one of its ancestors.
	///
	/// Walks `..` and stops after a bounded number of steps, because a
	/// filesystem that is already cyclic must not hang here: this runs on the
	/// error path of nothing, and an unbounded walk over a corrupt image would
	/// turn a rejected rename into a hang.
	fn is_descendant(&mut self, ancestor: InodeNum, start: InodeNum) -> IoResult<bool> {
		let mut cur = start;
		for _ in 0..1024 {
			if cur == ancestor {
				return Ok(true);
			}
			match self.dir_lookup(cur, OsStr::new("..")) {
				Ok(next) if next != cur => cur = next,
				_ => return Ok(false),
			}
		}
		log::error!("is_descendant({ancestor}, {start}): no root after 1024 levels");
		Ok(false)
	}

	/// Point `inr`'s `..` at `parent`.
	fn rewrite_dotdot(&mut self, inr: InodeNum, parent: InodeNum) -> IoResult<()> {
		let dino = self.read_inode(inr)?;
		let mut block = vec![0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			let n = self.inode_read(inr, pos, &mut block)?;
			assert_eq!(n, DIRBLKSIZE);
			if let Some(at) = dotdot_entry(&block, self.file.config())? {
				let at = at as usize;
				block[at..at + 4].copy_from_slice(&parent.get().to_le_bytes());
				self.inode_write(inr, pos, &block)?;
				log::trace!("rewrite_dotdot({inr}): .. is now {parent}");
				return Ok(());
			}
			pos += DIRBLKSIZE as u64;
		}
		Err(err!(ENOENT))
	}

	pub fn rmdir(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;
		let x = self.dir_iter(inr, |name, _inr, kind| {
			if kind != InodeType::Directory || (name != "." && name != "..") {
				Some(name.to_os_string())
			} else {
				None
			}
		})?;

		if x.is_some() {
			log::debug!("rmdir({dinr}, {name:?}): x = {x:?}");
			return Err(err!(ENOTEMPTY));
		}

		let (child, dir_blk) = self.dir_unlink(dinr, name)?;

		self.unlink_entry(inr, OsStr::new(".."))?;
		self.unlink_entry(inr, OsStr::new("."))?;
		self.inode_free(inr)?;

		// The parent's link count comes off here -- but not directly.  It comes
		// off when `..` is unlinked above, because `..` named the parent, and
		// that is the only decrement a removed subdirectory should cause.
		//
		// What is missing is the *ordering*: that decrement used to reach the
		// disk before the entry removal did, so a crash in between left the
		// parent counting a subdirectory its tree no longer contains.  The
		// randomised crash test found it -- seed 10 -- and the repair is the same
		// one `mkdir` uses: hold the parent's inode until the directory block
		// holding the removal is on the disk.  See `Ufs::block_inode_on_dir`.
		let parent_blk = self.metadata_blk(self.superblock.ino_to_fsba(dinr));
		self.block_inode_on_dir(dinr, parent_blk, dir_blk);

		let _ = child;
		Ok(())
	}

	pub fn mknod(
		&mut self,
		dinr: InodeNum,
		name: &OsStr,
		kind: InodeType,
		perm: u16,
		uid: u32,
		gid: u32,
	) -> IoResult<InodeAttr> {
		self.assert_rw()?;
		check_name_is_legal(name, false)?;
		let mut ino = Inode::new(kind, perm, uid, gid, self.superblock.bsize as u32);
		let inr = self.inode_alloc(Some(dinr), &mut ino)?;
		self.dir_newlink(dinr, inr, name, kind)?;
		Ok(ino.as_attr(inr))
	}

	pub fn mkdir(
		&mut self,
		dinr: InodeNum,
		name: &OsStr,
		perm: u16,
		uid: u32,
		gid: u32,
	) -> IoResult<InodeAttr> {
		let inr = self
			.mknod(dinr, name, InodeType::Directory, perm, uid, gid)?
			.inr;

		// The entry `mknod()` just staged is in the last directory block, so
		// that block is the one the parent's link count has to wait for.
		let dsize = self.read_inode(dinr)?.size;
		let dir_blk = match self.dirent_block(dinr, dsize.saturating_sub(1))? {
			Some(b) => b,
			None => self.dirent_block(dinr, 0)?.ok_or_else(|| err!(EIO))?,
		};
		let parent_blk = self.metadata_blk(self.superblock.ino_to_fsba(dinr));

		let mut dino = self.read_inode(dinr)?;
		dino.nlink += 1;
		self.write_inode(dinr, &dino)?;
		self.block_inode_on_dir(dinr, parent_blk, dir_blk);

		// update nlink
		let mut ino = self.read_inode(inr)?;
		ino.nlink = 2;
		self.write_inode(inr, &ino)?;

		let block = newdir(dinr, inr, self.file.config())?;
		self.inode_truncate(inr, block.len() as u64)?;
		self.inode_write(inr, 0, &block)?;

		let ino = self.read_inode(inr)?;
		Ok(ino.as_attr(inr))
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::{OsStr, OsString};

	use super::*;
	use crate::testutil;

	const IMAGES: [&str; 2] = ["ufs-little", "ufs-big"];

	/// What a kernel sees when it reads a directory through `READDIR`, and then
	/// through `READDIRPLUS`, has to be one listing, not two that happen to
	/// agree.  Both FUSE operations in `fuse3.rs` are built from this function,
	/// so asserting that it agrees with `dir_iter` is what makes "they cannot
	/// disagree" a checked claim rather than a comment.
	#[test]
	fn dir_entries_agrees_with_dir_iter() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);

			let mut streamed = Vec::new();
			ug.dir_iter(InodeNum::ROOT, |n, inr, kind| -> Option<()> {
				streamed.push((n.to_os_string(), inr, kind));
				None
			})
			.unwrap();

			let collected = ug.dir_entries(InodeNum::ROOT).unwrap();
			let from_entries: Vec<_> = collected
				.iter()
				.map(|e| (e.name.clone(), e.inr, e.kind))
				.collect();

			assert_eq!(
				from_entries, streamed,
				"{name}: dir_entries diverged from dir_iter"
			);
			assert!(!collected.is_empty(), "{name}: empty root listing");
		}
	}

	/// Offsets count from 1 and are contiguous.
	///
	/// The kernel resumes a partially-read directory by handing back the offset
	/// of the last entry it received, so a gap or a duplicated value here
	/// silently loses or repeats entries on a directory too large for one
	/// reply -- and never on one that fits, which is why it is worth pinning.
	#[test]
	fn offsets_are_contiguous_and_one_based() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);
			let entries = ug.dir_entries(InodeNum::ROOT).unwrap();

			for (i, e) in entries.iter().enumerate() {
				assert_eq!(
					e.offset,
					i as i64 + 1,
					"{name}: offset {} at index {i}",
					e.offset
				);
			}
		}
	}

	/// Replaying the kernel's loop -- fill a reply buffer, resume from the last
	/// offset sent -- has to yield the whole listing exactly once, whatever the
	/// buffer size.
	///
	/// This is the property the offsets exist for, and it is checked at page
	/// sizes around the ones that actually straddle a directory block.
	fn continuation_yields_everything_exactly_once(entries: &[DirEntry], page: usize) {
		let mut got = Vec::new();
		let mut cursor = 0i64;
		loop {
			let batch: Vec<_> = entries
				.iter()
				.filter(|e| e.offset > cursor)
				.take(page)
				.collect();
			if batch.is_empty() {
				break;
			}
			cursor = batch.last().unwrap().offset;
			got.extend(batch.into_iter().cloned());
		}
		assert_eq!(got, entries, "continuation lost or duplicated entries");
	}

	#[test]
	fn continuation_is_exhaustive_at_every_page_size() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);
			let entries = ug.dir_entries(InodeNum::ROOT).unwrap();
			let n = entries.len();
			for page in [1, 2, 3, 5, n, n + 1, 64] {
				continuation_yields_everything_exactly_once(&entries, page);
			}
		}
	}

	/// Reading a directory twice gives the same thing.
	///
	/// Offsets are only usable as cursors if they do not change between reads;
	/// if they did, a kernel resuming a read would skip or repeat entries.
	#[test]
	fn offsets_are_stable_across_reads() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);
			let a = ug.dir_entries(InodeNum::ROOT).unwrap();
			let b = ug.dir_entries(InodeNum::ROOT).unwrap();
			assert_eq!(a, b, "{name}: listing changed between identical reads");
		}
	}

	/// `.` and `..` come first, name the directory and its parent, and are
	/// directories themselves.
	#[test]
	fn dot_entries_lead_the_listing() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);
			let entries = ug.dir_entries(InodeNum::ROOT).unwrap();

			let dot = &entries[0];
			let dotdot = &entries[1];
			assert_eq!(dot.name, OsStr::new("."));
			assert_eq!(dotdot.name, OsStr::new(".."));
			assert_eq!(
				dot.kind,
				InodeType::Directory,
				"{name}: . is not a directory"
			);
			assert_eq!(
				dotdot.kind,
				InodeType::Directory,
				"{name}: .. is not a directory"
			);
			assert_eq!(
				dot.inr,
				InodeNum::ROOT,
				"{name}: . does not name the directory"
			);
			assert_eq!(
				dotdot.inr,
				InodeNum::ROOT,
				"{name}: .. does not name the parent"
			);
		}
	}

	/// Every entry names something that exists, with the type the directory
	/// records -- the claim `READDIRPLUS` makes about the whole listing.
	#[test]
	fn every_entry_names_an_inode_of_that_type() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_ro(name);
			let entries = ug.dir_entries(InodeNum::ROOT).unwrap();

			for e in &entries {
				let ino = ug.read_inode(e.inr).unwrap();
				assert_eq!(
					ino.kind(),
					e.kind,
					"{name}: {} is recorded as {:?} but is {:?}",
					e.name.to_string_lossy(),
					e.kind,
					ino.kind()
				);
			}
		}
	}

	/// A new directory holds `.` and `..` and nothing else, and they name the
	/// right places.
	#[test]
	fn empty_directory_holds_only_dot_entries() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let root = InodeNum::ROOT;
			ug.mkdir(root, OsStr::new("empty"), 0o755, 0, 0).unwrap();
			let dir = ug.dir_lookup(root, OsStr::new("empty")).unwrap();

			let entries = ug.dir_entries(dir).unwrap();
			assert_eq!(entries.len(), 2, "{name}: {entries:?}");
			assert_eq!(entries[0].name, OsStr::new("."));
			assert_eq!(entries[1].name, OsStr::new(".."));
			assert_eq!(
				entries[0].inr, dir,
				"{name}: . does not name the new directory"
			);
			assert_eq!(entries[1].inr, root, "{name}: .. does not name the parent");
			assert_eq!(entries[0].offset, 1);
			assert_eq!(entries[1].offset, 2);
		}
	}

	/// Every record in a directory's first block: (offset, inode, reclen, name).
	fn first_block(
		ug: &mut crate::Ufs<std::fs::File>,
		dir: InodeNum,
	) -> Vec<(usize, InodeNum, u16, OsString)> {
		let mut blk = [0u8; DIRBLKSIZE];
		ug.inode_read(dir, 0, &mut blk).unwrap();
		let config = ug.file.config();
		let mut out = Vec::new();
		let mut off = 0usize;
		while off < DIRBLKSIZE {
			let mut file = Decoder::new(Cursor::new(&blk), config);
			file.seek(off as u64).unwrap();
			let Some(h) = Header::parse(&mut file).unwrap() else {
				break;
			};
			out.push((off, h.inr, h.reclen, h.name().to_owned()));
			off += usize::from(h.reclen);
		}
		out
	}

	/// A directory this driver creates has the layout FreeBSD's `newdir()`
	/// gives one, and which `resources/ufs-little.img` demonstrates: `.` at
	/// offset 0 and `..` immediately after it, each at its natural size, with
	/// the first real entry packed into the space that follows.
	///
	/// Getting this wrong is silent, which is what makes it worth pinning.  An
	/// earlier version gave `..` a reclen covering the rest of the block, and
	/// that is exactly what marks a record as the last entry in the block --
	/// the one `newlink_block()` splits to make room.  So the first file added
	/// to a new directory was written into `..`, pushing it out of position 12
	/// and leaving a directory that listed `.` twice with `..` in the middle.
	#[test]
	fn new_directory_has_the_freebsd_dot_entry_layout() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let root = InodeNum::ROOT;
			ug.mkdir(root, OsStr::new("laid"), 0o755, 0, 0).unwrap();
			let dir = ug.dir_lookup(root, OsStr::new("laid")).unwrap();

			let recs = first_block(&mut ug, dir);
			assert_eq!(
				recs[0],
				(0, dir, 12, OsString::from(".")),
				"{name}: . is not a 12-byte record at offset 0"
			);
			assert_eq!(
				recs[1],
				(12, root, 12, OsString::from("..")),
				"{name}: .. is not a 12-byte record at offset 12"
			);

			// The first file packs into the space after `..`, rather than
			// forcing the directory to grow while most of block 0 is empty.
			// Enough to span several blocks, so the ordering this pins holds
			// past the point where the directory has to grow.
			let added = 32;
			let made: Vec<String> = (0..added)
				.map(|i| format!("file-{i:04}-padding-padding-padding"))
				.collect();
			for n in &made {
				ug.mknod(dir, OsStr::new(n), InodeType::RegularFile, 0o644, 0, 0)
					.unwrap();
			}

			// The dot entries are still at their offsets and still whole.
			let recs = first_block(&mut ug, dir);
			assert_eq!(
				recs[0],
				(0, dir, 12, OsString::from(".")),
				"{name}: . moved"
			);
			assert_eq!(
				recs[1],
				(12, root, 12, OsString::from("..")),
				"{name}: .. moved or was split"
			);
			assert_eq!(
				recs[2].3.to_string_lossy(),
				made[0],
				"{name}: first file did not pack after .."
			);

			// And the listing is exactly the files, after `.` and `..`, each
			// once -- the property that was silently false before.
			let entries = ug.dir_entries(dir).unwrap();
			let names: Vec<OsString> = entries.iter().map(|e| e.name.clone()).collect();
			let mut want = vec![OsString::from("."), OsString::from("..")];
			want.extend(made.iter().map(OsString::from));
			assert_eq!(names, want, "{name}: listing is not what was created");
		}
	}

	/// Growing, emptying and regrowing a directory leaves an image `fsck`
	/// considers coherent.
	///
	/// The layout bug above was invisible to `check_consistency` and to every
	/// name-based lookup, because the entry was still findable -- it was just
	/// in the wrong place, and `..` had been split in two.  Only walking the
	/// directory shows it.  This test therefore checks the listing *and* asks
	/// `fsck`, because the two failures are independent.
	#[test]
	fn a_mutated_directory_stays_coherent() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let root = InodeNum::ROOT;
			ug.mkdir(root, OsStr::new("mutated"), 0o755, 0, 0).unwrap();
			let dir = ug.dir_lookup(root, OsStr::new("mutated")).unwrap();

			let named = |i: u32| format!("f{i:04}-padding-padding-padding");
			for i in 0..40 {
				ug.mknod(
					dir,
					OsStr::new(&named(i)),
					InodeType::RegularFile,
					0o644,
					0,
					0,
				)
				.unwrap();
			}
			// Remove a run from the middle and a run from the start, so the
			// freed records are ones a later insert has to reuse.
			for i in 0..10 {
				ug.unlink(dir, OsStr::new(&named(i))).unwrap();
			}
			for i in 20..25 {
				ug.unlink(dir, OsStr::new(&named(i))).unwrap();
			}
			// Refill with names that do not fit in what the removals freed, so
			// the tail has to be taken as well as any hole.
			for i in 100..130 {
				ug.mknod(
					dir,
					OsStr::new(&named(i)),
					InodeType::RegularFile,
					0o644,
					0,
					0,
				)
				.unwrap();
			}
			ug.sync_metadata().unwrap();

			let entries = ug.dir_entries(dir).unwrap();
			let mut got: Vec<String> = entries[2..]
				.iter()
				.map(|e| e.name.to_string_lossy().into_owned())
				.collect();
			got.sort();
			let mut want: Vec<String> = (10..20).chain(25..40).chain(100..130).map(named).collect();
			want.sort();
			assert_eq!(got, want, "{name}: listing is not what was left");
			assert_eq!(entries[0].name, OsStr::new("."));
			assert_eq!(entries[1].name, OsStr::new(".."));

			let rep = ug.check_consistency().unwrap();
			assert!(rep.is_clean(), "{name}: {}", rep.summary());
		}
	}

	/// The case the golden image cannot cover: a directory spanning several
	/// UFS directory blocks.
	///
	/// `DIRBLKSIZE` is 512, and each entry costs its name plus an 8-byte header,
	/// so long names reach a second block in a handful of entries.  The seam is
	/// where an offset computed per-block instead of per-listing would go wrong,
	/// which is why the continuation is replayed at page sizes around it.
	#[test]
	fn multi_block_directory_numbers_continuously_across_the_seam() {
		for name in IMAGES {
			let (_img, mut ug) = testutil::open_rw(name);
			let root = InodeNum::ROOT;
			ug.mkdir(root, OsStr::new("many"), 0o755, 0, 0).unwrap();
			let dir = ug.dir_lookup(root, OsStr::new("many")).unwrap();

			// Enough to spill well past one 512-byte block.
			let entries_added = 32;
			for i in 0..entries_added {
				let name_ = format!("{i:04}-a-name-long-enough-to-fill-the-block");
				ug.mknod(dir, OsStr::new(&name_), InodeType::RegularFile, 0o644, 0, 0)
					.unwrap();
			}

			let size = ug.read_inode(dir).unwrap().size;
			assert!(
				size > DIRBLKSIZE as u64,
				"{name}: directory is {size} bytes, not multi-block"
			);

			let entries = ug.dir_entries(dir).unwrap();
			assert_eq!(
				entries.len(),
				entries_added + 2,
				"{name}: wrong entry count"
			);
			for (i, e) in entries.iter().enumerate() {
				assert_eq!(e.offset, i as i64 + 1, "{name}: offset gap at {i}");
			}

			// Page sizes around the block size, so the boundary is exercised
			// from both sides.
			for page in [1, 3, 6, 7, 8, 15, 34] {
				continuation_yields_everything_exactly_once(&entries, page);
			}

			// Resuming from an offset that is not the last one handed out --
			// a kernel restarting a read part way -- yields the strict suffix.
			let mid = 5;
			let suffix: Vec<_> = entries.iter().filter(|e| e.offset > mid).cloned().collect();
			assert_eq!(suffix.len(), entries.len() - mid as usize);
			assert_eq!(suffix[0].offset, mid + 1);
			assert!(entries
				.iter()
				.all(|e| e.offset <= mid || suffix.contains(e)));
		}
	}
}
