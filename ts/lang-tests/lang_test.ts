import { assert, assertEquals, assertThrows } from "@std/assert";
import { equal, Input, Output } from "./pds.ts";
import * as pds_Arguments from "./gen/Arguments.ts";
import * as pds_Branch from "./gen/Branch.ts";
import * as sample_branch from "./gen/sample_branch.ts";
import * as encoded_branch from "./gen/encoded_branch.ts";
import * as decoded_branch from "./gen/decoded_branch.ts";
import * as sample_zeros from "./gen/sample_zeros.ts";
import * as encoded_zeros from "./gen/encoded_zeros.ts";
import * as merged_branch from "./gen/merged_branch.ts";
import * as decoded_split from "./gen/decoded_split.ts";
import * as switched_branch from "./gen/switched_branch.ts";
import * as decoded_switch from "./gen/decoded_switch.ts";
import * as pds_BoundWords from "./gen/BoundWords.ts";
import * as pds_Choices from "./gen/Choices.ts";
import * as pds_Counted from "./gen/Counted.ts";
import * as pds_Tagged from "./gen/Tagged.ts";
import * as pds_Widened from "./gen/Widened.ts";
import * as pds_Words from "./gen/Words.ts";
import * as sample_choices from "./gen/sample_choices.ts";
import * as sample_counted from "./gen/sample_counted.ts";
import * as sample_default from "./gen/sample_default.ts";
import * as sample_flags from "./gen/sample_flags.ts";
import * as sample_tagged from "./gen/sample_tagged.ts";
import * as sample_words from "./gen/sample_words.ts";
import * as encoded_counted from "./gen/encoded_counted.ts";
import * as encoded_words from "./gen/encoded_words.ts";
import * as encoded_choices from "./gen/encoded_choices.ts";
import * as encoded_tagged from "./gen/encoded_tagged.ts";
import * as encoded_default from "./gen/encoded_default.ts";
import * as sample_packed from "./gen/sample_packed.ts";
import * as encoded_packed from "./gen/encoded_packed.ts";
import * as sample_flagset from "./gen/sample_flagset.ts";
import * as encoded_flagset from "./gen/encoded_flagset.ts";
import * as sample_blob from "./gen/sample_blob.ts";
import * as encoded_blob from "./gen/encoded_blob.ts";
import * as sample_holders from "./gen/sample_holders.ts";
import * as encoded_holders from "./gen/encoded_holders.ts";
import * as sample_loop from "./gen/sample_loop.ts";
import * as encoded_loop from "./gen/encoded_loop.ts";
import * as sample_top_bits from "./gen/sample_top_bits.ts";
import * as encoded_top_bits from "./gen/encoded_top_bits.ts";
import * as encoded_widened from "./gen/encoded_widened.ts";
import * as decoded_counted from "./gen/decoded_counted.ts";
import * as decoded_words from "./gen/decoded_words.ts";
import * as decoded_choices from "./gen/decoded_choices.ts";
import * as decoded_tagged from "./gen/decoded_tagged.ts";
import * as decoded_default from "./gen/decoded_default.ts";
import * as decoded_packed from "./gen/decoded_packed.ts";
import * as decoded_flagset from "./gen/decoded_flagset.ts";
import * as decoded_blob from "./gen/decoded_blob.ts";
import * as decoded_holders from "./gen/decoded_holders.ts";
import * as decoded_loop from "./gen/decoded_loop.ts";
import * as decoded_top_bits from "./gen/decoded_top_bits.ts";
import * as sample_arguments from "./gen/sample_arguments.ts";
import * as encoded_arguments from "./gen/encoded_arguments.ts";
import * as decoded_arguments from "./gen/decoded_arguments.ts";
import * as pds_Mapped from "./gen/Mapped.ts";
import * as pds_Blob from "./gen/Blob.ts";
import * as pds_FixedArm from "./gen/FixedArm.ts";
import * as sample_mapped from "./gen/sample_mapped.ts";
import * as encoded_mapped from "./gen/encoded_mapped.ts";
import * as decoded_mapped from "./gen/decoded_mapped.ts";

type Codec<T> = {
	deserialize(input: Input): T;
	serialize(v: T, output: Output): void;
};

function roundtrip<T>(codec: Codec<T>, bytes: number[]): T {
	const input = new Input(new Uint8Array(bytes));
	const v = codec.deserialize(input);
	assertEquals(input.remaining(), 0);
	const out = new Output();
	codec.serialize(v, out);
	assertEquals([...out.into_inner()], bytes);
	return v;
}

