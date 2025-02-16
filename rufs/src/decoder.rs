use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};

use bincode::{
	config::{BigEndian, Configuration, Fixint, LittleEndian, NoLimit},
	Decode,
	Encode,
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

	fn decode<T: Decode>(&self, rdr: &mut impl Read) -> Result<T> {
		match self {
			Self::Little(cfg) => bincode::decode_from_std_read(rdr, *cfg),
			Self::Big(cfg) => bincode::decode_from_std_read(rdr, *cfg),
		}
		.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to decode"))
	}

	fn encode(&self, wtr: &mut impl Write, x: &impl Encode) -> Result<()> {
		match self {
			Self::Little(cfg) => bincode::encode_into_std_write(x, wtr, *cfg),
			Self::Big(cfg) => bincode::encode_into_std_write(x, wtr, *cfg),
		}
		.map(|_| ())
		.map_err(|_| Error::new(ErrorKind::InvalidInput, "failed to encode"))
	}
}

pub struct Decoder<T: Read> {
	inner:  T,
	config: Config,
}

impl<T: Read> Decoder<T> {
	pub fn new(inner: T, config: Config) -> Self {
		Self { inner, config }
	}

	pub fn inner(&self) -> &T {
		&self.inner
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

impl<T: Read + Write> Decoder<T> {
	pub fn write(&mut self, buf: &[u8]) -> Result<()> {
		self.inner.write_all(buf)
	}

	pub fn encode(&mut self, x: &impl Encode) -> Result<()> {
		self.config.encode(&mut self.inner, x)
	}

	pub fn fill(&mut self, b: u8, num: usize) -> Result<()> {
		for _ in 0..num {
			self.write(&[b])?;
		}
		Ok(())
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

	pub fn align_to(&mut self, align: u64) -> Result<()> {
		assert_eq!(align.count_ones(), 1);
		let pos = self.inner.stream_position()?;
		let new_pos = (pos + align - 1) & !(align - 1);
		self.seek(new_pos)
	}

	pub fn pos(&mut self) -> Result<u64> {
		self.inner.stream_position()
	}

	pub fn seek_relative(&mut self, off: i64) -> Result<()> {
		self.inner.seek(SeekFrom::Current(off))?;
		Ok(())
	}
}

impl<T: Read + Write + Seek> Decoder<T> {
	pub fn write_at(&mut self, pos: u64, buf: &[u8]) -> Result<()> {
		self.seek(pos)?;
		self.write(buf)
	}

	pub fn encode_at(&mut self, pos: u64, x: &impl Encode) -> Result<()> {
		self.seek(pos)?;
		self.encode(x)
	}

	pub fn fill_at(&mut self, pos: u64, b: u8, num: usize) -> Result<()> {
		self.seek(pos)?;
		self.fill(b, num)
	}
}
