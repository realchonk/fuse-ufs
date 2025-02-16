use std::{
	fs::File, io::{self, BufRead, Read, Result as IoResult, Seek, SeekFrom, Write}, os::unix::fs::MetadataExt, path::Path
};

pub trait Backend: Read + Write + Seek {}

impl<T: Read + Write + Seek> Backend for T {}

/// Block-level Abstraction Layer.
///
/// `BlockReader` maps random access reads and writes onto block operations.
pub struct BlockReader<T: Backend> {
	inner: T,
	block: Vec<u8>,
	idx:   usize,
	dirty: bool,
	rw:    bool,
	#[cfg(feature = "bcache")]
	cache: lru::LruCache<u64, Vec<u8>>,
}

impl BlockReader<File> {
	pub fn open(path: &Path, rw: bool) -> IoResult<Self> {
		let file = File::options().read(true).write(rw).open(path)?;
		let bs = file.metadata()?.blksize() as usize;
		Ok(BlockReader::new(file, bs, rw))
	}
}

impl<T: Backend> BlockReader<T> {
	pub fn new(inner: T, bs: usize, rw: bool) -> Self {
		let block = vec![0u8; bs];
		Self {
			inner,
			block,
			idx: bs,
			dirty: false,
			rw,
			#[cfg(feature = "bcache")]
			cache: crate::new_lru(crate::BCACHE_SIZE),
		}
	}

	pub fn write_enabled(&self) -> bool {
		self.rw
	}

	fn refill(&mut self) -> IoResult<()> {
		if self.dirty {
			panic!("Cannot refill dirty BlockReader");
		}

		#[cfg(feature = "bcache")]
		let pos = self.inner.stream_position()?;
		#[cfg(feature = "bcache")]
		if let Some(cached) = self.cache.get(&pos) {
			self.block.copy_from_slice(cached);
			self.inner.seek(SeekFrom::Current(self.block.len() as i64))?;
			self.idx = 0;
			return Ok(())
		}
		
		self.block.fill(0u8);
		let mut num = 0;
		while num < self.block.len() {
			match self.inner.read(&mut self.block[num..])? {
				0 => break,
				n => num += n,
			}
		}
		if num < self.block.len() {
			log::error!("BlockReader::refill(): num={num}, eof?");
		}
		#[cfg(feature = "bcache")]
		self.cache.push(pos, self.block.clone());
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

	/// Get the underlying block size.
	pub fn blksize(&self) -> usize {
		self.block.len()
	}
}

impl<T: Backend> Read for BlockReader<T> {
	fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
		self.refill_if_empty()?;
		let num = buf.len().min(self.buffered());
		let buf = &mut buf[0..num];
		buf.copy_from_slice(&self.block[self.idx..(self.idx + num)]);
		self.idx += num;
		Ok(num)
	}
}

impl<T: Backend> Write for BlockReader<T> {
	fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
		if !self.rw {
			panic!(
				"BUG: BlockReader::write() should never be called when the medium is not writable"
			);
		}
		self.refill_if_empty()?;
		let num = buf.len().min(self.buffered());
		self.block[self.idx..(self.idx + num)].copy_from_slice(&buf[0..num]);
		self.idx += num;
		self.dirty = true;
		self.flush()?;
		Ok(num)
	}

	fn flush(&mut self) -> IoResult<()> {
		if !self.dirty {
			return Ok(());
		}

		#[allow(unused_variables)]
		let pos = self
			.inner
			.seek(SeekFrom::Current(-(self.block.len() as i64)))?;

		#[cfg(feature = "bcache")]
		self.cache.push(pos, self.block.clone());
		
		let mut num = 0;
		while num < self.block.len() {
			match self.inner.write(&self.block[num..])? {
				0 => break,
				n => num += n,
			}
		}
		if num < self.block.len() {
			let pos = self.inner.stream_position()?;
			log::error!(
				"short write: pos={pos}, num={num}, len={}",
				self.block.len()
			);
		}
		self.dirty = false;
		Ok(())
	}
}

