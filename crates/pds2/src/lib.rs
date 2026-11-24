use pds2_core::specializer::Registry;

pub mod survey;
use pds2_specializer_protodef::UpgradeRegistry;

pub fn registry() -> Registry {
	let mut reg = Registry::default();
	pds2_specializer_bytes::register(&mut reg);
	pds2_specializer_generic::register(&mut reg);
	pds2_specializer_protodef::register(&mut reg);
	pds2_specializer_minecraft_data::register(&mut reg);
	pds2_specializer_protobuf::register(&mut reg);
	pds2_specializer_csn1::register(&mut reg);
	pds2_specializer_bifrostlink::register(&mut reg);
	reg
}

pub fn minecraft_data_upgraders() -> UpgradeRegistry {
	let mut reg = UpgradeRegistry::default();
	pds2_specializer_protodef::register_upgraders(&mut reg);
	pds2_specializer_minecraft_data::register_upgraders(&mut reg);
	reg
}
