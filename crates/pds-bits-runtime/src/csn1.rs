use crate::{Error, Input, Output, Result};

pub const PAD: u8 = 0x2b;

pub fn pad_bit(position: usize) -> bool {
	(PAD >> (7 - position % 8)) & 1 == 1
}

pub fn symbol(position: usize, s: u8) -> bool {
	match s {
		b'0' => false,
		b'1' => true,
		b'L' => pad_bit(position),
		b'H' => !pad_bit(position),
		_ => unreachable!("{:?} is not a CSN.1 bit", s as char),
	}
}

fn prefix(input: &Input, pattern: &[u8]) -> std::result::Result<bool, usize> {
	for (i, &s) in pattern.iter().enumerate() {
		if input.remaining() == 0 {
			return Err(pattern.len() - i);
		}
		let at = input.position();
		let bit = input.read_bit().map_err(|_| pattern.len() - i)?;
		if bit != symbol(at, s) {
			return Ok(false);
		}
	}
	Ok(true)
}

pub fn choose(input: &Input, patterns: &[&[u8]]) -> Result<usize> {
	let at = input.offset();
	let mut missing = 0;
	for (i, pattern) in patterns.iter().enumerate() {
		match prefix(input, pattern) {
			Ok(true) => return Ok(i),
			Ok(false) => {}
			Err(n) => missing = missing.max(input.offset() - at + n),
		}
		input.rewind(at);
	}
	if missing > 0 {
		Err(input.short(missing))
	} else {
		Err(Error::InvalidValue)
	}
}

pub fn accept(input: &Input, pattern: &[u8]) -> Result<bool> {
	match choose(input, &[pattern]) {
		Ok(_) => Ok(true),
		Err(Error::InvalidValue) => Ok(false),
		Err(e) => Err(e),
	}
}

pub fn write_pattern(output: &mut Output, pattern: &[u8]) {
	for &s in pattern {
		let bit = symbol(output.len(), s);
		output.write_bit(bit);
	}
}

pub fn write_zeros(output: &mut Output, mut n: usize) {
	while n > 0 {
		let take = n.min(64);
		output.write(0, take as u32);
		n -= take;
	}
}

pub fn write_padding(output: &mut Output) {
	write_padding_to(output, output.len().next_multiple_of(8));
}

pub fn write_padding_to(output: &mut Output, end: usize) {
	while output.len() < end {
		let bit = pad_bit(output.len());
		output.write_bit(bit);
	}
}

pub fn skip_padding(input: &Input) {
	input.rest();
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn l_and_h_follow_the_padding_pattern() {
		let mut out = Output::new();
		write_pattern(&mut out, b"LLLLLLLL");
		write_pattern(&mut out, b"HHHHHHHH");
		write_pattern(&mut out, b"0L1H");
		assert_eq!(out.into_inner(), [0x2b, 0xd4, 0b0011_0000]);
		let input = Input::new(&[0x2b, 0x2b]);
		input.skip(3).unwrap();
		assert!(accept(&input, b"LLL").unwrap());
		assert!(!accept(&input, b"H").unwrap());
		assert_eq!(input.offset(), 6);
	}

	#[test]
	fn padding_fills_the_octet_with_l() {
		let mut out = Output::new();
		out.write(0b111, 3);
		write_padding(&mut out);
		write_padding(&mut out);
		assert_eq!(out.into_inner(), [0b1110_1011]);
	}

	#[test]
	fn choose_tells_mismatch_from_shortage() {
		let input = Input::new(&[0b0110_0000]);
		assert_eq!(choose(&input, &[b"1", b"00", b"01"]).unwrap(), 2);
		assert_eq!(input.offset(), 2);
		assert!(matches!(choose(&input, &[b"0", b"11"]), Err(Error::InvalidValue)));
		assert_eq!(input.offset(), 2);
		let input = Input::with_len(&[0b1100_0000], 2);
		assert!(matches!(choose(&input, &[b"0", b"111"]), Err(Error::NotEnoughData)));
		assert_eq!(input.needed(), 3);
		assert_eq!(input.offset(), 0);
	}
}
