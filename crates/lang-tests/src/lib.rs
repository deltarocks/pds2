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

pub mod generated;
pub mod pds_native;
pub mod pds {
	pub use pds_bytes_runtime::*;
}

#[cfg(test)]
mod tests {
	use crate::generated;
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
		use generated::counted as t;
		let v = roundtrip(t::deserialize, t::serialize, &[3, 7, 8]).unwrap();
		assert_eq!(v.data, vec![7, 8]);
		assert!(roundtrip(t::deserialize, t::serialize, &[0]).is_none());
	}

	#[test]
	fn types_live_beside_their_codec_modules() {
		let v: generated::Counted = generated::sample_counted::value();
		assert!(format!("{v:?}").starts_with("Counted {"), "{v:?}");
		let _: generated::counted::Ty = v;
	}

	#[test]
	fn explicit_cast_widens_before_multiplying() {
		use generated::widened as t;
		let mut bytes = vec![200];
		bytes.extend((0..800).map(|i| i as u8));
		let v = roundtrip(t::deserialize, t::serialize, &bytes).unwrap();
		assert_eq!(v.items.len(), 400);
	}

	#[test]
	fn functions_inline_like_written_expressions() {
		use generated::scaled as t;
		let mut bytes = vec![200];
		bytes.extend((0..800).map(|i| i as u8));
		let v = roundtrip(t::deserialize, t::serialize, &bytes).unwrap();
		assert_eq!(v.items.len(), 400);
		let encoded = generated::encoded_scaled::value();
		let v = roundtrip(t::deserialize, t::serialize, &encoded).unwrap();
		assert_eq!((v.n, v.items), (4, vec![1, 2, 3, 0xffff, 5, 6, 7, 8]));
	}

	#[test]
	fn typed_terminator_reads_whole_values() {
		let bytes = [0, 1, 0xff, 0, 0xff, 0xff];
		let inferred = roundtrip(
			generated::words::deserialize,
			generated::words::serialize,
			&bytes,
		)
		.unwrap();
		let bound = roundtrip(
			generated::bound_words::deserialize,
			generated::bound_words::serialize,
			&bytes,
		)
		.unwrap();
		assert_eq!(inferred, vec![1, 0xff00]);
		assert_eq!(bound, inferred);
	}

	#[test]
	fn typed_def_arguments_take_constants_and_fields() {
		use generated::choices as t;
		roundtrip(t::deserialize, t::serialize, &[1, 5, 0, 7]).unwrap();
		roundtrip(t::deserialize, t::serialize, &[2, 0, 9, 0, 7]).unwrap();
	}

	#[test]
	fn def_arguments_reach_nested_fields_elements_arms_and_defs() {
		use generated::Choice;
		use generated::arguments as t;
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
		use generated::tagged as t;
		for bytes in [&[1, 0][..], &[2, 9, 5, 1, 3, 4], &[7, 0], &[2, 9, 5, 1, 6]] {
			roundtrip(t::deserialize, t::serialize, bytes).unwrap();
		}
		assert!(roundtrip(t::deserialize, t::serialize, &[8, 0]).is_none());
	}

	#[test]
	fn recursion_fails_cleanly_past_stack_budget() {
		use crate::pds::{Error, stack};
		use generated::tree as t;
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
		use generated::view;
		let owned = generated::sample_blob::value();
		let bytes = generated::encoded_blob::value();
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
		use generated::view;
		let blob = generated::sample_blob::value();
		assert_eq!(view::blob::view(&blob).name.as_ptr(), blob.name.as_ptr());

		let mut tree = generated::tree::deserialize(&mut Input::new(&[2, 0, 1, 0])).unwrap();
		let children = tree.children.as_ptr();
		let taken = view::tree::into_view(&mut tree);
		assert_eq!(taken.children.as_ptr(), children);
		assert_eq!(taken.children.len(), 2);
		assert!(tree.children.is_empty());
	}

	#[test]
	fn views_keep_unborrowable_fields_owned() {
		use generated::view;
		let sample = generated::sample_blob::value();
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
		use generated::sample_choices as choices;
		use generated::sample_counted as counted;
		use generated::sample_default as default;
		use generated::sample_flags as flags;
		use generated::sample_tagged as tagged;
		use generated::sample_words as words;
		encodes(
			counted::value(),
			counted::deserialize,
			counted::serialize,
			&[3, 7, 8],
		);
		encodes(
			words::value(),
			words::deserialize,
			words::serialize,
			&[0, 1, 0xff, 0, 0xff, 0xff],
		);
		encodes(
			choices::value(),
			choices::deserialize,
			choices::serialize,
			&[2, 0, 9, 0, 7],
		);
		encodes(
			tagged::value(),
			tagged::deserialize,
			tagged::serialize,
			&[2, 9, 5, 1, 3, 4],
		);
		encodes(
			default::value(),
			default::deserialize,
			default::serialize,
			&[7, 0],
		);
		let mut out = Output::new();
		flags::serialize(&flags::value(), &mut out).unwrap();
		assert_eq!(out.into_inner(), [1, 0, 1]);
	}

	macro_rules! encoded_like_runtime {
		($sample:ident, $encoded:ident) => {{
			let value = generated::$sample::value();
			let mut out = Output::new();
			generated::$sample::serialize(&value, &mut out).unwrap();
			assert_eq!(
				out.into_inner(),
				generated::$encoded::value(),
				stringify!($encoded)
			);
			let back = roundtrip(
				generated::$sample::deserialize,
				generated::$sample::serialize,
				&generated::$encoded::value(),
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
		encoded_like_runtime!(sample_mapped, encoded_mapped);
		let widened = generated::encoded_widened::value();
		let v = roundtrip(
			generated::widened::deserialize,
			generated::widened::serialize,
			&widened,
		)
		.unwrap();
		assert_eq!(v.items, vec![1, 2, 3, 0xffff]);
	}

	#[test]
	fn compile_time_deserialization_matches_values() {
		assert_eq!(
			generated::decoded_counted::value(),
			generated::sample_counted::value()
		);
		assert_eq!(
			generated::decoded_words::value(),
			generated::sample_words::value()
		);
		assert_eq!(
			generated::decoded_choices::value(),
			generated::sample_choices::value()
		);
		assert_eq!(
			generated::decoded_tagged::value(),
			generated::sample_tagged::value()
		);
		assert_eq!(
			generated::decoded_default::value(),
			generated::sample_default::value()
		);
		assert_eq!(
			generated::decoded_packed::value(),
			generated::sample_packed::value()
		);
		assert_eq!(
			generated::decoded_flagset::value(),
			generated::sample_flagset::value()
		);
		assert_eq!(
			generated::decoded_blob::value(),
			generated::sample_blob::value()
		);
		assert_eq!(
			generated::decoded_holders::value(),
			generated::sample_holders::value()
		);
		assert_eq!(
			generated::decoded_loop::value(),
			generated::sample_loop::value()
		);
		assert_eq!(
			generated::decoded_top_bits::value(),
			generated::sample_top_bits::value()
		);
		assert_eq!(
			generated::decoded_arguments::value(),
			generated::sample_arguments::value()
		);
		assert_eq!(
			generated::decoded_mapped::value(),
			generated::sample_mapped::value()
		);
	}

	#[test]
	fn sized_lengths_are_checked_on_serialize() {
		let mut blob = generated::sample_blob::value();
		blob.fixed.pop();
		assert!(generated::blob::serialize(&blob, &mut Output::new()).is_err());
		let short = generated::FixedArm::Four(vec![1, 2, 3]);
		assert!(generated::fixed_arm::serialize(&short, &mut Output::new()).is_err());
		let exact = generated::FixedArm::Four(vec![1, 2, 3, 4]);
		assert!(generated::fixed_arm::serialize(&exact, &mut Output::new()).is_ok());
	}

	#[test]
	fn map_rejects_duplicate_keys() {
		use generated::mapped as t;
		let bytes = [2, 1, b'a', 0, 1, 1, b'a', 0, 2];
		assert!(t::deserialize(&mut Input::new(&bytes)).is_err());
	}

	#[test]
	fn def_derives_extend_generated_ones() {
		fn copy_eq<T: Copy + Eq>(v: T) -> (T, T) {
			(v, v)
		}
		let (a, b) = copy_eq(generated::Kind::B(3));
		assert_eq!(a, b);
	}

	#[test]
	fn protobuf_fields_follow_their_presence() {
		use generated::branch as t;
		let encoded = generated::encoded_branch::value();
		let v = roundtrip(t::deserialize, t::serialize, &encoded).unwrap();
		assert_eq!(v, generated::sample_branch::value());
		assert_eq!(v, generated::decoded_branch::value());

		let zeros = generated::sample_zeros::value();
		let mut out = Output::new();
		t::serialize(&zeros, &mut out).unwrap();
		assert_eq!(out.into_inner(), [0x0a, 0x02, 0x08, 0x01]);
		assert_eq!(generated::encoded_zeros::value(), [0x0a, 0x02, 0x08, 0x01]);

		let negative = generated::Branch {
			weight: -0.0,
			..zeros
		};
		let mut out = Output::new();
		t::serialize(&negative, &mut out).unwrap();
		let bytes = out.into_inner();
		assert_eq!(bytes.len(), 4 + 9);
		assert!(
			t::deserialize(&mut Input::new(&bytes))
				.unwrap()
				.weight
				.is_sign_negative()
		);
	}

	#[test]
	fn protobuf_required_fields_are_checked_after_merging() {
		use generated::branch as t;
		assert!(t::deserialize(&mut Input::new(&[])).is_err());
		assert!(t::deserialize(&mut Input::new(&[0x0a, 0x02, 0x10, 0x04])).is_err());
		let split = [
			0x0a, 0x02, 0x10, 0x04, 0x0a, 0x02, 0x08, 0x03, 0x12, 0x02, 0x10, 0x07, 0x12, 0x02,
			0x08, 0x09, 0x38, 0x05, 0x32, 0x02, 0x10, 0x01, 0x32, 0x02, 0x08, 0x02,
		];
		let v = t::deserialize(&mut Input::new(&split)).unwrap();
		assert_eq!(v, generated::merged_branch::value());
		assert_eq!(v, generated::decoded_split::value());
		let restarted = [
			0x0a, 0x02, 0x08, 0x01, 0x32, 0x02, 0x08, 0x02, 0x38, 0x05, 0x32, 0x02, 0x10, 0x01,
		];
		assert!(t::deserialize(&mut Input::new(&restarted)).is_err());
		let mut merged = generated::merged_branch::value();
		t::deserialize_extend(&mut Input::new(&[0x0a, 0x02, 0x10, 0x08]), &mut merged).unwrap();
		assert_eq!((merged.root.id, merged.root.tag), (3, 8));
		let switched = [
			0x0a, 0x02, 0x08, 0x01, 0x42, 0x02, 0x08, 0x01, 0x32, 0x02, 0x08, 0x02, 0x42, 0x02,
			0x10, 0x05,
		];
		let v = t::deserialize(&mut Input::new(&switched)).unwrap();
		assert_eq!(v, generated::switched_branch::value());
		assert_eq!(v, generated::decoded_switch::value());
	}
}
