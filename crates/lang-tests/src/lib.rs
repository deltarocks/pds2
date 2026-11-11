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
	use crate::gen;
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
		use gen::counted as t;
		let v = roundtrip(t::deserialize, t::serialize, &[3, 7, 8]).unwrap();
		assert_eq!(v.data, vec![7, 8]);
		assert!(roundtrip(t::deserialize, t::serialize, &[0]).is_none());
	}

	#[test]
	fn types_live_beside_their_codec_modules() {
		let v: gen::Counted = gen::sample_counted::value();
		assert!(format!("{v:?}").starts_with("Counted {"), "{v:?}");
		let _: gen::counted::Ty = v;
	}

	#[test]
	fn explicit_cast_widens_before_multiplying() {
		use gen::widened as t;
		let mut bytes = vec![200];
		bytes.extend((0..800).map(|i| i as u8));
		let v = roundtrip(t::deserialize, t::serialize, &bytes).unwrap();
		assert_eq!(v.items.len(), 400);
	}

	#[test]
	fn typed_terminator_reads_whole_values() {
		let bytes = [0, 1, 0xff, 0, 0xff, 0xff];
		let inferred = roundtrip(gen::words::deserialize, gen::words::serialize, &bytes).unwrap();
		let bound =
			roundtrip(gen::bound_words::deserialize, gen::bound_words::serialize, &bytes).unwrap();
		assert_eq!(inferred, vec![1, 0xff00]);
		assert_eq!(bound, inferred);
	}

	#[test]
	fn typed_def_arguments_take_constants_and_fields() {
		use gen::choices as t;
		roundtrip(t::deserialize, t::serialize, &[1, 5, 0, 7]).unwrap();
		roundtrip(t::deserialize, t::serialize, &[2, 0, 9, 0, 7]).unwrap();
	}

	#[test]
	fn def_arguments_reach_nested_fields_elements_arms_and_defs() {
		use gen::arguments as t;
		use gen::Choice;
		let one = [1, 5, 6, 7, 0, 8, 9, 10, 11, 12, 13, 14];
		let two = [
			2, 0, 5, 0, 6, 0, 7, 0, 0, 8, 0, 9, 0, 10, 0, 11, 0, 12, 0, 13, 14,
		];
		let default_arm = [1, 5, 6, 7, 3, 9, 10, 11, 12, 13, 14];
		let unknown_tag = [3, 5, 6, 7, 0, 8, 9, 10, 11, 12, 13, 14];
		let v = roundtrip(t::deserialize, t::serialize, &one).unwrap();
		assert_eq!(v.nested.forwarded.choice, Choice::One(10));
		assert_eq!(v.nested.deep.inner.choice, Choice::One(9));
		let v = roundtrip(t::deserialize, t::serialize, &two).unwrap();
		assert_eq!(v.nested.forwarded.choice, Choice::Two(10));
		assert_eq!(v.nested.wrapped.choice, Choice::Two(13));
		let choices = [Choice::Two(11), Choice::Two(12)];
		assert_eq!(v.nested.choices, choices);
		roundtrip(t::deserialize, t::serialize, &default_arm).unwrap();
		assert!(roundtrip(t::deserialize, t::serialize, &unknown_tag).is_none());
	}

	#[test]
	fn union_tags_compare_by_construction_or_cast() {
		use gen::tagged as t;
		for bytes in [&[1, 0][..], &[2, 9, 5, 1, 3, 4], &[7, 0], &[2, 9, 5, 1, 6]] {
			roundtrip(t::deserialize, t::serialize, bytes).unwrap();
		}
		assert!(roundtrip(t::deserialize, t::serialize, &[8, 0]).is_none());
	}

	#[test]
	fn recursion_fails_cleanly_past_stack_budget() {
		use crate::pds::{stack, Error};
		use gen::tree as t;
		let v = roundtrip(t::deserialize, t::serialize, &[2, 0, 1, 0]).unwrap();
		assert_eq!(v.children[1].children.len(), 1);
		let mut deep = vec![1u8; 1 << 20];
		deep.push(0);
		let r = stack::budget(256 << 10, || t::deserialize(&mut Input::new(&deep)));
		assert!(matches!(r, Err(Error::TooDeep)));
		let shallow = [1, 1, 1, 0];
		let r = stack::budget(256 << 10, || t::deserialize(&mut Input::new(&shallow)));
		assert!(r.is_ok());
	}

	#[test]
	fn views_borrow_from_input_and_from_owned_values() {
		use gen::view;
		let owned = gen::sample_blob::value();
		let bytes = gen::encoded_blob::value();
		let range = bytes.as_ptr_range();
		let parsed = view::blob::deserialize(&mut Input::new(&bytes)).unwrap();
		assert_eq!(parsed.name, "pds");
		assert!(range.contains(&parsed.name.as_ptr()));
		assert!(range.contains(&parsed.raw.as_ptr()));
		let adapted = view::blob::view(&owned);
		assert_eq!(adapted.name.as_ptr(), owned.name.as_ptr());
		assert_eq!(adapted, parsed);
		let mut out = Output::new();
		view::blob::serialize(&adapted, &mut out).unwrap();
		assert_eq!(out.into_inner(), bytes);
		assert_eq!(view::blob::size_of(&parsed), bytes.len());
	}

	#[test]
	fn into_view_borrows_leaves_and_takes_the_rest() {
		use gen::view;
		let blob = gen::sample_blob::value();
		assert_eq!(view::blob::view(&blob).name.as_ptr(), blob.name.as_ptr());

		let mut tree = gen::tree::deserialize(&mut Input::new(&[2, 0, 1, 0])).unwrap();
		let children = tree.children.as_ptr();
		let taken = view::tree::into_view(&mut tree);
		assert_eq!(taken.children.as_ptr(), children);
		assert_eq!(taken.children.len(), 2);
		assert!(tree.children.is_empty());
	}

	#[test]
	fn views_keep_unborrowable_fields_owned() {
		use gen::view;
		let sample = gen::sample_blob::value();
		let blob = view::blob::view(&sample);
		let n: u8 = blob.n;
		assert_eq!(n, 3);
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
		use gen::sample_choices as choices;
		use gen::sample_counted as counted;
		use gen::sample_default as default;
		use gen::sample_flags as flags;
		use gen::sample_tagged as tagged;
		use gen::sample_words as words;
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
			let value = gen::$sample::value();
			let mut out = Output::new();
			gen::$sample::serialize(&value, &mut out).unwrap();
			assert_eq!(out.into_inner(), gen::$encoded::value(), stringify!($encoded));
			let back = roundtrip(
				gen::$sample::deserialize,
				gen::$sample::serialize,
				&gen::$encoded::value(),
			);
			assert_eq!(back, Some(value), stringify!($encoded));
		}};
	}

	#[test]
	fn compile_time_serialization_matches_generated_serializers() {
		encoded_like_runtime!(sample_counted, encoded_counted);
		encoded_like_runtime!(sample_words, encoded_words);
		encoded_like_runtime!(sample_choices, encoded_choices);
		encoded_like_runtime!(sample_tagged, encoded_tagged);
		encoded_like_runtime!(sample_default, encoded_default);
		encoded_like_runtime!(sample_packed, encoded_packed);
		encoded_like_runtime!(sample_flagset, encoded_flagset);
		encoded_like_runtime!(sample_blob, encoded_blob);
		encoded_like_runtime!(sample_holders, encoded_holders);
		encoded_like_runtime!(sample_loop, encoded_loop);
		encoded_like_runtime!(sample_top_bits, encoded_top_bits);
		encoded_like_runtime!(sample_arguments, encoded_arguments);
		let widened = gen::encoded_widened::value();
		let v = roundtrip(gen::widened::deserialize, gen::widened::serialize, &widened).unwrap();
		assert_eq!(v.items, vec![1, 2, 3, 0xffff]);
	}

	#[test]
	fn compile_time_deserialization_matches_values() {
		assert_eq!(gen::decoded_counted::value(), gen::sample_counted::value());
		assert_eq!(gen::decoded_words::value(), gen::sample_words::value());
		assert_eq!(gen::decoded_choices::value(), gen::sample_choices::value());
		assert_eq!(gen::decoded_tagged::value(), gen::sample_tagged::value());
		assert_eq!(gen::decoded_default::value(), gen::sample_default::value());
		assert_eq!(gen::decoded_packed::value(), gen::sample_packed::value());
		assert_eq!(gen::decoded_flagset::value(), gen::sample_flagset::value());
		assert_eq!(gen::decoded_blob::value(), gen::sample_blob::value());
		assert_eq!(gen::decoded_holders::value(), gen::sample_holders::value());
		assert_eq!(gen::decoded_loop::value(), gen::sample_loop::value());
		assert_eq!(gen::decoded_top_bits::value(), gen::sample_top_bits::value());
		assert_eq!(gen::decoded_arguments::value(), gen::sample_arguments::value());
	}
}
