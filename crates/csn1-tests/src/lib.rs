#![allow(
	non_camel_case_types,
	non_snake_case,
	dead_code,
	unused_imports,
	unused_variables,
	unused_parens,
	unused_mut,
	unreachable_patterns,
	clippy::all
)]

pub mod gen;
pub mod pds {
	pub use pds_bits_runtime::*;
}
pub mod pds_native {
	pub use pds_bits_runtime::natives::pds_void;
}

#[cfg(test)]
mod tests {
	use crate::gen::rr::{self, GprsIndicator, SelectionParameter};
	use crate::gen::{synthetic, synthetic_fixture, synthetic_roundtrip, si3_fixture, LhFlag};
	use crate::pds::{Error, Input, Output, Result};

	fn roundtrip<T: PartialEq + std::fmt::Debug>(
		de: fn(&mut Input) -> Result<T>,
		ser: fn(&T, &mut Output) -> Result<()>,
		size: fn(&T) -> usize,
		bytes: &[u8],
	) -> T {
		let mut input = Input::new(bytes);
		let v = de(&mut input).unwrap();
		assert_eq!(input.remaining(), 0, "{bytes:02x?} not fully consumed");
		let mut out = Output::new();
		ser(&v, &mut out).unwrap();
		assert_eq!(out.len(), size(&v));
		assert_eq!(out.into_inner(), bytes);
		v
	}

	#[test]
	fn si3_rest_octets_of_padding_only_are_all_absent() {
		let v = roundtrip(
			rr::si3_rest_octets::deserialize,
			rr::si3_rest_octets::serialize,
			rr::si3_rest_octets::size_of,
			&[0x2b; 4],
		);
		assert_eq!(v.selection_parameter, None);
		assert_eq!(v.power_offset, None);
		assert_eq!(v.si2ter_indicator, LhFlag::L);
		assert_eq!(v.gprs_indicator, None);
		assert_eq!(v.si2quater_position, None);
	}

	#[test]
	fn si3_rest_octets_read_h_against_the_padding_pattern() {
		let v = roundtrip(
			rr::si3_rest_octets::deserialize,
			rr::si3_rest_octets::serialize,
			rr::si3_rest_octets::size_of,
			&[0x8a, 0x5f, 0x63, 0xab],
		);
		assert_eq!(
			v.selection_parameter,
			Some(SelectionParameter {
				cbq: false,
				cell_reselect_offset: 10,
				temporary_offset: 2,
				penalty_time: 31,
			})
		);
		assert_eq!(v.si2ter_indicator, LhFlag::H);
		assert_eq!(v.early_classmark_sending_control, LhFlag::L);
		assert_eq!(
			v.gprs_indicator,
			Some(GprsIndicator {
				ra_colour: 3,
				si13_position: true,
			})
		);
		assert_eq!(v, si3_fixture::value());
	}

	#[test]
	fn si3_rest_octets_ignore_what_follows_in_their_octets() {
		let mut input = Input::new(&[0x2b, 0x2b, 0xff, 0xff, 0x42]);
		let v = rr::si3_rest_octets::deserialize(&mut input).unwrap();
		assert_eq!(v.gprs_indicator, None);
		assert_eq!(input.offset(), 32);
	}

	#[test]
	fn synthetic_message_writes_every_construct() {
		let bytes = [0xaa, 0xf2, 0xc7, 0xe6, 0x17, 0xfe, 0xab];
		let v = roundtrip(synthetic::deserialize, synthetic::serialize, synthetic::size_of, &bytes);
		assert_eq!(v, synthetic_fixture::value());
		assert_eq!(v, synthetic_roundtrip::value());
		assert_eq!(v.deltas, [-16, 15, -1]);
		assert_eq!(v.items, [3, 15]);
	}

	#[test]
	fn values_too_wide_for_their_bits_are_rejected() {
		let mut v = synthetic_fixture::value();
		v.items[0] = 16;
		assert!(matches!(synthetic::serialize(&v, &mut Output::new()), Err(Error::InvalidValue)));
		let mut v = synthetic_fixture::value();
		v.deltas.push(16);
		assert!(matches!(synthetic::serialize(&v, &mut Output::new()), Err(Error::InvalidValue)));
	}

	#[test]
	fn fixed_bits_are_checked_and_shortfalls_are_hinted() {
		let mut input = Input::new(&[0b0010_0000]);
		assert!(matches!(synthetic::deserialize(&mut input), Err(Error::InvalidValue)));
		let mut input = Input::new(&[0xaa, 0xf2, 0xc7]);
		assert!(matches!(synthetic::deserialize(&mut input), Err(Error::NotEnoughData)));
		assert_eq!(input.needed(), 27);
	}
}
