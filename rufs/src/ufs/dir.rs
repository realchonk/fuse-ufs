use std::io::{BufRead, Write};

use super::*;
use crate::{err, InodeNum};

#[derive(Debug, Clone, Copy)]
struct Header {
	inr:     InodeNum,
	reclen:  u16,
	dtype:   u8,
	namelen: u8,
	name:    [u8; UFS_MAXNAMELEN + 1],
}

impl Header {
	fn new(inr: InodeNum, kind: InodeType, dname: &[u8]) -> Self {
		assert!(dname.len() <= UFS_MAXNAMELEN);
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		name[0..dname.len()].copy_from_slice(dname);
		let reclen = dirsiz(dname.len() as u8);
		Self {
			inr,
			reclen,
			dtype: dtype_of(kind),
			name,
			namelen: dname.len() as u8,
		}
	}

	/// `end` is the end of the chunk. `None` there, `EIO` for a corrupt record.
	fn parse<T: BufRead + Seek>(file: &mut Decoder<T>, end: u64) -> IoResult<Option<Header>> {
		let pos = file.pos()?;
		if pos >= end {
			return Ok(None);
		}
		if end - pos < 8 {
			log::error!("Header::parse(): truncated record at {pos}");
			return Err(err!(EIO));
		}
		let inr: InodeNum = file.decode()?;
		let reclen: u16 = file.decode()?;
		if reclen == 0 {
			return Ok(None);
		}
		let kind: u8 = file.decode()?;
		let namelen: u8 = file.decode()?;
		// The checks of fsck's dircheck().
		if reclen < 8 ||
			!reclen.is_multiple_of(4) ||
			u64::from(reclen) > end - pos ||
			(inr.get() != 0 && reclen < dirsiz(namelen))
		{
			log::error!(
				"Header::parse(): corrupt record at {pos}: inr={inr}, reclen={reclen}, \
				 namelen={namelen}"
			);
			return Err(err!(EIO));
		}
		// The name of an unused entry (d_ino 0) may be stale.
		let namelen = if inr.get() == 0 { 0 } else { namelen };
		let mut name = [0u8; UFS_MAXNAMELEN + 1];
		file.read(&mut name[0..namelen.into()])?;

		// skip remaining bytes of record, if any
		let off = reclen - (namelen as u16) - 8;
		file.seek_relative(off as i64)?;

		log::trace!(
			"Header::read(): {{ inr={inr}, reclen={reclen}, namelen={namelen}, name=\"{}\", kind={kind} }}",
			name[0..namelen.into()].escape_ascii()
		);

		Ok(Some(Self {
			inr,
			reclen,
			dtype: kind,
			namelen,
			name,
		}))
	}

