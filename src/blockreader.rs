use std::{
	fs::File,
	io::{self, BufRead, Read, Result as IoResult, Seek, SeekFrom},
	os::unix::fs::MetadataExt,
	path::Path,
};

pub struct BlockReader {
	file: File,
	block: Vec<u8>,
	idx: usize,
}

impl BlockReader {
	pub fn open(path: &Path) -> IoResult<Self> {
		let file = File::options().read(true).write(false).open(path)?;

		let bs = file.metadata()?.blksize() as usize;
		let block = vec![0u8; bs];
		Ok(Self {
			file,
			block,
			idx: bs,
		})
	}

	fn refill(&mut self) -> IoResult<()> {
		self.file.read_exact(&mut self.block)?;
		self.idx = 0;
		Ok(())
	}

	fn buffered(&self) -> usize {
		self.block.len() - self.idx
	}

	fn refill_if_empty(&mut self) -> IoResult<()> {
		if self.buffered() == 0 {
			self.refill()?;
		}
		Ok(())
	}

	pub fn blksize(&self) -> usize {
		self.block.len()
	}
}

impl Read for BlockReader {
	fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
		self.refill_if_empty()?;
		let num = buf.len().min(self.buffered());
		let buf = &mut buf[0..num];
		buf.copy_from_slice(&self.block[self.idx..(self.idx + num)]);
		self.idx += num;
		Ok(num)
	}
}

impl BufRead for BlockReader {
	fn fill_buf(&mut self) -> IoResult<&[u8]> {
		self.refill_if_empty()?;
		Ok(&self.block[self.idx..])
	}

	fn consume(&mut self, amt: usize) {
		assert!(amt <= self.buffered());
		self.idx += amt;
	}
}

impl Seek for BlockReader {
	fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
		let bs = self.blksize() as u64;
		match pos {
			SeekFrom::Start(pos) => {
				let real = self.file.seek(SeekFrom::Start(pos / bs * bs))?;
				let rem = pos - real;
				assert!(rem < bs);

				self.refill()?;
				self.idx = rem as usize;

				Ok(real + rem)
			}
			SeekFrom::Current(offset) => {
				let real = self.file.stream_position()?;
				let cur = real - self.block.len() as u64 + self.idx as u64;
				let newidx = offset + self.idx as i64;
				if newidx >= 0 && newidx < self.blksize() as i64 {
					// The data is already buffered; just adjust the pointer
					self.idx = newidx as usize;
					Ok(real - self.block.len() as u64 + newidx as u64)
				} else if cur as i64 + offset < 0 {
					Err(io::Error::from_raw_os_error(libc::EINVAL))
				} else {
					self.seek(SeekFrom::Start((cur as i64 + offset) as u64))
				}
			}
			SeekFrom::End(_) => todo!("SeekFrom::End()"),
		}
	}
}

#[cfg(test)]
mod t {
	use super::*;

	mod seek {
		use super::*;

		const FSIZE: u64 = 1 << 20;

		fn harness() -> BlockReader {
			let f = tempfile::NamedTempFile::new().unwrap();
			f.as_file().set_len(FSIZE).unwrap();
			let br = BlockReader::open(f.path()).unwrap();
			let bs = br.blksize();
			assert!(FSIZE > 2 * bs as u64);
			br
		}

		/// Seeking to SeekFrom::Current(0) should refill the internal buffer but otherwise be a
		/// no-op.
		#[test]
		#[allow(clippy::seek_from_current)] // That's the whole point of the test
		fn current_0() {
			let mut br = harness();
			let bs = br.blksize();
			let pos = bs + (bs >> 2);
			br.seek(SeekFrom::Start(pos as u64)).unwrap();
			let idx = br.idx;
			let real_pos = br.file.stream_position().unwrap();

			br.seek(SeekFrom::Current(0)).unwrap();
			assert_eq!(real_pos, br.file.stream_position().unwrap());
			assert_eq!(idx, br.idx);
		}

		/// Seek to a negative offset from current
		#[test]
		fn current_neg() {
			let mut br = harness();
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.file.stream_position().unwrap();

			br.seek(SeekFrom::Current(-1)).unwrap();
			assert_eq!(
				real_pos + idx - 1,
				br.file.stream_position().unwrap() + br.idx as u64
			);
		}

		/// Seek to a negative absolute offset using SeekFrom::Current
		#[test]
		fn current_neg_neg() {
			let mut br = harness();
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();

			let e = br.seek(SeekFrom::Current(-2 * initial as i64)).unwrap_err();
			assert_eq!(libc::EINVAL, e.raw_os_error().unwrap());
		}

		/// Seek to a small positive offset from current, within the current block
		#[test]
		fn current_pos_incr() {
			let mut br = harness();
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.file.stream_position().unwrap();

			br.seek(SeekFrom::Current(1)).unwrap();
			assert_eq!(
				real_pos + idx + 1,
				br.file.stream_position().unwrap() + br.idx as u64
			);
		}

		/// Seek to a large positive offset from current
		#[test]
		fn current_pos_large() {
			let mut br = harness();
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.file.stream_position().unwrap();

			br.seek(SeekFrom::Current(bs as i64)).unwrap();
			assert_eq!(
				real_pos + idx + bs as u64,
				br.file.stream_position().unwrap() + br.idx as u64
			);
		}
	}
}
