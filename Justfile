protoc := "nix shell nixpkgs#protobuf -c protoc"

lang:
	rm -rf crates/lang-tests/src/gen ts/lang-tests/gen
	cargo run compile crates/lang-tests/lang.pds rust --to crates/lang-tests/src/gen --view ''
	cargo run compile crates/lang-tests/lang.pds typescript --to ts/lang-tests/gen

csn1:
	rm -rf crates/csn1-tests/src/gen
	cargo run compile crates/csn1-tests/csn1.pds rust --to crates/csn1-tests/src/gen

bifrostlink-gen:
	rm -rf target/bifrostlink-gen
	mkdir -p target/bifrostlink-gen
	echo 'edition = "2021"' > target/bifrostlink-gen/rustfmt.toml
	cargo run compile crates/pds2-specializer-bifrostlink/tests/remowt.pds rust --to target/bifrostlink-gen
	rustfmt --check target/bifrostlink-gen/mod.rs

descriptor-set:
	mkdir -p target
	{{protoc}} -I crates/protobuf-descriptor --include_imports --descriptor_set_out=target/descriptor.pb google/protobuf/descriptor.proto

descriptor: descriptor-set
	cargo run protobuf convert target/descriptor.pb --root google/protobuf/descriptor.proto --to crates/protobuf-descriptor/descriptor.pds
	just descriptor-gen crates/protobuf-descriptor/descriptor.pds

bootstrap:
	just descriptor-gen crates/protobuf-descriptor/bootstrap.pds
	just descriptor

descriptor-gen schema:
	rm -rf target/descriptor-gen
	cargo run compile {{schema}} rust --to target/descriptor-gen
	rm -rf crates/protobuf-descriptor/src/gen
	mv target/descriptor-gen crates/protobuf-descriptor/src/gen

descriptor-check: descriptor-set
	cargo run protobuf convert target/descriptor.pb --root google/protobuf/descriptor.proto --to target/descriptor.check.pds
	diff crates/protobuf-descriptor/descriptor.pds target/descriptor.check.pds

test: lang csn1 bifrostlink-gen descriptor-check
	cargo test --workspace
	cd ts && deno test --allow-net --allow-read