Deno.test("arithmetic in virtual fields and counts", () => {
	assertEquals([...roundtrip(pds_Counted, [3, 7, 8]).data], [7, 8]);
	assertThrows(() => roundtrip(pds_Counted, [0]));
});

Deno.test("explicit cast widens before multiplying", () => {
	const bytes = [200, ...Array.from({ length: 800 }, (_, i) => i & 0xff)];
	assertEquals(roundtrip(pds_Widened, bytes).items.length, 400);
});

Deno.test("typed terminator reads whole values", () => {
	const bytes = [0, 1, 0xff, 0, 0xff, 0xff];
	assertEquals([...roundtrip(pds_Words, bytes)], [1, 0xff00]);
	assertEquals([...roundtrip(pds_BoundWords, bytes)], [1, 0xff00]);
});

Deno.test("typed def arguments take constants and fields", () => {
	roundtrip(pds_Choices, [1, 5, 0, 7]);
	roundtrip(pds_Choices, [2, 0, 9, 0, 7]);
});

Deno.test("def arguments reach nested fields, elements, arms and defs", () => {
	const one = roundtrip(pds_Arguments, [1, 5, 6, 7, 0, 8, 9, 10, 11, 12, 13, 14]);
	assertEquals(one.nested.forwarded.choice, { kind: "one", value: 10 });
	assertEquals(one.nested.deep.inner.choice, { kind: "one", value: 9 });
	const bytes = [2, 0, 5, 0, 6, 0, 7, 0, 0, 8, 0, 9, 0, 10, 0, 11, 0, 12, 0, 13, 14];
	const two = roundtrip(pds_Arguments, bytes);
	assertEquals(two.nested.wrapped.choice, { kind: "two", value: 13 });
	assertEquals(two.nested.choices, [{ kind: "two", value: 11 }, { kind: "two", value: 12 }]);
	roundtrip(pds_Arguments, [1, 5, 6, 7, 3, 9, 10, 11, 12, 13, 14]);
	assertThrows(() => roundtrip(pds_Arguments, [3, 5, 6, 7, 0, 8, 9, 10, 11, 12, 13, 14]));
});

Deno.test("union tags compare by construction or cast", () => {
	for (const bytes of [[1, 0], [2, 9, 5, 1, 3, 4], [7, 0], [2, 9, 5, 1, 6]]) {
		roundtrip(pds_Tagged, bytes);
	}
	assertThrows(() => roundtrip(pds_Tagged, [8, 0]));
});

Deno.test("constants construct schema values", () => {
	const cases: [Codec<unknown> & { value(): unknown }, number[]][] = [
		[sample_counted, [3, 7, 8]],
		[sample_words, [0, 1, 0xff, 0, 0xff, 0xff]],
		[sample_choices, [2, 0, 9, 0, 7]],
		[sample_tagged, [2, 9, 5, 1, 3, 4]],
		[sample_default, [7, 0]],
		[sample_flags, [1, 0, 1]],
	];
	for (const [codec, bytes] of cases) {
		const out = new Output();
		codec.serialize(codec.value(), out);
		assertEquals([...out.into_inner()], bytes);
		assertEquals(roundtrip(codec, bytes), codec.value());
	}
});

Deno.test("compile-time serialization matches generated serializers", () => {
	const cases: [Codec<unknown> & { value(): unknown }, { value(): Uint8Array }][] = [
		[sample_counted, encoded_counted],
		[sample_words, encoded_words],
		[sample_choices, encoded_choices],
		[sample_tagged, encoded_tagged],
		[sample_default, encoded_default],
		[sample_packed, encoded_packed],
		[sample_flagset, encoded_flagset],
		[sample_blob, encoded_blob],
		[sample_holders, encoded_holders],
		[sample_loop, encoded_loop],
		[sample_top_bits, encoded_top_bits],
		[sample_arguments, encoded_arguments],
		[sample_mapped, encoded_mapped],
	];
	for (const [codec, encoded] of cases) {
		const out = new Output();
		codec.serialize(codec.value(), out);
		assertEquals(out.into_inner(), encoded.value());
		assertEquals(roundtrip(codec, [...encoded.value()]), codec.value());
	}
	assertEquals([...roundtrip(pds_Widened, [...encoded_widened.value()]).items], [1, 2, 3, 0xffff]);
});

