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
}
