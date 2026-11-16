use super::*;
use crate::{err, InodeNum};

impl<R: Backend> Ufs<R> {
	fn iter_xattr<T>(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		mut f: impl FnMut(&ExtattrHeader, &[u8], &[u8]) -> Option<T>,
	) -> IoResult<Option<T>> {
		if ino.extsize == 0 {
			return Ok(None);
		}

		let fs = self.superblock.fsize as u64;
		let bs = self.superblock.bsize as usize;
		let sz = ino.extsize as usize;
		let bad = |what: &str| {
			log::error!("inode {inr}: invalid extattr {what}");
			err!(EIO)
		};
		if sz > UFS_NXADDR * bs {
			return Err(bad("area size"));
		}

		let mut blocks = vec![0u8; ino.extsize as usize];
		let mut nr = 0;
		let mut blkidx = 0;

		while nr < blocks.len() {
			let bno = ino.extb[blkidx] as u64;
			if bno == 0 {
				return Err(bad("block"));
			}
			let num = bs.min(blocks.len() - nr);
			let pos = self.check_blkno(inr, bno, (num as u64).div_ceil(fs))? * fs;
			self.file.read_at(pos, &mut blocks[nr..(nr + num)])?;
			blkidx += 1;
			nr += num;
		}

		let file = Cursor::new(blocks);
		let mut file = Decoder::new(file, self.file.config());
		let mut name = [0u8; 64];
		let mut data = Vec::new();

		loop {
			let begin = file.pos()?;
			let Ok(hdr) = file.decode::<ExtattrHeader>() else {
				break;
			};
			let namelen = hdr.namelen as usize;

			if namelen == 0 {
				break;
			} else if namelen > UFS_EXTATTR_MAXNAMELEN {
				return Err(bad("name length"));
			}

			file.read(&mut name[0..namelen]).map_err(|_| bad("name"))?;
			file.align_to(8)?;
			let len = (hdr.len as u64)
				.checked_sub(file.pos()? - begin)
				.filter(|&len| len <= sz as u64)
				.ok_or_else(|| bad("length"))?;
			data.resize(len as usize, 0u8);
			file.read(&mut data).map_err(|_| bad("value"))?;
			let keep = data
				.len()
				.checked_sub(hdr.contentpadlen as usize)
				.ok_or_else(|| bad("padding"))?;
			data.truncate(keep);

			if let Some(x) = f(&hdr, &name[0..namelen], &data) {
				return Ok(Some(x));
			}
		}

		Ok(None)
	}

	fn read_xattr<T>(
		&mut self,
		inr: InodeNum,
		ino: &Inode,
		xname: &[u8],
		mut f: impl FnMut(&ExtattrHeader, &[u8]) -> T,
	) -> IoResult<T> {
		self.iter_xattr(inr, ino, |hdr, n, data| {
			let ns = hdr.namespace()?;
			if xname == ns.with_name(n) {
				Some(f(hdr, data))
			} else {
				None
			}
		})
		.and_then(|r| r.ok_or(IoError::from_raw_os_error(crate::ENOATTR)))
	}

	/// Get the size of the extended attribute area of inode `inr`.
	pub fn xattr_list_len(&mut self, inr: InodeNum) -> IoResult<u32> {
		let ino = self.read_inode(inr)?;
		Ok(ino.extsize)
	}

	/// Get the list of extended attribyte names.
	/// Each entry follows the following format:
	/// `"namespace.name\0"`
	pub fn xattr_list(&mut self, inr: InodeNum) -> IoResult<Vec<u8>> {
		let ino = self.read_inode(inr)?;
		let mut data = Vec::new();
		self.iter_xattr(inr, &ino, |hdr, name, _data| {
			let ns = hdr.namespace()?;
			data.extend(ns.with_name(name));
			data.push(0);
			None::<()>
		})?;
		Ok(data)
	}

	/// Get the size of an extended attribute.
	pub fn xattr_len(&mut self, inr: InodeNum, name: &OsStr) -> IoResult<u32> {
		let ino = self.read_inode(inr)?;
		let len = self.read_xattr(inr, &ino, name.as_encoded_bytes(), |_hdr, data| data.len())?;
		Ok(len as u32)
	}

	/// Read the value of an extended attribute.
	pub fn xattr_read(&mut self, inr: InodeNum, name: &OsStr) -> IoResult<Vec<u8>> {
		let ino = self.read_inode(inr)?;
		let data = self.read_xattr(inr, &ino, name.as_encoded_bytes(), |_hdr, data| data.into())?;
		Ok(data)
	}
}

#[cfg(test)]
mod t {
	use super::*;
	use crate::ufs::testfs::*;

	/// List the extended attributes of a file whose 8192-byte extattr area
	/// starts at fragment `bno`. Fails with the errno.
	fn list(bno: UfsDaddr) -> Result<Vec<u8>, Option<i32>> {
		let mut ino = Inode::new(InodeType::RegularFile, 0o644, 0, 0, 32768);
		ino.extsize = 8192;
		ino.extb[0] = bno;
		let mut img = image(&superblock());
		put_inode(&mut img, InodeNum::ROOT, &ino);
		let mut ufs = open(img).unwrap();
		ufs.xattr_list(InodeNum::ROOT).map_err(|e| e.raw_os_error())
	}

	/// The extattr area must end within the file system (256 fragments of 4096 bytes).
	#[test]
	fn area_past_the_end() {
		assert_eq!(list(254), Ok(Vec::new()));
		assert_eq!(list(255), Err(Some(libc::EIO)));
	}
}