Deno.test("sized lengths are checked on serialize", () => {
	const blob = sample_blob.value();
	assertThrows(() => pds_Blob.serialize({ ...blob, fixed: blob.fixed.slice(1) }, new Output()));
	assertThrows(() =>
		pds_FixedArm.serialize({ kind: "four", value: new Uint8Array(3) }, new Output())
	);
	pds_FixedArm.serialize({ kind: "four", value: new Uint8Array(4) }, new Output());
});

Deno.test("map rejects duplicate keys", () => {
	const input = new Input(new Uint8Array([2, 1, 97, 0, 1, 1, 97, 0, 2]));
	assertThrows(() => pds_Mapped.deserialize(input));
});

Deno.test("equal compares map entries", () => {
	assert(equal(new Map([["a", 1]]), new Map([["a", 1]])));
	assert(!equal(new Map([["a", 1]]), new Map([["a", 2]])));
	assert(!equal(new Map([["a", 1]]), new Map()));
	assert(!equal(new Map(), {}));
});

Deno.test("compile-time deserialization matches values", () => {
	const cases: [{ value(): unknown }, { value(): unknown }][] = [
		[decoded_counted, sample_counted],
		[decoded_words, sample_words],
		[decoded_choices, sample_choices],
		[decoded_tagged, sample_tagged],
		[decoded_default, sample_default],
		[decoded_packed, sample_packed],
		[decoded_flagset, sample_flagset],
		[decoded_blob, sample_blob],
		[decoded_holders, sample_holders],
		[decoded_loop, sample_loop],
		[decoded_top_bits, sample_top_bits],
		[decoded_arguments, sample_arguments],
		[decoded_mapped, sample_mapped],
	];
	for (const [decoded, sample] of cases) {
		assertEquals(decoded.value(), sample.value());
	}
});

Deno.test("protobuf fields follow their presence", () => {
	const out = new Output();
	pds_Branch.serialize(sample_branch.value(), out);
	assertEquals(out.into_inner(), encoded_branch.value());
	assertEquals(pds_Branch.deserialize(new Input(encoded_branch.value())), sample_branch.value());
	assertEquals(decoded_branch.value(), sample_branch.value());

	const zeros = new Output();
	pds_Branch.serialize(sample_zeros.value(), zeros);
	assertEquals([...zeros.into_inner()], [0x0a, 0x02, 0x08, 0x01]);
	assertEquals([...encoded_zeros.value()], [0x0a, 0x02, 0x08, 0x01]);

	const negative = new Output();
	pds_Branch.serialize({ ...sample_zeros.value(), weight: -0 }, negative);
	const bytes = negative.into_inner();
	assertEquals(bytes.length, 4 + 9);
	assert(Object.is(pds_Branch.deserialize(new Input(bytes)).weight, -0));
});

Deno.test("protobuf required fields are checked after merging", () => {
	assertThrows(() => pds_Branch.deserialize(new Input(new Uint8Array([]))));
	assertThrows(() => pds_Branch.deserialize(new Input(new Uint8Array([0x0a, 0x02, 0x10, 0x04]))));
	const split = new Uint8Array([
		0x0a, 0x02, 0x10, 0x04, 0x0a, 0x02, 0x08, 0x03, 0x12, 0x02, 0x10, 0x07, 0x12, 0x02, 0x08,
		0x09, 0x38, 0x05, 0x32, 0x02, 0x10, 0x01, 0x32, 0x02, 0x08, 0x02,
	]);
	assertEquals(pds_Branch.deserialize(new Input(split)), merged_branch.value());
	assertEquals(decoded_split.value(), merged_branch.value());
	const restarted = new Uint8Array([
		0x0a, 0x02, 0x08, 0x01, 0x32, 0x02, 0x08, 0x02, 0x38, 0x05, 0x32, 0x02, 0x10, 0x01,
	]);
	assertThrows(() => pds_Branch.deserialize(new Input(restarted)));
	const merged = merged_branch.value();
	pds_Branch.deserialize_extend(new Input(new Uint8Array([0x0a, 0x02, 0x10, 0x08])), merged);
	assertEquals([merged.root.id, merged.root.tag], [3, 8]);
	const switched = new Uint8Array([
		0x0a, 0x02, 0x08, 0x01, 0x42, 0x02, 0x08, 0x01, 0x32, 0x02, 0x08, 0x02, 0x42, 0x02, 0x10, 0x05,
	]);
	assertEquals(pds_Branch.deserialize(new Input(switched)), switched_branch.value());
	assertEquals(decoded_switch.value(), switched_branch.value());
});
