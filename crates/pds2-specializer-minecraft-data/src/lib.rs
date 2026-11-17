use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;
use pds2_specializer_bytes::register_encoder;
use pds2_specializer_protodef::UpgradeRegistry;

mod natives;
mod ts;

use natives::*;

pub fn register(reg: &mut Registry) {
	pds2_specializer_generic::register_terminated(reg, "entityMetadataLoop");

	reg.register_layout("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_codegen::<Rust>("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_codegen::<JavaScript>("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_value("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	register_encoder(reg, "topBitSetTerminatedArray", TopBitSetTerminatedArray);

	reg.register_asthir("registryEntryHolder", RegistryEntryHolder);
	reg.register_layout("registryEntryHolder", RegistryEntryHolder);
	reg.register_codegen::<Rust>("registryEntryHolder", RegistryEntryHolder);
	reg.register_codegen::<JavaScript>("registryEntryHolder", RegistryEntryHolder);
	reg.register_value("registryEntryHolder", RegistryEntryHolder);
	register_encoder(reg, "registryEntryHolder", RegistryEntryHolder);

	reg.register_asthir("registryEntryHolderSet", RegistryEntryHolderSet);
	reg.register_layout("registryEntryHolderSet", RegistryEntryHolderSet);
	reg.register_codegen::<Rust>("registryEntryHolderSet", RegistryEntryHolderSet);
	reg.register_codegen::<JavaScript>("registryEntryHolderSet", RegistryEntryHolderSet);
	reg.register_value("registryEntryHolderSet", RegistryEntryHolderSet);
	register_encoder(reg, "registryEntryHolderSet", RegistryEntryHolderSet);
}

pub fn register_upgraders(reg: &mut UpgradeRegistry) {
	reg.register(EntityMetadataLoop);
	reg.register(TopBitSetTerminatedArray);
	reg.register(RegistryEntryHolder);
	reg.register(RegistryEntryHolderSet);
	reg.register(UUID);
	reg.register(AnonOptionalNbt);
	reg.register(AnonymousNbt);
	reg.register(LpVec3);
	for name in ["nbt", "optionalNbt", "compressedNbt", "byte_array", "string"] {
		reg.register(Opaque(name));
	}
}
