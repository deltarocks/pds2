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
	pub use pds_runtime::*;
}
pub mod pds_native {
	pub use pds_runtime::natives::pds_void;
	pub use pds_runtime::protobuf::natives::*;
}

pub use gen::envoy::config::cluster::v3::cluster;

#[cfg(test)]
mod tests {
	use crate::cluster;
	use crate::gen::envoy::config::cluster::v3::cluster as c;
	use crate::gen::google::protobuf::value::Kind;
	use crate::pds::{Input, Output};

	fn fixture() -> Vec<u8> {
		std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/cluster.bin"))
			.expect("run `just envoy` to encode the fixture with protoc")
	}

	#[test]
	fn cluster_roundtrips_protoc_encoding() {
		let bytes = fixture();
		let mut input = Input::new(&bytes);
		let decoded = cluster::deserialize(&mut input).unwrap();
		assert_eq!(input.remaining(), 0);

		assert_eq!(decoded.name.as_deref(), Some("backend"));
		let endpoints = &decoded.load_assignment.as_ref().unwrap().endpoints[0];
		assert_eq!(endpoints.lb_endpoints.len(), 2);
		assert_eq!(endpoints.priority, Some(1));
		let tags = &decoded.metadata.as_ref().unwrap().filter_metadata[0];
		assert_eq!(tags.key.as_deref(), Some("envoy.lb"));
		let weight = tags.value.as_ref().unwrap().fields[1].value.as_ref().unwrap();
		assert_eq!(weight.kind, Some(Kind::NumberValue(-1.5)));

		let mut out = Output::new();
		cluster::serialize(&decoded, &mut out).unwrap();
		assert_eq!(cluster::size_of(&decoded), bytes.len());
		assert_eq!(out.into_inner(), bytes);
	}

	#[test]
	fn text_fixture_encodes_like_protoc() {
		let value = crate::gen::cluster_fixture::value();
		let mut out = Output::new();
		cluster::serialize(&value, &mut out).unwrap();
		assert_eq!(out.into_inner(), fixture());
		let decoded = cluster::deserialize(&mut Input::new(&fixture())).unwrap();
		assert_eq!(decoded, value);
	}

	#[test]
	fn unknown_fields_are_skipped() {
		let mut out = Output::new();
		crate::pds::protobuf::write_key(&mut out, 1, 2);
		crate::pds::protobuf::write_len(&mut out, 1);
		out.write(b"x");
		crate::pds::protobuf::write_key(&mut out, 9999, 0);
		crate::pds::protobuf::write_varint(&mut out, u64::MAX);
		let bytes = out.into_inner();
		let decoded = c::deserialize(&mut Input::new(&bytes)).unwrap();
		assert_eq!(decoded.name.as_deref(), Some("x"));
	}

	fn framed(out: &mut Output, number: u32, body: &[u8]) {
		crate::pds::protobuf::write_key(out, number, 2);
		crate::pds::protobuf::write_len(out, body.len());
		out.write(body);
	}

	fn bytes(parts: &[(u32, Vec<u8>)]) -> Vec<u8> {
		let mut out = Output::new();
		for (number, body) in parts {
			framed(&mut out, *number, body);
		}
		out.into_inner()
	}

	#[test]
	fn repeated_singular_message_fields_merge() {
		let wire = bytes(&[(33, bytes(&[(1, b"a".to_vec())])), (33, bytes(&[(2, vec![])]))]);
		let decoded = c::deserialize(&mut Input::new(&wire)).unwrap();
		let assignment = decoded.load_assignment.as_ref().unwrap();
		assert_eq!(assignment.cluster_name.as_deref(), Some("a"));
		assert_eq!(assignment.endpoints.len(), 1);
	}

	#[test]
	fn repeated_oneof_message_members_merge_until_another_member_is_set() {
		use crate::gen::google::protobuf::value;
		let entry = |key: &str| bytes(&[(1, bytes(&[(1, key.as_bytes().to_vec())]))]);
		let keys = |wire: Vec<u8>| -> Vec<String> {
			match value::deserialize(&mut Input::new(&wire)).unwrap().kind {
				Some(Kind::StructValue(s)) => s.fields.iter().map(|f| f.key.clone().unwrap()).collect(),
				other => panic!("expected a struct, got {other:?}"),
			}
		};
		assert_eq!(keys(bytes(&[(5, entry("a")), (5, entry("b"))])), ["a", "b"]);
		assert_eq!(keys(bytes(&[(5, entry("a")), (3, b"s".to_vec()), (5, entry("b"))])), ["b"]);
	}
}
