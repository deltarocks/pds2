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

pub use gen::google::protobuf::descriptor_proto::Ty as DescriptorProto;
pub use gen::google::protobuf::enum_descriptor_proto::Ty as EnumDescriptorProto;
pub use gen::google::protobuf::field_descriptor_proto::Ty as FieldDescriptorProto;
pub use gen::google::protobuf::field_descriptor_proto_label::Ty as Label;
pub use gen::google::protobuf::field_descriptor_proto_type::Ty as Type;
pub use gen::google::protobuf::file_descriptor_proto::Ty as FileDescriptorProto;
pub use gen::google::protobuf::file_descriptor_set::Ty as FileDescriptorSet;

pub fn decode(bytes: &[u8]) -> pds::Result<FileDescriptorSet> {
	gen::google::protobuf::file_descriptor_set::deserialize(&mut pds::Input::new(bytes))
}
