import { assertEquals } from "@std/assert";
import { Input, Output } from "./pds.ts";
import * as cluster from "./gen/pds_envoy_config_cluster_v3_Cluster.ts";
import * as cluster_fixture from "./gen/pds_cluster_fixture.ts";

Deno.test("cluster roundtrips protoc encoding", async () => {
	const bytes = await Deno.readFile(new URL("../../target/cluster.bin", import.meta.url));
	const input = new Input(bytes);
	const decoded = cluster.deserialize(input);
	assertEquals(input.remaining(), 0);

	assertEquals(decoded.name, "backend");
	const endpoints = decoded.load_assignment!.endpoints![0];
	assertEquals(endpoints.lb_endpoints!.length, 2);
	assertEquals(endpoints.priority, 1);
	const weight = decoded.metadata!.filter_metadata![0].value!.fields![1].value!;
	assertEquals(weight.kind, { kind: "number_value", value: -1.5 });

	const out = new Output();
	cluster.serialize(decoded, out);
	assertEquals(cluster.size_of(decoded), bytes.length);
	assertEquals(out.into_inner(), bytes);
});

Deno.test("text fixture encodes like protoc", async () => {
	const bytes = await Deno.readFile(new URL("../../target/cluster.bin", import.meta.url));
	const value = cluster_fixture.value();
	const out = new Output();
	cluster.serialize(value, out);
	assertEquals(out.into_inner(), bytes);
	assertEquals(cluster.deserialize(new Input(bytes)), value);
});
