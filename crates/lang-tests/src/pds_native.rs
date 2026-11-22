pub use pds_bytes_runtime::natives::{pds_bool, pds_i8, pds_u16, pds_u8, pds_void};
pub use pds_bytes_runtime::protobuf::natives::{pds_bytes, pds_double, pds_int32 as pds_varint};

pub mod view {
	pub use pds_bytes_runtime::protobuf::natives::view::pds_bytes;
}
