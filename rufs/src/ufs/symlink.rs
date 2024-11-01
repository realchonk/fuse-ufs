use super::*;
use crate::InodeNum;

impl<R: Read + Seek> Ufs<R> {
	/// Read the contents of a symbolic link.
	#[doc(alias = "readlink")]
	pub fn symlink_read(&mut self, inr: InodeNum) -> IoResult<Vec<u8>> {
		let ino = self.read_inode(inr)?;

		if ino.mode & S_IFMT != S_IFLNK {
			return Err(IoError::from_raw_os_error(libc::EINVAL));
		}

		match &ino.data {
			InodeData::Shortlink(link) => {
				assert_eq!(ino.blocks, 0);
				let len = ino.size as usize;
				Ok(link[0..len].to_vec())
			}
			InodeData::Blocks { .. } => {
				// TODO: this has to be tested for other configurations, such as 4K/4K
				assert!(ino.blocks <= 8);

				let len = ino.size as usize;
				let mut buf = vec![0u8; self.superblock.bsize as usize];
				self.inode_read_block(inr, &ino, 0, &mut buf)?;
				buf.resize(len, 0u8);
				Ok(buf)
			}
		}
	}
}
