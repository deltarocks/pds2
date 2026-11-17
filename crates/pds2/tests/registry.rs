#[test]
fn every_specializer_registers_an_encoder() {
	let reg = pds2::registry();
	let missing: Vec<&str> = reg
		.specializers()
		.into_iter()
		.filter(|n| !reg.has_encoder(n))
		.collect();
	assert!(missing.is_empty(), "no encoder for {missing:?}");
}
