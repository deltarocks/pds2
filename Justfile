protoc := "nix shell nixpkgs#protobuf -c protoc"
protos := "target/protos"
envoy_includes := "-I " + protos + "/data-plane-api -I " + protos + "/xds -I " + protos + "/protoc-gen-validate"
envoy_roots := "envoy/config/cluster/v3/cluster.proto envoy/extensions/upstreams/http/v3/http_protocol_options.proto"

lang:
	rm -rf crates/lang-tests/src/gen ts/lang-tests/gen
	cargo run compile crates/lang-tests/lang.pds rust --to crates/lang-tests/src/gen --view ''
	cargo run compile crates/lang-tests/lang.pds typescript --to ts/lang-tests/gen

csn1:
	rm -rf crates/csn1-tests/src/gen
	cargo run compile crates/csn1-tests/csn1.pds rust --to crates/csn1-tests/src/gen

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

envoy-protos:
	test -d {{protos}}/data-plane-api || gh repo clone envoyproxy/data-plane-api {{protos}}/data-plane-api -- --depth 1
	test -d {{protos}}/xds || gh repo clone cncf/xds {{protos}}/xds -- --depth 1
	test -d {{protos}}/protoc-gen-validate || gh repo clone bufbuild/protoc-gen-validate {{protos}}/protoc-gen-validate -- --depth 1

envoy: envoy-protos
	{{protoc}} {{envoy_includes}} --include_imports --descriptor_set_out=target/envoy.pb {{envoy_roots}}
	cargo run protobuf convert target/envoy.pb --root envoy/config/cluster/v3/cluster.proto --root envoy/extensions/upstreams/http/v3/http_protocol_options.proto --to crates/envoy-api/envoy.pds
	rm -rf crates/envoy-api/src/gen ts/envoy-api/gen
	cargo run compile crates/envoy-api/envoy.pds rust --const cluster_fixture=crates/envoy-api/cluster.txtpds2 --to crates/envoy-api/src/gen
	cargo run compile crates/envoy-api/envoy.pds typescript --const cluster_fixture=crates/envoy-api/cluster.txtpds2 --to ts/envoy-api/gen
	{{protoc}} {{envoy_includes}} --encode=envoy.config.cluster.v3.Cluster {{envoy_roots}} < crates/envoy-api/cluster.txtpb > target/cluster.bin
	cargo run protobuf encode crates/envoy-api/envoy.pds crates/envoy-api/cluster.txtpds2 | cmp - target/cluster.bin
	cargo run protobuf decode crates/envoy-api/envoy.pds --type root::envoy::config::cluster::v3::Cluster target/cluster.bin > target/cluster.decoded.txtpds2
	cargo run protobuf encode crates/envoy-api/envoy.pds target/cluster.decoded.txtpds2 | cmp - target/cluster.bin

test: lang csn1 descriptor-check envoy
	cargo test --workspace
	cd ts && deno test --allow-net --allow-read
