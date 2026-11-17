use anyhow::{bail, ensure, Result};

pub struct Input<'b> {
	bytes: &'b [u8],
	pos: usize,
	base: usize,
}

impl<'b> Input<'b> {
	pub fn new(bytes: &'b [u8]) -> Self {
		Self::at(bytes, 0)
	}

	pub fn at(bytes: &'b [u8], base: usize) -> Self {
		Input {
			bytes,
			pos: 0,
			base,
		}
	}

	pub fn offset(&self) -> usize {
		self.pos
	}

	pub fn position(&self) -> usize {
		self.base + self.pos
	}

	pub fn rewind(&mut self, at: usize) {
		self.pos = at;
	}

	pub fn remaining(&self) -> usize {
		self.bytes.len() - self.pos
	}

	pub fn peek(&self) -> &'b [u8] {
		&self.bytes[self.pos..]
	}

	pub fn take(&mut self, n: usize) -> Result<&'b [u8]> {
		ensure!(
			n <= self.remaining(),
			"byte {}: {n} bytes needed, {} left",
			self.position(),
			self.remaining()
		);
		let out = &self.bytes[self.pos..self.pos + n];
		self.pos += n;
		Ok(out)
	}

	pub fn byte(&mut self) -> Result<u8> {
		Ok(self.take(1)?[0])
	}

	pub fn rest(&mut self) -> &'b [u8] {
		let out = self.peek();
		self.pos = self.bytes.len();
		out
	}

	pub fn sub(&mut self, n: usize) -> Result<Input<'b>> {
		let base = self.position();
		Ok(Input::at(self.take(n)?, base))
	}

	pub fn varint(&mut self) -> Result<u64> {
		let start = self.position();
		let mut out = 0u64;
		for i in 0..10 {
			let b = self.byte()?;
			let bits = u64::from(b & 0x7f);
			ensure!(i < 9 || bits <= 1, "byte {start}: varint overflows 64 bits");
			out |= bits << (7 * i);
			if b & 0x80 == 0 {
				return Ok(out);
			}
		}
		bail!("byte {start}: varint is longer than 10 bytes")
	}
}
