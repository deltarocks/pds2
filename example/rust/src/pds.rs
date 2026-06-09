#![allow(clippy::just_underscores_and_digits, dead_code)]
use std::cell::Cell;

pub struct Input<'i> {
	data: &'i [u8],
	offset: Cell<usize>,
}
impl<'i> Input<'i> {
	pub fn new(data: &'i [u8]) -> Self {
		Self {
			data,
			offset: Cell::new(0),
		}
	}
	pub fn offset(&self) -> usize {
		self.offset.get()
	}
	pub fn peek(&self) -> Result<u8> {
		self.data
			.get(self.offset())
			.copied()
			.ok_or(Error::NotEnoughData)
	}
	pub fn read(&self) -> Result<u8> {
		let value = self.peek()?;
		self.skip();
		Ok(value)
	}
	pub fn skip(&self) {
		self.offset.set(self.offset.get() + 1)
	}
}
pub struct Output<'o> {
	out: &'o mut [u8],
	offset: usize,
}
impl<'o> Output<'o> {
	pub fn new(out: &'o mut [u8]) -> Self {
		Self { out, offset: 0 }
	}
	pub fn write(&mut self, v: &[u8]) {
		// Size is precalculated
		assert!(self.out.len() - self.offset >= v.len());
		self.out[self.offset..self.offset + v.len()].copy_from_slice(v);
		self.offset += v.len()
	}
}

#[derive(Debug)]
pub enum Error {
	NotEnoughData,
	TooLargeInteger,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub mod pds_protocol_vu32 {
	use super::*;
	pub type Ty = u32;
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		let mut out = [0u8; 5];
		let mut size = 0;
		let mut value = *_0;

		while value > 0 {
			out[size] = (value & 0x7f) as u8;
			value >>= 7;
			if value > 0 {
				out[size] |= 0x80;
			}
			size += 1;
		}
		output.write(&out[0..size]);
		Ok(())
	}
	pub fn deserialize(input: &mut Input) -> Result<Ty> {
		let mut out = 0u32;
		let mut size = 1;
		let mut shift = 0;

		loop {
			if size > 5 {
				return Err(Error::TooLargeInteger);
			}
			let b = input.read()?;

			out |= ((b & 0x7f) as u32) << shift;

			size += 1;
			shift += 7;
			if b & 0x80 == 0 {
				break;
			}
		}
		Ok(out)
	}
	pub fn size_of(_0: &Ty) -> usize {
		let mut size = 0;
		let mut value = *_0;
		while value > 0 {
			size += 1;
			value >>= 7;
		}
		size
	}
}

pub mod pds_protocol_vi32 {
	use super::*;
	pub type Ty = i32;
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		let v = *_0;
		let v = if v < 0 {
			((-v as u32) << 1) | 1
		} else {
			(v as u32) << 1
		};
		pds_protocol_vu32::serialize(&v, output)
	}
	pub fn deserialize(input: &mut Input) -> Result<Ty> {
		let v = pds_protocol_vu32::deserialize(input)?;
		let v = if v & 1 == 0 {
			(v >> 1) as i32
		} else {
			-((v >> 1) as i32)
		};
		Ok(v)
	}
}

pub mod pds_protocol_u8 {
	use super::*;
	pub type Ty = u8;
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		output.write(&[*_0]);
		Ok(())
	}
	pub fn deserialize(input: &mut Input) -> Result<Ty> {
		input.read()
	}
	pub fn size_of(_0: &Ty) -> usize {
		1
	}
}

pub mod pds_protocol_myarray {
	use super::*;
	pub struct Args<'i> {
		pub length: &'i u32,
	}
	pub type Ty = Vec<u8>;
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		output.write(_0);
		Ok(())
	}
	pub fn deserialize(input: &mut Input, args: Args) -> Result<Ty> {
		let mut out = Vec::with_capacity(*args.length as usize);
		for _ in 0..*args.length {
			out.push(input.read()?);
		}
		Ok(out)
	}
	pub fn size_of(_0: &Ty) -> usize {
		_0.len()
	}
	pub fn virtual_size(_0: &Ty) -> u32 {
		_0.len() as u32
	}
}

pub mod pds_protocol_mymap {
	use std::collections::HashMap;

	use super::*;
	pub type Ty<K, V> = HashMap<K, V>;
}

pub mod pds_parray {
	use super::*;
	pub type Ty = ();
	pub struct Args<'i> {
		pub size: &'i usize,
	}
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		todo!()
	}
	pub fn deserialize(input: &mut Input, args: Args<'_>) -> Result<Ty> {
		todo!()
	}
	pub fn virtual_size(_0: &Ty) -> usize {
		0
	}
	pub fn size_of(_0: &Ty) -> usize {
		todo!()
	}
}
pub mod pds_usize {
	use super::*;
	pub type Ty = usize;
	pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {
		todo!()
	}
	pub fn deserialize(input: &mut Input) -> Result<Ty> {
		todo!()
	}
	pub fn size_of(_0: &Ty) -> usize {
		todo!()
	}
}
