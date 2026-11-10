import { assertEquals, assertThrows } from "@std/assert";
import { Input, Output } from "./pds.ts";
import * as pds_BoundWords from "./gen/pds_BoundWords.ts";
import * as pds_Choices from "./gen/pds_Choices.ts";
import * as pds_Counted from "./gen/pds_Counted.ts";
import * as pds_Tagged from "./gen/pds_Tagged.ts";
import * as pds_Widened from "./gen/pds_Widened.ts";
import * as pds_Words from "./gen/pds_Words.ts";

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
