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

pub use gen::pds_envoy_config_cluster_v3_Cluster as cluster;

#[cfg(test)]
mod tests {
	use crate::cluster;
	use crate::gen::pds_envoy_config_cluster_v3_Cluster as c;
	use crate::gen::pds_google_protobuf_Value::kind;
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
		assert_eq!(weight.kind, Some(kind::number_value(-1.5)));

		let mut out = Output::new();
		cluster::serialize(&decoded, &mut out).unwrap();
		assert_eq!(cluster::size_of(&decoded), bytes.len());
		assert_eq!(out.into_inner(), bytes);
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
}
