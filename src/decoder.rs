use std::io::{BufReader, Error, ErrorKind, Read, Result, Seek, SeekFrom};

use bincode::{
	config::{Configuration, Fixint, LittleEndian, NoLimit},
	Decode,
};

pub struct Decoder<T> {
	inner:  BufReader<T>,
	config: Configuration<LittleEndian, Fixint, NoLimit>,
}

impl<T: Read> Decoder<T> {
	pub fn new(inner: T) -> Self {
		Self {
			inner:  BufReader::with_capacity(4096, inner),
			config: bincode::config::standard()
				.with_fixed_int_encoding()
				.with_little_endian(),
		}
	}

	pub fn decode<X: Decode>(&mut self) -> Result<X> {
		bincode::decode_from_reader(&mut self.inner, self.config)
			.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to decode"))
	}

	pub fn read(&mut self, buf: &mut [u8]) -> Result<()> {
		self.inner.read_exact(buf)
	}
}

impl<T: Read + Seek> Decoder<T> {
	pub fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> Result<()> {
		self.seek(pos)?;
		self.read(buf)
	}

	pub fn decode_at<X: Decode>(&mut self, pos: u64) -> Result<X> {
		self.seek(pos)?;
		self.decode()
	}

	pub fn seek(&mut self, pos: u64) -> Result<()> {
		self.inner.seek(SeekFrom::Start(pos))?;
		Ok(())
	}

	pub fn seek_relative(&mut self, off: i64) -> Result<()> {
		self.inner.seek_relative(off)
	}
}
