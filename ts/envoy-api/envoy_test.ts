import { assertEquals } from "@std/assert";
import { Input, Output, protobuf } from "./pds.ts";
import * as cluster from "./gen/envoy/config/cluster/v3/Cluster.ts";
import * as cluster_fixture from "./gen/cluster_fixture.ts";
import * as value from "./gen/google/protobuf/Value.ts";

function bytes(parts: [number, Uint8Array][]): Uint8Array {
	const out = new Output();
	for (const [number, body] of parts) {
		protobuf.writeKey(out, number, 2);
		protobuf.writeLen(out, body.length);
		out.write(body);
	}
	return out.into_inner();
}

const text = (s: string) => new TextEncoder().encode(s);

Deno.test("repeated singular message fields merge", () => {
	const wire = bytes([[33, bytes([[1, text("a")]])], [33, bytes([[2, new Uint8Array()]])]]);
	const decoded = cluster.deserialize(new Input(wire));
	assertEquals(decoded.load_assignment!.cluster_name, "a");
	assertEquals(decoded.load_assignment!.endpoints!.length, 1);
});

Deno.test("repeated oneof message members merge until another member is set", () => {
	const entry = (key: string) => bytes([[1, bytes([[1, text(key)]])]]);
	const keys = (wire: Uint8Array) => {
		const kind = value.deserialize(new Input(wire)).kind;
		if (kind?.kind !== "struct_value") throw new Error(`expected a struct, got ${kind?.kind}`);
		return kind.value.fields!.map((f) => f.key);
	};
	assertEquals(keys(bytes([[5, entry("a")], [5, entry("b")]])), ["a", "b"]);
	assertEquals(keys(bytes([[5, entry("a")], [3, text("s")], [5, entry("b")]])), ["b"]);
});

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