	fn write<T: Read + Write + Seek>(&self, file: &mut Decoder<T>) -> IoResult<()> {
		let kind = self.dtype;
		log::trace!(
			"Header::write(inr={}, reclen={}, namelen={}, name=\"{}\", kind={kind})",
			self.inr,
			self.reclen,
			self.namelen,
			self.name().escape_ascii()
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

	/// The file type from d_type; `None` for whiteouts, `DT_UNKNOWN` and invalid values.
	fn kind(&self) -> Option<InodeType> {
		match self.dtype {
			DT_FIFO => Some(InodeType::NamedPipe),
			DT_CHR => Some(InodeType::CharDevice),
			DT_DIR => Some(InodeType::Directory),
			DT_BLK => Some(InodeType::BlockDevice),
			DT_REG => Some(InodeType::RegularFile),
			DT_LNK => Some(InodeType::Symlink),
			DT_SOCK => Some(InodeType::Socket),
			_ => None,
		}
	}

	fn name(&self) -> &[u8] {
		&self.name[0..self.namelen.into()]
	}

	fn minlen(&self) -> u16 {
		dirsiz(self.namelen)
	}
}

fn dtype_of(kind: InodeType) -> u8 {
	match kind {
		InodeType::NamedPipe => DT_FIFO,
		InodeType::CharDevice => DT_CHR,
		InodeType::Directory => DT_DIR,
		InodeType::BlockDevice => DT_BLK,
		InodeType::RegularFile => DT_REG,
		InodeType::Symlink => DT_LNK,
		InodeType::Socket => DT_SOCK,
	}
}

/// FreeBSD's DIRECTSIZ: header, name and NUL, rounded up to 4 bytes.
fn dirsiz(namelen: u8) -> u16 {
	(8 + namelen as u16 + 1 + 3) & !3
}

/// List the entries of one chunk. A corrupt record gives `EIO` if `strict`; otherwise
/// the rest of the chunk is skipped, as FreeBSD does on a read-only mount.
fn readdir_block<T>(
	inr: InodeNum,
	block: &[u8],
	config: Config,
	strict: bool,
	mut f: impl FnMut(&[u8], InodeNum, InodeType) -> Option<T>,
	mut kind_of: impl FnMut(InodeNum) -> IoResult<InodeType>,
) -> IoResult<Option<T>> {
	let end = block.len() as u64;
	let mut file = Decoder::new(Cursor::new(block), config);

	loop {
		let hdr = match Header::parse(&mut file, end) {
			Ok(Some(hdr)) => hdr,
			Ok(None) => break,
			Err(e) if strict => return Err(e),
			Err(_) => {
				log::error!("readdir_block({inr}): skipping the rest of a corrupt chunk");
				break;
			}
		};
		if hdr.inr.get() == 0 {
			continue;
		}

		let kind = match hdr.kind() {
			Some(kind) => kind,
			None if hdr.dtype == DT_WHT => {
				log::warn!(
					"readdir_block({inr}): encountered a whiteout entry: \"{}\"",
					hdr.name().escape_ascii()
				);
				continue;
			}
			// DT_UNKNOWN, or a d_type that is not defined: the inode knows.
			None => {
				match kind_of(hdr.inr) {
					Ok(kind) => kind,
					Err(e) if strict => return Err(e),
					Err(e) => {
						log::error!(
							"readdir_block({inr}): skipping \"{}\": {e}",
							hdr.name().escape_ascii()
						);
						continue;
					}
				}
			}
		};

		let res = f(hdr.name(), hdr.inr, kind);
		if res.is_some() {
			return Ok(res);
		}
	}

	Ok(None)
}

fn newlink_block(block: &mut [u8], mut entry: Header, config: Config) -> IoResult<bool> {
	let end = block.len() as u64;
	let mut file = Decoder::new(Cursor::new(block), config);

	loop {
		let pos = file.pos()?;
		let Some(mut hdr) = Header::parse(&mut file, end)? else {
			break;
		};
		if hdr.inr.get() == 0 {
			if hdr.reclen < entry.minlen() {
				continue;
			}
			entry.reclen = hdr.reclen;
			file.seek(pos)?;
			entry.write(&mut file)?;
			return Ok(true);
		}
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
		return Ok(true);
	}

	Ok(false)
}

fn unlink_block(
	dinr: InodeNum,
	block: &mut [u8],
	name: &[u8],
	config: Config,
) -> IoResult<Option<(InodeNum, bool)>> {
	let end = block.len() as u64;
	let mut file = Decoder::new(Cursor::new(block), config);
	let mut prevpos = 0;

	loop {
		let pos = file.pos()?;
		let Some(hdr) = Header::parse(&mut file, end)? else {
			break;
		};

		if hdr.inr.get() == 0 || hdr.name() != name {
			prevpos = pos;
			continue;
		}

		let shown = name.escape_ascii();
		log::trace!("unlink_block({dinr}, \"{shown}\"): pos={pos}, hdr={hdr:?}");

		let has;
		if pos == 0 {
			match Header::parse(&mut file, end)? {
				Some(next) => {
					log::trace!("unlink_block({dinr}, \"{shown}\"): next={next:?}");
					let new = Header {
						reclen: hdr.reclen + next.reclen,
						..next
					};
					file.seek(pos)?;
					new.write(&mut file)?;
					has = true;
				}
				_ => {
					log::trace!("unlink_block({dinr}, \"{shown}\"): no next");
					has = false;
				}
			}
		} else {
			file.seek(prevpos)?;
			match Header::parse(&mut file, end)? {
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
		return Ok(Some((hdr.inr, has)));
	}

	Ok(None)
}

fn newdir(dinr: InodeNum, inr: InodeNum, config: Config) -> IoResult<[u8; DIRBLKSIZE]> {
	let mut block = [0u8; DIRBLKSIZE];
	let mut file = Decoder::new(Cursor::new(&mut block as &mut [u8]), config);

	let h_self = Header::new(inr, InodeType::Directory, b".");
	let mut h_parent = Header::new(dinr, InodeType::Directory, b"..");
	h_parent.reclen = (DIRBLKSIZE as u16) - h_self.reclen;
	h_self.write(&mut file)?;
	h_parent.write(&mut file)?;

	Ok(block)
}

impl<R: Backend> Ufs<R> {
	/// Find a file named `name` in the directory referenced by `pinr`.
	///
	/// `name` is compared as `as_encoded_bytes()`. On non-Unix targets that is
	/// UTF-8 for valid Unicode; use [`Self::dir_lookup_bytes`] for raw names.
	pub fn dir_lookup(&mut self, pinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
		self.dir_lookup_bytes(pinr, name.as_encoded_bytes())
	}

	/// Find a file named `name` (raw on-disk bytes) in the directory referenced by `pinr`.
	pub fn dir_lookup_bytes(&mut self, pinr: InodeNum, name: &[u8]) -> IoResult<InodeNum> {
		log::trace!("dir_lookup_bytes({pinr}, \"{}\");", name.escape_ascii());
		self.dir_iter_bytes(
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

	/// Iterate through a directory referenced by `inr`, and call `f` for each entry.
	#[cfg(unix)]
	pub fn dir_iter<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		use std::os::unix::ffi::OsStrExt;
		self.dir_iter_bytes(inr, |name, inr, kind| f(OsStr::from_bytes(name), inr, kind))
	}

	/// Iterate through a directory referenced by `inr`, and call `f` for each entry.
	/// Names are passed as raw on-disk bytes, which need not be valid UTF-8.
	pub fn dir_iter_bytes<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&[u8], InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let ino = self.read_inode(inr)?;
		ino.assert_dir()?;
		self.dir_check_size(inr, &ino)?;
		let strict = self.write_enabled();
		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < ino.size {
			let n = self.dir_read_chunk(inr, &ino, pos, &mut block)?;
			if let Some(x) = readdir_block(
				inr,
				&block[..n],
				self.file.config(),
				strict,
				&mut f,
				|inr| Ok(self.read_inode(inr)?.kind()),
			)? {
				return Ok(Some(x));
			}

			pos += DIRBLKSIZE as u64;
		}
		Ok(None)
	}

	/// Check that directory `inr` is no larger than the data area. The directory
	/// loops run size / DIRBLKSIZE times, whatever the block pointers say.
	fn dir_check_size(&self, inr: InodeNum, ino: &Inode) -> IoResult<()> {
		let sb = &self.superblock;
		let data = sb.dsize as u64 * sb.fsize as u64;
		if ino.size > data {
			log::error!(
				"directory {inr}: size {} exceeds the data area ({data} bytes)",
				ino.size
			);
			return Err(err!(EIO));
		}
		Ok(())
	}

	/// Read the directory chunk at `pos`. Returns its length: DIRBLKSIZE, or less for a
	/// partial chunk at the end. A hole gives `EIO`; directories have none.
	fn dir_read_chunk(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		pos: u64,
		block: &mut [u8],
	) -> IoResult<usize> {
		let bs = self.superblock.bsize as u64;
		if pos.is_multiple_of(bs) && self.inode_resolve_block(inr, ino, pos / bs)?.is_none() {
			log::error!("directory {inr}: hole at offset {pos}");
			return Err(err!(EIO));
		}
		let n = self.inode_read(inr, pos, &mut block[..DIRBLKSIZE])?;
		if n != DIRBLKSIZE {
			log::warn!("directory {inr}: partial chunk of {n} bytes at offset {pos}");
		}
		Ok(n)
	}

	pub(super) fn dir_unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
		log::trace!("dir_unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;
		self.dir_check_size(dinr, &dino)?;

		let mut block = vec![0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			if self.dir_read_chunk(dinr, &dino, pos, &mut block)? != DIRBLKSIZE {
				return Err(err!(EIO));
			}

			if let Some((inr, has)) = unlink_block(
				dinr,
				&mut block,
				name.as_encoded_bytes(),
				self.file.config(),
			)? {
				if has {
					self.inode_write(dinr, pos, &block)?;
				} else {
					let n =
						self.inode_copy_range(dinr, &dino, (pos + DIRBLKSIZE as u64).., pos..)?;
					assert_eq!(n, dino.size - pos - DIRBLKSIZE as u64);
					self.inode_truncate(dinr, dino.size - DIRBLKSIZE as u64)?;
				}
				return Ok(inr);
			}

			pos += DIRBLKSIZE as u64;
		}

		Err(err!(ENOENT))
	}

	pub(super) fn dir_newlink(
		&mut self,
		dinr: InodeNum,
		inr: InodeNum,
		name: &OsStr,
		kind: InodeType,
	) -> IoResult<()> {
		log::trace!("dir_newlink({dinr}, {inr}, {name:?}, {kind:?});");
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;
		self.dir_check_size(dinr, &dino)?;

		let mut entry = Header::new(inr, kind, name.as_encoded_bytes());

		let mut block = [0u8; DIRBLKSIZE];
		let mut pos = 0;
		while pos < dino.size {
			if self.dir_read_chunk(dinr, &dino, pos, &mut block)? != DIRBLKSIZE {
				return Err(err!(EIO));
			}

			if newlink_block(&mut block, entry, self.file.config())? {
				self.inode_write(dinr, pos, &block)?;
				return Ok(());
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
		Ok(())
	}

	pub fn unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		log::trace!("unlink({dinr}, {name:?});");
		self.assert_rw()?;
		let inr = self.dir_unlink(dinr, name)?;
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
			// User has requested to error if the destination already exists
			match self.dir_lookup(s_dinr, s_name) {
				Ok(_) => return Err(err!(EEXIST)),
				// TODO: Need raw OS error here?
				Err(e) if e.kind() == ErrorKind::NotFound => {}
				// TODO: Might want to handle not a directory etc specially.
				Err(e) => return Err(e),
			}
		} else {
			// Not sure whether the kernel ensures for us that the file no longer exists,
			// if this triggers we know we guessed wrong.
			match self.unlink(d_dinr, d_name) {
				    Ok(_) => log::warn!("rename({d_dinr}, {d_name:?} {s_dinr}, {s_name:?}, {replace}): Destination already exists, unlinked"),
					// TODO: Need raw OS error here?
					Err(e) if e.kind() == ErrorKind::NotFound => { },
					// TODO: Handle not a directory etc.
					Err(e) => return Err(e),
				}
		}

		let kind = self.inode_attr(inr)?.kind;
		// unlink decrements the refcount, if it reaches 0 the file may get removed.
		// Bump the counter before to workaround.
		self.inode_bump(inr)?;
		self.dir_newlink(d_dinr, inr, d_name, kind)?;

		self.unlink(s_dinr, s_name)?;
		Ok(inr)
	}

	pub fn rmdir(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;
		let x = self.dir_iter_bytes(inr, |name, _inr, kind| {
			if kind != InodeType::Directory || (name != b"." && name != b"..") {
				Some(name.to_vec())
			} else {
				None
			}
		})?;

		if let Some(x) = x {
			log::debug!(
				"rmdir({dinr}, \"{}\"): not empty: \"{}\"",
				name.as_encoded_bytes().escape_ascii(),
				x.escape_ascii()
			);
			return Err(err!(ENOTEMPTY));
		}

		assert_eq!(inr, self.dir_unlink(dinr, name)?);

		self.unlink(inr, OsStr::new(".."))?;
		self.unlink(inr, OsStr::new("."))?;
		self.inode_free(inr)?;
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
		check_name_is_legal(name.as_encoded_bytes(), false)?;
		let mut ino = Inode::new(kind, perm, uid, gid, self.superblock.bsize as u32);
		let inr = self.inode_alloc(&mut ino)?;
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

		let mut dino = self.read_inode(dinr)?;
		dino.nlink += 1;
		self.write_inode(dinr, &dino)?;

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
mod t {
	use super::*;

	fn inr(n: u32) -> InodeNum {
		// SAFETY: only used to build test records.
		unsafe { InodeNum::new(n) }
	}

	fn reclen_at(block: &[u8], pos: usize) -> u16 {
		u16::from_le_bytes([block[pos + 4], block[pos + 5]])
	}

	#[test]
	fn record_sizes() {
		assert_eq!(dirsiz(3), 12);
		assert_eq!(dirsiz(4), 16);
		assert_eq!(dirsiz(255), 264);

		let block = newdir(inr(2), inr(5), Config::little()).unwrap();
		assert_eq!(reclen_at(&block, 0), 12);
		assert_eq!(reclen_at(&block, 12), (DIRBLKSIZE - 12) as u16);
	}

	fn push_entry(block: &mut Vec<u8>, inr: u32, reclen: u16, dtype: u8, name: &[u8]) {
		let start = block.len();
		block.extend(inr.to_le_bytes());
		block.extend(reclen.to_le_bytes());
		block.push(dtype);
		block.push(name.len() as u8);
		block.extend(name);
		block.resize(start + reclen as usize, 0);
	}

	fn list(block: &[u8]) -> Vec<(Vec<u8>, u32)> {
		let mut names = Vec::new();
		readdir_block(
			inr(2),
			block,
			Config::little(),
			false,
			|name, inr, _kind| {
				names.push((name.to_vec(), inr.get()));
				None::<()>
			},
			|_| Ok(InodeType::RegularFile),
		)
		.unwrap();
		names
	}

	/// FreeBSD zeroes d_ino when it removes the first entry of a chunk, but keeps its
	/// d_reclen. The entries behind it must still be found.
	#[test]
	fn unused_first_entry() {
		let mut block = Vec::new();
		push_entry(&mut block, 0, 16, DT_REG, b"gone");
		block[7] = 200;
		push_entry(&mut block, 5, 16, DT_REG, b"live");
		push_entry(&mut block, 6, (DIRBLKSIZE - 32) as u16, DT_DIR, b"subdir");
		assert_eq!(
			list(&block),
			vec![(b"live".to_vec(), 5), (b"subdir".to_vec(), 6)]
		);
	}

	/// Unlinking the entry after an unused first entry merges it into that record,
	/// which keeps its d_type.
	#[test]
	fn unlink_after_unused_entry() {
		let mut block = Vec::new();
		push_entry(&mut block, 0, 16, DT_DIR, b"gone");
		push_entry(&mut block, 5, 16, DT_REG, b"live");
		push_entry(&mut block, 6, (DIRBLKSIZE - 32) as u16, DT_REG, b"other");

		let res = unlink_block(inr(2), &mut block, b"live", Config::little()).unwrap();
		assert_eq!(res.map(|(inr, has)| (inr.get(), has)), Some((5, true)));
		assert_eq!(list(&block), vec![(b"other".to_vec(), 6)]);
		assert_eq!(reclen_at(&block, 0), 32);
		assert_eq!(block[6], DT_DIR);
	}

	#[test]
	fn newlink_reuses_unused_entry() {
		let mut block = Vec::new();
		push_entry(&mut block, 0, 16, DT_REG, b"gone");
		push_entry(&mut block, 5, (DIRBLKSIZE - 16) as u16, DT_REG, b"live");

		let entry = Header::new(inr(7), InodeType::RegularFile, b"new");
		assert!(newlink_block(&mut block, entry, Config::little()).unwrap());
		assert_eq!(
			list(&block),
			vec![(b"new".to_vec(), 7), (b"live".to_vec(), 5)]
		);
	}

	/// A corrupt record ends a read-only listing of its chunk; strict reading and the
	/// write paths give EIO.
	#[test]
	fn corrupt_record() {
		let mut block = Vec::new();
		push_entry(&mut block, 5, 16, DT_REG, b"ok");
		push_entry(&mut block, 6, 13, DT_REG, b"bad");
		block.resize(DIRBLKSIZE, 0);
		assert_eq!(list(&block), vec![(b"ok".to_vec(), 5)]);

		let strict = readdir_block(
			inr(2),
			&block,
			Config::little(),
			true,
			|_, _, _| None::<()>,
			|_| Ok(InodeType::RegularFile),
		);
		assert!(strict.is_err());
		let entry = Header::new(inr(7), InodeType::RegularFile, b"new");
		assert!(newlink_block(&mut block, entry, Config::little()).is_err());
		assert!(unlink_block(inr(2), &mut block, b"none", Config::little()).is_err());
	}

	/// A record that runs past the end of its chunk is corrupt.
	#[test]
	fn record_past_chunk() {
		let mut block = Vec::new();
		push_entry(&mut block, 5, 16, DT_REG, b"ok");
		push_entry(&mut block, 6, DIRBLKSIZE as u16, DT_REG, b"long");
		block.truncate(DIRBLKSIZE);
		assert_eq!(list(&block), vec![(b"ok".to_vec(), 5)]);
	}

	/// The last chunk of a directory may be partial.
	#[test]
	fn partial_chunk() {
		let mut block = Vec::new();
		push_entry(&mut block, 5, 16, DT_REG, b"a");
		push_entry(&mut block, 6, 84, DT_REG, b"b");
		assert_eq!(list(&block), vec![(b"a".to_vec(), 5), (b"b".to_vec(), 6)]);
	}

	fn kinds(
		block: &[u8],
		strict: bool,
		resolved: Option<InodeType>,
	) -> IoResult<Vec<(Vec<u8>, InodeType)>> {
		let mut out = Vec::new();
		readdir_block(
			inr(2),
			block,
			Config::little(),
			strict,
			|name, _inr, kind| {
				out.push((name.to_vec(), kind));
				None::<()>
			},
			|_| resolved.ok_or(err!(EIO)),
		)?;
		Ok(out)
	}

	/// An unknown or undefined d_type comes from the inode; if that fails, a read-only
	/// listing skips the entry and strict reading fails.
	#[test]
	fn unknown_dtype() {
		let mut block = Vec::new();
		push_entry(&mut block, 5, 16, DT_UNKNOWN, b"what");
		push_entry(&mut block, 6, (DIRBLKSIZE - 16) as u16, 99, b"bad");

		let dir = Some(InodeType::Directory);
		let want = vec![
			(b"what".to_vec(), InodeType::Directory),
			(b"bad".to_vec(), InodeType::Directory),
		];
		assert_eq!(kinds(&block, false, dir).unwrap(), want);
		assert!(kinds(&block, false, None).unwrap().is_empty());
		assert!(kinds(&block, true, None).is_err());
	}

	/// A file system whose root directory has `size` bytes. Every direct block
	/// points at fragment 56, which holds "." and ".." in its first chunk.
	fn root_dir(size: u64) -> Ufs<Cursor<Vec<u8>>> {
		use crate::ufs::testfs::*;

		let root = InodeNum::ROOT;
		let mut img = image(&superblock());
		let chunk = newdir(root, root, Config::little()).unwrap();
		img[56 * 4096..][..DIRBLKSIZE].copy_from_slice(&chunk);
		let mut ino = Inode::new(InodeType::Directory, 0o755, 0, 0, 32768);
		ino.size = size;
		ino.data = InodeData::Blocks(InodeBlocks {
			direct:   [56; UFS_NDADDR],
			indirect: [0; 3],
		});
		put_inode(&mut img, root, &ino);
		open(img).unwrap()
	}

	fn root_names(mut ufs: Ufs<Cursor<Vec<u8>>>) -> Result<Vec<Vec<u8>>, Option<i32>> {
		let mut names = Vec::new();
		ufs.dir_iter_bytes(InodeNum::ROOT, |name, _inr, _kind| {
			names.push(name.to_vec());
			None::<()>
		})
		.map_err(|e| e.raw_os_error())?;
		Ok(names)
	}

	/// A directory larger than the data area (192 fragments of 4096 bytes) gives EIO.
	#[test]
	fn dir_size_bounds() {
		let dots = vec![b".".to_vec(), b"..".to_vec()];
		assert_eq!(
			root_names(root_dir(65536)),
			Ok([dots.clone(), dots].concat())
		);
		assert_eq!(root_names(root_dir(1 << 20)), Err(Some(libc::EIO)));
	}
}
