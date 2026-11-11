import { assertEquals, assertThrows } from "@std/assert";
import { Input, Output } from "./pds.ts";
import * as pds_Arguments from "./gen/Arguments.ts";
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
	];
	for (const [codec, encoded] of cases) {
		const out = new Output();
		codec.serialize(codec.value(), out);
		assertEquals(out.into_inner(), encoded.value());
		assertEquals(roundtrip(codec, [...encoded.value()]), codec.value());
	}
	assertEquals([...roundtrip(pds_Widened, [...encoded_widened.value()]).items], [1, 2, 3, 0xffff]);
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
	];
	for (const [decoded, sample] of cases) {
		assertEquals(decoded.value(), sample.value());
	}
});
