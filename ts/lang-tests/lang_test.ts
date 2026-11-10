import { assertEquals, assertThrows } from "@std/assert";
import { Input, Output } from "./pds.ts";
import * as pds_BoundWords from "./gen/pds_BoundWords.ts";
import * as pds_Choices from "./gen/pds_Choices.ts";
import * as pds_Counted from "./gen/pds_Counted.ts";
import * as pds_Tagged from "./gen/pds_Tagged.ts";
import * as pds_Widened from "./gen/pds_Widened.ts";
import * as pds_Words from "./gen/pds_Words.ts";
import * as sample_choices from "./gen/pds_sample_choices.ts";
import * as sample_counted from "./gen/pds_sample_counted.ts";
import * as sample_default from "./gen/pds_sample_default.ts";
import * as sample_flags from "./gen/pds_sample_flags.ts";
import * as sample_tagged from "./gen/pds_sample_tagged.ts";
import * as sample_words from "./gen/pds_sample_words.ts";
import * as encoded_counted from "./gen/pds_encoded_counted.ts";
import * as encoded_words from "./gen/pds_encoded_words.ts";
import * as encoded_choices from "./gen/pds_encoded_choices.ts";
import * as encoded_tagged from "./gen/pds_encoded_tagged.ts";
import * as encoded_default from "./gen/pds_encoded_default.ts";
import * as sample_packed from "./gen/pds_sample_packed.ts";
import * as encoded_packed from "./gen/pds_encoded_packed.ts";
import * as sample_flagset from "./gen/pds_sample_flagset.ts";
import * as encoded_flagset from "./gen/pds_encoded_flagset.ts";
import * as sample_blob from "./gen/pds_sample_blob.ts";
import * as encoded_blob from "./gen/pds_encoded_blob.ts";
import * as sample_holders from "./gen/pds_sample_holders.ts";
import * as encoded_holders from "./gen/pds_encoded_holders.ts";
import * as sample_loop from "./gen/pds_sample_loop.ts";
import * as encoded_loop from "./gen/pds_encoded_loop.ts";
import * as sample_top_bits from "./gen/pds_sample_top_bits.ts";
import * as encoded_top_bits from "./gen/pds_encoded_top_bits.ts";
import * as encoded_widened from "./gen/pds_encoded_widened.ts";

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
	];
	for (const [codec, encoded] of cases) {
		const out = new Output();
		codec.serialize(codec.value(), out);
		assertEquals(out.into_inner(), encoded.value());
		assertEquals(roundtrip(codec, [...encoded.value()]), codec.value());
	}
	assertEquals([...roundtrip(pds_Widened, [...encoded_widened.value()]).items], [1, 2, 3, 0xffff]);
});