impl<T: Backend> BufRead for BlockReader<T> {
	fn fill_buf(&mut self) -> IoResult<&[u8]> {
		self.refill_if_empty()?;
		Ok(&self.block[self.idx..])
	}

	fn consume(&mut self, amt: usize) {
		assert!(amt <= self.buffered());
		self.idx += amt;
	}
}

impl<T: Backend> Seek for BlockReader<T> {
	fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
		let bs = self.blksize() as u64;
		match pos {
			SeekFrom::Start(pos) => {
				self.flush()?;
				let real = self.inner.seek(SeekFrom::Start(pos / bs * bs))?;
				let rem = pos - real;
				assert!(rem < bs);

				self.refill()?;
				self.idx = rem as usize;

				Ok(real + rem)
			}
			SeekFrom::Current(offset) => {
				let real = self.inner.stream_position()?;
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

	const FSIZE: u64 = 1 << 20;

	fn harness(rw: bool) -> BlockReader<File> {
		let f = tempfile::NamedTempFile::new().unwrap();
		f.as_file().set_len(FSIZE).unwrap();
		let br = BlockReader::open(f.path(), rw).unwrap();
		let bs = br.blksize();
		assert!(FSIZE > 2 * bs as u64);
		br
	}

	mod write {
		use super::*;

		#[test]
		fn simple_write() {
			let mut br = harness(true);
			let bs = br.blksize();
			let pos = bs + (bs >> 2);
			let mut buf = vec![0x55u8; bs];
			br.seek(SeekFrom::Start(pos as u64)).unwrap();
			br.write_all(&buf).unwrap();
			buf.fill(0);
			br.seek(SeekFrom::Start(pos as u64)).unwrap();
			br.read_exact(&mut buf).unwrap();
			assert_eq!(buf, vec![0x55u8; bs]);
		}
	}

	mod seek {
		use super::*;

		/// Seeking to SeekFrom::Current(0) should refill the internal buffer but otherwise be a
		/// no-op.
		#[test]
		#[allow(clippy::seek_from_current)] // That's the whole point of the test
		fn current_0() {
			let mut br = harness(false);
			let bs = br.blksize();
			let pos = bs + (bs >> 2);
			br.seek(SeekFrom::Start(pos as u64)).unwrap();
			let idx = br.idx;
			let real_pos = br.inner.stream_position().unwrap();

			br.seek(SeekFrom::Current(0)).unwrap();
			assert_eq!(real_pos, br.inner.stream_position().unwrap());
			assert_eq!(idx, br.idx);
		}

		/// Seek to a negative offset from current
		#[test]
		fn current_neg() {
			let mut br = harness(false);
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.inner.stream_position().unwrap();

			br.seek(SeekFrom::Current(-1)).unwrap();
			assert_eq!(
				real_pos + idx - 1,
				br.inner.stream_position().unwrap() + br.idx as u64
			);
		}

		/// Seek to a negative absolute offset using SeekFrom::Current
		#[test]
		fn current_neg_neg() {
			let mut br = harness(false);
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();

			let e = br.seek(SeekFrom::Current(-2 * initial as i64)).unwrap_err();
			assert_eq!(libc::EINVAL, e.raw_os_error().unwrap());
		}

		/// Seek to a small positive offset from current, within the current block
		#[test]
		fn current_pos_incr() {
			let mut br = harness(false);
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.inner.stream_position().unwrap();

			br.seek(SeekFrom::Current(1)).unwrap();
			assert_eq!(
				real_pos + idx + 1,
				br.inner.stream_position().unwrap() + br.idx as u64
			);
		}

		/// Seek to a large positive offset from current
		#[test]
		fn current_pos_large() {
			let mut br = harness(false);
			let bs = br.blksize();
			let initial = bs + (bs >> 2);
			br.seek(SeekFrom::Start(initial as u64)).unwrap();
			let idx = br.idx as u64;
			let real_pos = br.inner.stream_position().unwrap();

			br.seek(SeekFrom::Current(bs as i64)).unwrap();
			assert_eq!(
				real_pos + idx + bs as u64,
				br.inner.stream_position().unwrap() + br.idx as u64
			);
		}
	}
}
