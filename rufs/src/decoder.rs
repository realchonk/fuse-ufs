use std::io::{BufRead, Error, ErrorKind, Result, Seek, SeekFrom};

use bincode::{
	config::{BigEndian, Configuration, Fixint, LittleEndian, NoLimit},
	Decode,
};

#[derive(Clone, Copy)]
pub enum Config {
	Little(Configuration<LittleEndian, Fixint, NoLimit>),
	Big(Configuration<BigEndian, Fixint, NoLimit>),
}

impl Config {
	pub const fn little() -> Self {
		let cfg = bincode::config::standard()
			.with_fixed_int_encoding()
			.with_little_endian();
		Self::Little(cfg)
	}

	pub const fn big() -> Self {
		let cfg = bincode::config::standard()
			.with_fixed_int_encoding()
			.with_big_endian();
		Self::Big(cfg)
	}

	fn decode<T: Decode>(&self, rdr: &mut dyn BufRead) -> Result<T> {
		match self {
			Self::Little(cfg) => bincode::decode_from_reader(rdr, *cfg),
			Self::Big(cfg) => bincode::decode_from_reader(rdr, *cfg),
		}
		.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to decode"))
	}
}

pub struct Decoder<T: BufRead> {
	inner:  T,
	config: Config,
}

impl<T: BufRead> Decoder<T> {
	pub fn new(inner: T, config: Config) -> Self {
		Self {
			inner,
			config,
		}
	}

	pub fn inner(&self) -> &T {
		&self.inner
	}

	pub fn inner_mut(&mut self) -> &mut T {
		&mut self.inner
	}

	pub fn decode<X: Decode>(&mut self) -> Result<X> {
		self.config.decode(&mut self.inner)
	}

	pub fn read(&mut self, buf: &mut [u8]) -> Result<()> {
		self.inner.read_exact(buf)
	}

	pub fn config(&self) -> Config {
		self.config
	}
}

impl<T: BufRead + Seek> Decoder<T> {
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

	pub fn align_to(&mut self, align: u64) -> Result<()> {
		assert_eq!(align.count_ones(), 1);
		let pos = self.inner.stream_position()?;
		let new_pos = (pos + align - 1) & !(align - 1);
		self.seek(new_pos)
	}

	pub fn pos(&mut self) -> Result<u64> {
		self.inner.stream_position()
	}
}
