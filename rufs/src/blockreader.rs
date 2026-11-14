use std::{
	fs::File,
	io::{self, BufRead, Read, Result as IoResult, Seek, SeekFrom, Write},
	path::Path,
};

pub trait Backend: Read + Write + Seek {}

impl<T: Read + Write + Seek> Backend for T {}

/// Wrap an error of the backend. The caller keeps its kind and cause, but its errno
/// (such as ENODATA for a medium error on Linux) is not taken for one of ours.
fn backend_err(e: io::Error) -> io::Error {
	io::Error::new(e.kind(), e)
}

/// Block-level Abstraction Layer.
///
/// `BlockReader` maps random access reads and writes onto block operations.
pub struct BlockReader<T: Backend> {
	inner: T,
	block: Vec<u8>,
	idx:   usize,
	dirty: bool,
	rw:    bool,
}

impl BlockReader<File> {
	pub fn open(path: &Path, rw: bool) -> IoResult<Self> {
		let file = File::options().read(true).write(rw).open(path)?;
		#[cfg(unix)]
		let bs = {
			use std::os::unix::fs::MetadataExt;
			file.metadata()?.blksize() as usize
		};
		// No st_blksize here; 4096 is the common page and sector size.
		#[cfg(not(unix))]
		let bs = 4096;
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
		}
	}

	pub fn write_enabled(&self) -> bool {
		self.rw
	}

	fn refill(&mut self) -> IoResult<()> {
		if self.dirty {
			panic!("Cannot refill dirty BlockReader");
		}
		self.block.fill(0u8);
		let mut num = 0;
		while num < self.block.len() {
			match self
				.inner
				.read(&mut self.block[num..])
				.map_err(backend_err)?
			{
				0 => break,
				n => num += n,
			}
		}
		if num < self.block.len() {
			log::error!("BlockReader::refill(): num={num}, eof?");
		}
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

		self.inner
			.seek_relative(-(self.block.len() as i64))
			.map_err(backend_err)?;
		let mut num = 0;
		while num < self.block.len() {
			match self.inner.write(&self.block[num..]).map_err(backend_err)? {
				0 => break,
				n => num += n,
			}
		}
		if num < self.block.len() {
			let pos = self.inner.stream_position().map_err(backend_err)?;
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
				let real = self
					.inner
					.seek(SeekFrom::Start(pos / bs * bs))
					.map_err(backend_err)?;
				let rem = pos - real;
				assert!(rem < bs);

				self.refill()?;
				self.idx = rem as usize;

				Ok(real + rem)
			}
			SeekFrom::Current(offset) => {
				let real = self.inner.stream_position().map_err(backend_err)?;
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

	const FSIZE: u64 = 8 << 20;

	fn harness(rw: bool) -> BlockReader<File> {
		let f = tempfile::NamedTempFile::new().unwrap();
		f.as_file().set_len(FSIZE).unwrap();
		let br = BlockReader::open(f.path(), rw).unwrap();
		let bs = br.blksize();
		assert!(
			FSIZE > 2 * bs as u64,
			"(FSIZE < 2 * bs) failed; FISZE = {FSIZE}, bs = {bs}"
		);
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

			br.seek_relative(0).unwrap();
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

			br.seek_relative(-1).unwrap();
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

			let e = br.seek_relative(-2 * initial as i64).unwrap_err();
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

			br.seek_relative(1).unwrap();
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

			br.seek_relative(bs as i64).unwrap();
			assert_eq!(
				real_pos + idx + bs as u64,
				br.inner.stream_position().unwrap() + br.idx as u64
			);
		}
	}

	struct Failing;

	impl Read for Failing {
		fn read(&mut self, _buf: &mut [u8]) -> IoResult<usize> {
			Err(io::Error::from_raw_os_error(61))
		}
	}

	impl Write for Failing {
		fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
			Ok(buf.len())
		}

		fn flush(&mut self) -> IoResult<()> {
			Ok(())
		}
	}

	impl Seek for Failing {
		fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
			Ok(0)
		}
	}

	/// A backend error keeps its kind and cause, but not its errno.
	#[test]
	fn backend_error_is_wrapped() {
		let mut br = BlockReader::new(Failing, 512, false);
		let e = br.read(&mut [0u8; 4]).unwrap_err();
		let cause = io::Error::from_raw_os_error(61);
		assert_eq!(e.raw_os_error(), None);
		assert_eq!(e.kind(), cause.kind());
		assert_eq!(e.get_ref().unwrap().to_string(), cause.to_string());
	}
}
