use genco::lang::Rust;
use pds2_core::specializer::Registry;

mod choice;
mod natives;
mod pattern;
pub mod wire;

pub use choice::Choice;
pub use natives::{Fixed, Optional, Padded, Repeated, Spare, Uint};
use wire::register_encoder;

pub fn register(reg: &mut Registry) {
	reg.register_asthir("uint", Uint);
	reg.register_layout("uint", Uint);
	reg.register_codegen::<Rust>("uint", Uint);
	register_encoder(reg, "uint", Uint);

	reg.register_asthir("optional", Optional);
	reg.register_layout("optional", Optional);
	reg.register_codegen::<Rust>("optional", Optional);
	reg.register_value("optional", Optional);
	register_encoder(reg, "optional", Optional);

	reg.register_asthir("repeated", Repeated);
	reg.register_layout("repeated", Repeated);
	reg.register_codegen::<Rust>("repeated", Repeated);
	reg.register_value("repeated", Repeated);
	register_encoder(reg, "repeated", Repeated);

	reg.register_asthir("choice", Choice);
	reg.register_layout("choice", Choice);
	reg.register_codegen::<Rust>("choice", Choice);
	reg.register_value("choice", Choice);
	register_encoder(reg, "choice", Choice);

	reg.register_asthir("fixed", Fixed);
	reg.register_layout("fixed", Fixed);
	reg.register_codegen::<Rust>("fixed", Fixed);
	reg.register_value("fixed", Fixed);
	register_encoder(reg, "fixed", Fixed);

	reg.register_asthir("spare", Spare);
	reg.register_layout("spare", Spare);
	reg.register_codegen::<Rust>("spare", Spare);
	reg.register_value("spare", Spare);
	register_encoder(reg, "spare", Spare);

	reg.register_asthir("padded", Padded);
	reg.register_layout("padded", Padded);
	reg.register_codegen::<Rust>("padded", Padded);
	reg.register_value("padded", Padded);
	register_encoder(reg, "padded", Padded);

	register_encoder(reg, "bits_container", wire::Container);
	register_encoder(reg, "bits_array", wire::Array);

	reg.register_function("serialized_bits", wire::SerializedBits);
	reg.register_function("deserialized_bits", wire::DeserializedBits);
}
