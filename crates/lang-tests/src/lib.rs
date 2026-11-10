#![allow(
	non_camel_case_types,
	non_snake_case,
	dead_code,
	unused_imports,
	unused_variables,
	unused_parens,
	unreachable_patterns,
	clippy::all
)]

pub mod gen;
pub mod pds_native;
pub mod pds {
	pub use pds_runtime::*;
}

#[cfg(test)]
mod tests {
	use crate::gen::*;
	use crate::pds::{Input, Output, Result};

	fn roundtrip<T>(
		de: fn(&mut Input) -> Result<T>,
		ser: fn(&T, &mut Output) -> Result<()>,
		bytes: &[u8],
	) -> Option<T> {
		let mut input = Input::new(bytes);
		let v = de(&mut input).ok()?;
		assert_eq!(input.offset(), bytes.len(), "{bytes:?} not fully consumed");
		let mut out = Output::new();
		ser(&v, &mut out).unwrap();
		assert_eq!(out.into_inner(), bytes);
		Some(v)
	}

	#[test]
	fn arithmetic_in_virtual_fields_and_counts() {
		use pds_Counted as t;
		let v = roundtrip(t::deserialize, t::serialize, &[3, 7, 8]).unwrap();
		assert_eq!(v.data, vec![7, 8]);
		assert!(roundtrip(t::deserialize, t::serialize, &[0]).is_none());
	}

	#[test]
	fn explicit_cast_widens_before_multiplying() {
		use pds_Widened as t;
		let mut bytes = vec![200];
		bytes.extend((0..800).map(|i| i as u8));
		let v = roundtrip(t::deserialize, t::serialize, &bytes).unwrap();
		assert_eq!(v.items.len(), 400);
	}

	#[test]
	fn typed_terminator_reads_whole_values() {
		let bytes = [0, 1, 0xff, 0, 0xff, 0xff];
		let inferred = roundtrip(pds_Words::deserialize, pds_Words::serialize, &bytes).unwrap();
		let bound =
			roundtrip(pds_BoundWords::deserialize, pds_BoundWords::serialize, &bytes).unwrap();
		assert_eq!(inferred, vec![1, 0xff00]);
		assert_eq!(bound, inferred);
	}

	#[test]
	fn typed_def_arguments_take_constants_and_fields() {
		use pds_Choices as t;
		roundtrip(t::deserialize, t::serialize, &[1, 5, 0, 7]).unwrap();
		roundtrip(t::deserialize, t::serialize, &[2, 0, 9, 0, 7]).unwrap();
	}

	#[test]
	fn union_tags_compare_by_construction_or_cast() {
		use pds_Tagged as t;
		for bytes in [&[1, 0][..], &[2, 9, 5, 1, 3, 4], &[7, 0], &[2, 9, 5, 1, 6]] {
			roundtrip(t::deserialize, t::serialize, bytes).unwrap();
		}
		assert!(roundtrip(t::deserialize, t::serialize, &[8, 0]).is_none());
	}

	fn encodes<T: PartialEq + std::fmt::Debug>(
		value: T,
		de: fn(&mut Input) -> Result<T>,
		ser: fn(&T, &mut Output) -> Result<()>,
		bytes: &[u8],
	) {
		let mut out = Output::new();
		ser(&value, &mut out).unwrap();
		assert_eq!(out.into_inner(), bytes);
		assert_eq!(roundtrip(de, ser, bytes), Some(value));
	}

	#[test]
	fn constants_construct_schema_values() {
		use pds_sample_choices as choices;
		use pds_sample_counted as counted;
		use pds_sample_default as default;
		use pds_sample_flags as flags;
		use pds_sample_tagged as tagged;
		use pds_sample_words as words;
		encodes(counted::value(), counted::deserialize, counted::serialize, &[3, 7, 8]);
		encodes(
			words::value(),
			words::deserialize,
			words::serialize,
			&[0, 1, 0xff, 0, 0xff, 0xff],
		);
		encodes(choices::value(), choices::deserialize, choices::serialize, &[2, 0, 9, 0, 7]);
		encodes(tagged::value(), tagged::deserialize, tagged::serialize, &[2, 9, 5, 1, 3, 4]);
		encodes(default::value(), default::deserialize, default::serialize, &[7, 0]);
		let mut out = Output::new();
		flags::serialize(&flags::value(), &mut out).unwrap();
		assert_eq!(out.into_inner(), [1, 0, 1]);
	}

	macro_rules! encoded_like_runtime {
		($sample:ident, $encoded:ident) => {{
			let value = $sample::value();
			let mut out = Output::new();
			$sample::serialize(&value, &mut out).unwrap();
			assert_eq!(out.into_inner(), $encoded::value(), stringify!($encoded));
			let back = roundtrip($sample::deserialize, $sample::serialize, &$encoded::value());
			assert_eq!(back, Some(value), stringify!($encoded));
		}};
	}

	#[test]
	fn compile_time_serialization_matches_generated_serializers() {
		encoded_like_runtime!(pds_sample_counted, pds_encoded_counted);
		encoded_like_runtime!(pds_sample_words, pds_encoded_words);
		encoded_like_runtime!(pds_sample_choices, pds_encoded_choices);
		encoded_like_runtime!(pds_sample_tagged, pds_encoded_tagged);
		encoded_like_runtime!(pds_sample_default, pds_encoded_default);
		encoded_like_runtime!(pds_sample_packed, pds_encoded_packed);
		encoded_like_runtime!(pds_sample_flagset, pds_encoded_flagset);
		encoded_like_runtime!(pds_sample_blob, pds_encoded_blob);
		encoded_like_runtime!(pds_sample_holders, pds_encoded_holders);
		encoded_like_runtime!(pds_sample_loop, pds_encoded_loop);
		encoded_like_runtime!(pds_sample_top_bits, pds_encoded_top_bits);
		let widened = pds_encoded_widened::value();
		let v = roundtrip(pds_Widened::deserialize, pds_Widened::serialize, &widened).unwrap();
		assert_eq!(v.items, vec![1, 2, 3, 0xffff]);
	}
}
