use std::{
	fs::File,
	io::{BufRead, Read, Result as IoResult, Seek, SeekFrom},
	os::unix::fs::MetadataExt,
	path::Path,
};

pub struct BlockReader {
	file:  File,
	block: Vec<u8>,
	idx:   usize,
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
			SeekFrom::Current(_) => todo!("SeekFrom::Current()"),
			SeekFrom::End(_) => todo!("SeekFrom::End()"),
		}
	}
}
