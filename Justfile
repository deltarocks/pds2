minecraft:
	rm -rf crates/minecraft-protocol/src/gen
	cargo run upgrade protocol.json
	cargo run compile protocol.pds rust --to crates/minecraft-protocol/src/gen
