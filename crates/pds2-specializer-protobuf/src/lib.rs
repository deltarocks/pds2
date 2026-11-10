use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;

mod convert;
pub mod enumeration;
pub mod message;

pub use convert::convert;
use enumeration::Enum;
use message::Message;

pub fn register(reg: &mut Registry) {
	reg.register_asthir("message", Message);
	reg.register_layout("message", Message);
	reg.register_codegen::<Rust>("message", Message);
	reg.register_codegen::<JavaScript>("message", Message);
	reg.register_value("message", Message);
	reg.register_encoder("message", Message);

	reg.register_asthir("enum", Enum);
	reg.register_layout("enum", Enum);
	reg.register_codegen::<Rust>("enum", Enum);
	reg.register_codegen::<JavaScript>("enum", Enum);
	reg.register_value("enum", Enum);
	reg.register_encoder("enum", Enum);
}
