minecraft: nbt
	rm -rf crates/minecraft-protocol/src/gen ts/minecraft-protocol/gen
	cargo run upgrade protocol.json
	cargo run compile protocol.pds rust --to crates/minecraft-protocol/src/gen
	cargo run compile protocol.pds typescript --to ts/minecraft-protocol/gen

nbt:
	rm -rf crates/nbt/src/gen ts/nbt/gen
	cargo run compile crates/nbt/nbt.pds rust --to crates/nbt/src/gen
	cargo run compile crates/nbt/nbt.pds typescript --to ts/nbt/gen

test: minecraft
	cargo test --workspace
	cd ts && deno test --allow-net
