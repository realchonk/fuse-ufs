use std::io::{BufRead, Write};

use super::*;
use crate::{err, InodeNum};

#[derive(Debug, Clone, Copy)]
struct Header {
	inr:     InodeNum,
	reclen:  u16,
	kind:    Option<InodeType>,
	namelen: u8,
	name:    [u8; UFS_MAXNAMELEN + 1],
}

impl Header {
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
}

fn readdir_block<T>(
	inr: InodeNum,
	block: &[u8],
	config: Config,
	mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut file = Decoder::new(Cursor::new(block), config);

	loop {
		let Ok(Some(hdr)) = Header::parse(&mut file) else {
			break;
		};

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

fn unlink_block(
	dinr: InodeNum,
	block: &mut [u8],
	name: &OsStr,
	config: Config,
) -> IoResult<Option<InodeNum>> {
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

		if pos == 0 {
			match Header::parse(&mut file)? {
				Some(next) => {
					let new = Header {
						reclen: hdr.reclen + next.reclen,
						..next
					};
					file.seek(pos)?;
					new.write(&mut file)?;
				}
				None => {
					todo!("unlink_block({dinr}): unlinking the only entry in a directory block")
				}
			}
		} else {
			file.seek(prevpos)?;
			match Header::parse(&mut file)? {
				Some(mut prev) => {
					prev.reclen += hdr.reclen;
					file.seek(prevpos)?;
					prev.write(&mut file)?;
				}
				None => {
					log::error!(
						"unlink_block({dinr}): previous entry is bad: prevpos={prevpos}, pos={pos}"
					);
					return Err(err!(EIO));
				}
			}
		}
		return Ok(Some(hdr.inr));
	}

	Ok(None)
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

	/// Iterate through a directory referenced by `inr`, and call `f` for each entry.
	pub fn dir_iter<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		ino.assert_dir()?;
		let ino = self.read_inode(inr)?;
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

	pub(super) fn dir_unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
		self.assert_rw()?;
		let dino = self.read_inode(dinr)?;
		dino.assert_dir()?;

		let mut block = vec![0u8; self.superblock.bsize as usize];
		let frag = self.superblock.frag as u64;

		for blkidx in 0..(dino.blocks / frag) {
			let size = self.inode_read_block(dinr, &dino, blkidx, &mut block)?;

			if let Some(inr) = unlink_block(dinr, &mut block[0..size], name, self.file.config())? {
				self.inode_write_block(dinr, &dino, blkidx, &block)?;
				return Ok(inr);
			}
		}

		Err(err!(ENOENT))
	}

	pub fn unlink(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		self.assert_rw()?;
		let inr = self.dir_unlink(dinr, name)?;
		self.inode_free(inr)?;
		Ok(())
	}

	pub fn rmdir(&mut self, dinr: InodeNum, name: &OsStr) -> IoResult<()> {
		self.assert_rw()?;
		let inr = self.dir_lookup(dinr, name)?;
		let x = self.dir_iter(inr, |name, _inr, kind| {
			if kind != InodeType::Directory || name != "." || name != ".." {
				Some(())
			} else {
				None
			}
		})?;

		if x.is_some() {
			return Err(err!(ENOTEMPTY));
		}

		assert_eq!(inr, self.dir_unlink(dinr, name)?);

		self.unlink(inr, OsStr::new(".."))?;
		self.unlink(inr, OsStr::new("."))?;
		self.inode_free(inr)?;
		Ok(())
	}
}
