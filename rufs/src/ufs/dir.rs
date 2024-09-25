use super::*;
use crate::{err, InodeNum};

fn readdir_block<T>(
	inr: InodeNum,
	block: &[u8],
	config: Config,
	mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
) -> IoResult<Option<T>> {
	let mut name = [0u8; UFS_MAXNAMELEN + 1];
	let file = Cursor::new(block);
	let mut file = Decoder::new(file, config);

	loop {
		let Ok(ino) = file.decode::<InodeNum>() else {
			break;
		};
		if ino.get() == 0 {
			break;
		}

		let reclen: u16 = file.decode()?;
		let kind: u8 = file.decode()?;
		let namelen: u8 = file.decode()?;
		let name = &mut name[0..namelen.into()];
		file.read(name)?;

		// skip remaining bytes of record, if any
		let off = reclen - (namelen as u16) - 8;
		file.seek_relative(off as i64)?;

		let name = unsafe { OsStr::from_encoded_bytes_unchecked(name) };
		let kind = match kind {
			DT_FIFO => InodeType::NamedPipe,
			DT_CHR => InodeType::CharDevice,
			DT_DIR => InodeType::Directory,
			DT_BLK => InodeType::BlockDevice,
			DT_REG => InodeType::RegularFile,
			DT_LNK => InodeType::Symlink,
			DT_SOCK => InodeType::Socket,
			DT_WHT => {
				log::warn!("readdir_block({inr}): encountered a whiteout entry: {name:?}");
				continue;
			}
			DT_UNKNOWN => todo!("DT_UNKNOWN: {ino}"),
			_ => panic!("invalid filetype: {kind}"),
		};
		let res = f(name, ino, kind);
		if res.is_some() {
			return Ok(res);
		}
	}

	Ok(None)
}

impl<R: Read + Seek> Ufs<R> {
	pub fn dir_lookup(&mut self, pinr: InodeNum, name: &OsStr) -> IoResult<InodeNum> {
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

	pub fn dir_iter<T>(
		&mut self,
		inr: InodeNum,
		mut f: impl FnMut(&OsStr, InodeNum, InodeType) -> Option<T>,
	) -> IoResult<Option<T>> {
		let ino = self.read_inode(inr)?;
		let mut block = vec![0u8; self.superblock.bsize as usize];
		let frag = self.superblock.frag as u64;

		for blkidx in 0..(ino.blocks / frag) {
			let size = self.inode_read_block(inr, &ino, blkidx, &mut block)?;

			let x = readdir_block(inr, &block[0..size], self.file.config(), &mut f)?;
			if x.is_some() {
				return Ok(x);
			}
		}
		Ok(None)
	}
}
