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
	pub use pds_bytes_runtime::*;
}
pub mod pds_native {
	pub use pds_bytes_runtime::natives::pds_void;
	pub use pds_bytes_runtime::protobuf::natives::*;
}

pub use gen::google::protobuf::{
	DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FieldDescriptorProtoLabel as Label,
	FieldDescriptorProtoType as Type, FileDescriptorProto, FileDescriptorSet,
};

use self::gen::google::protobuf::file_descriptor_set;

pub fn decode(bytes: &[u8]) -> pds::Result<FileDescriptorSet> {
	file_descriptor_set::deserialize(&mut pds::Input::new(bytes))
}
