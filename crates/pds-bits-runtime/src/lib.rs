use std::cell::Cell;

pub mod csn1;
pub mod natives;

pub use pds_bytes_runtime::{Error, Result, stack};

pub struct Input<'i> {
	data: &'i [u8],
	base: usize,
	end: usize,
	offset: Cell<usize>,
	needed: Cell<usize>,
}
impl<'i> Input<'i> {
	pub fn new(data: &'i [u8]) -> Self {
		Self::with_len(data, data.len() * 8)
	}
	pub fn with_len(data: &'i [u8], bits: usize) -> Self {
		Self {
			data,
			base: 0,
			end: bits.min(data.len() * 8),
			offset: Cell::new(0),
			needed: Cell::new(0),
		}
	}
	pub fn offset(&self) -> usize {
		self.offset.get()
	}
	pub fn position(&self) -> usize {
		self.base + self.offset()
	}
	pub fn remaining(&self) -> usize {
		self.end.saturating_sub(self.position())
	}
	pub fn needed(&self) -> usize {
		self.needed.get()
	}
	pub fn short(&self, n: usize) -> Error {
		self.needed.set(self.offset().saturating_add(n));
		Error::NotEnoughData
	}
	pub fn expect(&self, n: usize) -> Result<()> {
		if n > self.remaining() {
			return Err(self.short(n));
		}
		Ok(())
	}
	pub fn rewind(&self, offset: usize) {
		self.offset.set(offset)
	}
	pub fn skip(&self, n: usize) -> Result<()> {
		self.expect(n)?;
		self.offset.set(self.offset() + n);
		Ok(())
	}
	pub fn read(&self, n: u32) -> Result<u64> {
		assert!(n <= 64, "{n} bits don't fit a u64");
		self.expect(n as usize)?;
		let mut at = self.position();
		let mut left = n;
		let mut out = 0u64;
		while left > 0 {
			let used = (at % 8) as u32;
			let take = (8 - used).min(left);
			let byte = u32::from(self.data[at / 8]);
			let bits = (byte >> (8 - used - take)) & ((1 << take) - 1);
			out = (out << take) | u64::from(bits);
			at += take as usize;
			left -= take;
		}
		self.offset.set(self.offset() + n as usize);
		Ok(out)
	}
	pub fn read_bit(&self) -> Result<bool> {
		Ok(self.read(1)? == 1)
	}
	pub fn take(&self, n: usize) -> Result<Input<'i>> {
		self.expect(n)?;
		let base = self.position();
		self.offset.set(self.offset() + n);
		Ok(Input {
			data: self.data,
			base,
			end: base + n,
			offset: Cell::new(0),
			needed: Cell::new(0),
		})
	}
	pub fn rest(&self) -> Input<'i> {
		let n = self.remaining();
		self.take(n).expect("the remaining bits are there")
	}
}

#[derive(Default)]
pub struct Output {
	out: Vec<u8>,
	len: usize,
}
impl Output {
	pub fn new() -> Self {
		Self::default()
	}
	pub fn len(&self) -> usize {
		self.len
	}
	pub fn is_empty(&self) -> bool {
		self.len == 0
	}
	pub fn write(&mut self, v: u64, n: u32) {
		assert!(n <= 64, "{n} bits don't fit a u64");
		let mut left = n;
		while left > 0 {
			let used = (self.len % 8) as u32;
			if used == 0 {
				self.out.push(0);
			}
			let free = 8 - used;
			let take = free.min(left);
			let bits = ((v >> (left - take)) & ((1 << take) - 1)) as u8;
			*self.out.last_mut().expect("a byte was pushed") |= bits << (free - take);
			self.len += take as usize;
			left -= take;
		}
	}
	pub fn write_bit(&mut self, bit: bool) {
		self.write(u64::from(bit), 1)
	}
	pub fn into_inner(self) -> Vec<u8> {
		self.out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_msb_first_across_octets() {
		let input = Input::new(&[0b1011_0011, 0b0101_1111, 0xff]);
		assert_eq!(input.read(3).unwrap(), 0b101);
		assert_eq!(input.read(7).unwrap(), 0b100_1101);
		assert_eq!(input.offset(), 10);
		assert!(!input.read_bit().unwrap());
		assert_eq!(input.read(13).unwrap(), 0b1_1111_1111_1111);
		assert_eq!(input.remaining(), 0);
	}

	#[test]
	fn reads_whole_words() {
		let bytes = 0x0123_4567_89ab_cdefu64.to_be_bytes();
		let input = Input::new(&bytes);
		assert_eq!(input.read(64).unwrap(), 0x0123_4567_89ab_cdef);
		let input = Input::new(&[0xff; 9]);
		input.read(4).unwrap();
		assert_eq!(input.read(64).unwrap(), u64::MAX);
	}

	#[test]
	fn writes_what_it_reads() {
		let mut out = Output::new();
		out.write(0b101, 3);
		out.write(0b100_1101, 7);
		out.write_bit(false);
		out.write(0x1fff, 13);
		out.write(0x0123_4567_89ab_cdef, 64);
		assert_eq!(out.len(), 88);
		let bytes = out.into_inner();
		assert_eq!(&bytes[..3], &[0b1011_0011, 0b0101_1111, 0xff]);
		let input = Input::new(&bytes);
		input.skip(24).unwrap();
		assert_eq!(input.read(64).unwrap(), 0x0123_4567_89ab_cdef);
	}

	#[test]
	fn shortfalls_record_how_many_bits_the_input_needs() {
		let input = Input::new(&[0xff]);
		input.read(3).unwrap();
		assert!(matches!(input.read(6), Err(Error::NotEnoughData)));
		assert_eq!(input.needed(), 9);
		assert_eq!(input.offset(), 3);
		assert!(matches!(input.expect(40), Err(Error::NotEnoughData)));
		assert_eq!(input.needed(), 43);
	}

	#[test]
	fn taken_inputs_keep_their_absolute_position() {
		let input = Input::new(&[0b0000_0110, 0b1000_0000]);
		input.skip(5).unwrap();
		let sub = input.take(4).unwrap();
		assert_eq!(input.offset(), 9);
		assert_eq!(sub.position(), 5);
		assert_eq!(sub.read(4).unwrap(), 0b1101);
		assert!(matches!(sub.read_bit(), Err(Error::NotEnoughData)));
		assert_eq!(input.rest().remaining(), 7);
		assert_eq!(input.remaining(), 0);
	}
}
