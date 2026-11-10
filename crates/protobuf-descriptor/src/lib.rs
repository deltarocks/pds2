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

pub use gen::pds_google_protobuf_DescriptorProto::Ty as DescriptorProto;
pub use gen::pds_google_protobuf_EnumDescriptorProto::Ty as EnumDescriptorProto;
pub use gen::pds_google_protobuf_FieldDescriptorProto::Ty as FieldDescriptorProto;
pub use gen::pds_google_protobuf_FieldDescriptorProto_Label::Ty as Label;
pub use gen::pds_google_protobuf_FieldDescriptorProto_Type::Ty as Type;
pub use gen::pds_google_protobuf_FileDescriptorProto::Ty as FileDescriptorProto;
pub use gen::pds_google_protobuf_FileDescriptorSet::Ty as FileDescriptorSet;

pub fn decode(bytes: &[u8]) -> pds::Result<FileDescriptorSet> {
	gen::pds_google_protobuf_FileDescriptorSet::deserialize(&mut pds::Input::new(bytes))
}
