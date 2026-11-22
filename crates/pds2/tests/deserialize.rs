use anyhow::{anyhow, Result};

use pds2_core::mir::{self, TypeMap};
use protobuf_descriptor::gen::google::protobuf::file_descriptor_set as file_set;
use protobuf_descriptor::gen::google::protobuf::FieldDescriptorProtoType;
use protobuf_descriptor::pds::{Input, Output};

const DECL: &str = r#"
	@scalar u8 @encoding be def_native(u8);
	@scalar i8 @encoding be def_native(i8);
	@scalar u16 @encoding le def_native(u16);
	@scalar i32 @encoding zigzag def_native(sint);
	@scalar f32 @encoding le def_native(f32);
	@scalar f64 @encoding be def_native(f64);
	@scalar bool @encoding be def_native(bool);
	@scalar string @encoding raw def_native(text);
	@specializer array
	def_native(array) { type(Value); variant(sized) { argument(count); virtual_field(size) => u8; }; variant(inline) { type(Count); }; };
	@specializer option
	def_native(option) { type(Value); };
	@specializer container
	def_native(container) {
		body {
			item(field) { type(Value); };
			item(virtual_field) { argument(value); type(Value); };
		};
	};
	@specializer union
	def_native(union) {
		body {
			item(variant) { argument(match); type(Value) => void; };
			item(default) { anonymous; type(Value) => void; };
		};
		variant(external) { argument(tag); };
		variant(inline) { type(Tag); };
	};
	def_native(void) { is_void; };
	@specializer const
	def_native(const) { type(Value); argument(value) => Value; body; };
	def(Counted) => container {
		virtual_field(len, value: ./data/@size) => u8;
		field(data) => array(count: ../len) => u8;
	};
	def(Mirrored) => container {
		virtual_field(n, value: ./data/@size) => u8;
		field(data) => array(count: 2) => u8;
	};
	def(Shape) => union(Tag: u8) {
		variant(empty, match: 0);
		variant(dot, match: 1) => container { field(x) => i8; field(y) => i8; };
		default;
	};
	def(Numbers) => container {
		field(s) => sint;
		field(w) => u16;
		field(f) => f32;
		field(d) => f64;
		field(b) => bool;
		field(o) => option => u8;
		field(shapes) => array(Count: u8) => Shape;
		field(name) => text;
	};
"#;

const DESCRIPTOR: &str = include_str!("../../protobuf-descriptor/descriptor.pds");
const FILE_SET: &str = "root::google::protobuf::FileDescriptorSet";

fn constant(def: &str) -> Result<TypeMap> {
	let reg = pds2::registry();
	let ast = pds2_core::ast::parse(&format!("{DECL} {def}")).map_err(|e| anyhow!("{e}"))?;
	mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg)?, &reg)
}

fn err(def: &str) -> String {
	format!("{:#}", constant(def).expect_err("should be rejected"))
}

fn roundtrip(schema: &str, ty: &str, bytes: &[u8]) -> String {
	let reg = pds2::registry();
	let text = pds2_cli::decode(schema, ty, bytes, &reg).unwrap();
	let again = pds2_cli::encode(schema, None, &text, &reg).unwrap();
	assert_eq!(again, bytes, "{text}");
	text
}

#[test]
fn minimum_wire_sizes_are_known_at_compile_time() {
	let reg = pds2::registry();
	let types = constant("").unwrap();
	let elab = pds2_core::specializer::expr::Elaborator { types: &types, reg: &reg };
	let enc = pds2_specializer_bytes::Encoding::new(&elab);
	let min = |name: &str| enc.min_size(elab.resolve_name(name).unwrap());
	assert_eq!(min("u16"), 2);
	assert_eq!(min("sint"), 1);
	assert_eq!(min("text"), 0);
	assert_eq!(min("Counted"), 1);
	assert_eq!(min("Shape"), 1);
	assert_eq!(min("Numbers"), 1 + 2 + 4 + 8 + 1 + 1 + 1);
}

#[test]
fn deserialized_values_take_their_type_from_context() {
	constant("def(c) => const(value: Counted(deserialized(b\"\\x02\\x07\\x08\")));").unwrap();
	constant("def(c) => const(value: [deserialized(b\"\\x00\"), (dot: (x: 1, y: 2))]) => array(count: 2) => Shape;")
		.unwrap();
	constant("def(c) => const(value: Shape(deserialized(serialized(Shape((dot: (x: -1, y: 2)))))));").unwrap();
	constant("def(c) => const(value: deserialized(b\"\\x07\")) => u8;").unwrap();
	let e = err("def(c) => const(value: serialized(deserialized(b\"\\x00\"))) => u8;");
	assert!(e.contains("needs a type"), "{e}");
}

#[test]
fn deserialized_bytes_are_checked() {
	let e = err("def(c) => const(value: Counted(deserialized(b\"\\x02\\x07\")));");
	assert!(e.contains("1 bytes needed, 0 left"), "{e}");
	let e = err("def(c) => const(value: Counted(deserialized(b\"\\x01\\x07\\x08\")));");
	assert!(e.contains("1 bytes are left"), "{e}");
	let e = err("def(c) => const(value: Shape(deserialized(\"\")));");
	assert!(e.contains("byte 0: 1 bytes needed"), "{e}");
	let e = err("def(c) => const(value: Shape(deserialized(7)));");
	assert!(e.contains("takes bytes"), "{e}");
	let e = err("def(c) => const(value: Mirrored(deserialized(b\"\\x05\\x07\\x08\")));");
	assert!(e.contains("virtual field n reads Signed(5), but its value source gives Signed(2)"), "{e}");
	let e = err("def(c) => const(value: Shape(deserialized(b\"\\x00\\x00\")));");
	assert!(e.contains("1 bytes are left after Shape"), "{e}");
}

#[test]
fn decoded_values_are_written_back_as_source() {
	let bytes = [
		0x03, 0x01, 0x02, 0xcd, 0xcc, 0xcc, 0x3d, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0, 0x01, 0x01, 0x09,
		0x03, 0x00, 0x01, 0x01, 0xff, 0x05, b'q', b'"', b'\\', b'\n',
	];
	let text = roundtrip(DECL, "Numbers", &bytes);
	assert_eq!(
		text,
		"Numbers(\n\ts: -2,\n\tw: 513,\n\tf: 0.1,\n\td: 1.5,\n\tb: true,\n\to: 9,\n\tshapes: [empty, (dot: (x: 1, y: -1)), (default: 5)],\n\tname: \"q\\\"\\\\\\n\",\n)\n"
	);
	assert_eq!(roundtrip(DECL, "Counted", &[2, 7, 8]), "Counted(data: [7, 8])\n");
}

#[test]
fn protobuf_extensions_become_fields_of_the_extended_message() {
	let text = r#"root::google::protobuf::FileDescriptorSet(
		file: [
			(
				name: "a.proto",
				package: "a",
				message_type: [(
					name: "Opts",
					field: [(name: "x", number: 1, label: LABEL_OPTIONAL, type: TYPE_INT32)],
				)],
			),
			(
				name: "b.proto",
				package: "b",
				dependency: ["a.proto"],
				message_type: [
					(name: "Tag", field: [(name: "v", number: 1, label: LABEL_OPTIONAL, type: TYPE_BOOL)]),
					(
						name: "Holder",
						extension: [(
							name: "nested",
							number: 101,
							label: LABEL_REPEATED,
							type: TYPE_STRING,
							extendee: ".a.Opts",
						)],
					),
				],
				extension: [(
					name: "tag",
					number: 100,
					label: LABEL_OPTIONAL,
					type: TYPE_MESSAGE,
					type_name: ".b.Tag",
					extendee: ".a.Opts",
				)],
			),
		],
	)"#;
	let reg = pds2::registry();
	let set = protobuf_descriptor::decode(&pds2_cli::encode(DESCRIPTOR, None, text, &reg).unwrap())
		.unwrap();
	let schema = pds2_specializer_protobuf::convert(&set, &["a.proto".to_owned()]).unwrap();
	assert!(schema.contains("optional(b_tag, number: 100) => root::b::Tag;"), "{schema}");
	assert!(schema.contains("repeated(b_Holder_nested, number: 101) => root::string;"), "{schema}");
	assert!(!schema.contains("def(Holder)"), "{schema}");
	let value = "root::a::Opts(x: 1, b_tag: (v: true), b_Holder_nested: [\"p\", \"q\"])\n";
	let bytes = pds2_cli::encode(&schema, None, value, &reg).unwrap();
	assert_eq!(roundtrip(&schema, "root::a::Opts", &bytes), value);
}

#[test]
fn repeated_message_fields_merge_like_generated_code() {
	let options = |number: u8, text: u8| [0x42, 0x03, (number << 3) | 2, 0x01, text];
	let file: Vec<u8> = [options(1, b'x'), options(8, b'y')].concat();
	let bytes = [&[0x0a, file.len() as u8][..], &file].concat();

	let set = file_set::deserialize(&mut Input::new(&bytes)).unwrap();
	let opts = set.file[0].options.as_ref().unwrap();
	assert_eq!(opts.java_package.as_deref(), Some("x"));
	assert_eq!(opts.java_outer_classname.as_deref(), Some("y"));

	let text = pds2_cli::decode(DESCRIPTOR, FILE_SET, &bytes, &pds2::registry()).unwrap();
	assert!(text.contains("java_package: \"x\""), "{text}");
	assert!(text.contains("java_outer_classname: \"y\""), "{text}");
}

#[test]
fn protobuf_text_encodes_like_generated_code() {
	let text = r#"root::google::protobuf::FileDescriptorSet(
		file: [(
			name: "a.proto",
			package: "pkg",
			message_type: [(
				name: "M",
				field: [
					(name: "id", number: 1, label: LABEL_OPTIONAL, type: TYPE_INT32, oneof_index: -1),
					(name: "tags", number: 2, label: LABEL_REPEATED, type: 99, default_value: "\u{e9}\t"),
				],
				options: (deprecated: true),
				reserved_name: ["a", "b"],
			)],
			options: (
				java_package: "x",
				uninterpreted_option: [(
					name: [(name_part: "p", is_extension: false)],
					positive_int_value: 18446744073709551615,
					negative_int_value: -5,
					double_value: 0.1,
					string_value: b"\x00\xff",
				)],
			),
		)],
	)"#;
	let reg = pds2::registry();
	let bytes = pds2_cli::encode(DESCRIPTOR, None, text, &reg).unwrap();
	let set = file_set::deserialize(&mut Input::new(&bytes)).unwrap();
	let fields = &set.file[0].message_type[0].field;
	assert_eq!(fields[0].oneof_index, Some(-1));
	assert_eq!(fields[1].r#type, Some(FieldDescriptorProtoType::Unknown(99)));
	assert_eq!(fields[1].default_value.as_deref(), Some("\u{e9}\t"));
	let mut out = Output::new();
	file_set::serialize(&set, &mut out).unwrap();
	assert_eq!(out.into_inner(), bytes);

	let decoded = roundtrip(DESCRIPTOR, FILE_SET, &bytes);
	assert!(decoded.contains("type: 99"), "{decoded}");
	assert!(decoded.contains("positive_int_value: 18446744073709551615"), "{decoded}");
	assert!(decoded.contains("string_value: b\"\\x00\\xff\""), "{decoded}");
	let e = pds2_cli::encode(DESCRIPTOR, None, "(file: [])", &reg).unwrap_err();
	assert!(format!("{e:#}").contains("--type"), "{e:#}");
	assert_eq!(pds2_cli::encode(DESCRIPTOR, Some(FILE_SET), "(file: [])", &reg).unwrap(), b"");
}

#[test]
fn protobuf_presence_follows_syntax_labels_and_features() {
	let text = r#"root::google::protobuf::FileDescriptorSet(
		file: [
			(
				name: "p2.proto",
				package: "p2",
				message_type: [(
					name: "Old",
					field: [
						(name: "id", number: 1, label: LABEL_REQUIRED, type: TYPE_INT32),
						(name: "note", number: 2, label: LABEL_OPTIONAL, type: TYPE_STRING),
					],
				)],
			),
			(
				name: "p3.proto",
				package: "p3",
				syntax: "proto3",
				message_type: [(
					name: "New",
					field: [
						(name: "n", number: 1, label: LABEL_OPTIONAL, type: TYPE_INT32),
						(
							name: "kept",
							number: 2,
							label: LABEL_OPTIONAL,
							type: TYPE_INT32,
							oneof_index: 0,
							proto3_optional: true,
						),
						(name: "old", number: 3, label: LABEL_OPTIONAL, type: TYPE_MESSAGE, type_name: ".p2.Old"),
						(name: "a", number: 4, label: LABEL_OPTIONAL, type: TYPE_STRING, oneof_index: 1),
						(name: "d", number: 5, label: LABEL_OPTIONAL, type: TYPE_DOUBLE),
					],
					oneof_decl: [(name: "_kept"), (name: "choice")],
				)],
			),
			(
				name: "ed.proto",
				package: "ed",
				syntax: "editions",
				edition: EDITION_2023,
				options: (features: (field_presence: IMPLICIT)),
				message_type: [(
					name: "Ed",
					field: [
						(name: "i", number: 1, label: LABEL_OPTIONAL, type: TYPE_INT32),
						(
							name: "e",
							number: 2,
							label: LABEL_OPTIONAL,
							type: TYPE_INT32,
							options: (features: (field_presence: EXPLICIT)),
						),
						(
							name: "r",
							number: 3,
							label: LABEL_OPTIONAL,
							type: TYPE_INT32,
							options: (features: (field_presence: LEGACY_REQUIRED)),
						),
					],
				)],
			),
		],
	)"#;
	let reg = pds2::registry();
	let set = protobuf_descriptor::decode(&pds2_cli::encode(DESCRIPTOR, None, text, &reg).unwrap())
		.unwrap();
	let schema = pds2_specializer_protobuf::convert(&set, &[]).unwrap();
	for item in [
		"field(id, number: 1) => root::int32;",
		"optional(note, number: 2) => root::string;",
		"implicit(n, number: 1) => root::int32;",
		"optional(kept, number: 2) => root::int32;",
		"optional(old, number: 3) => root::p2::Old;",
		"oneof(a, number: 4, group: \"choice\") => root::string;",
		"implicit(d, number: 5) => root::double;",
		"implicit(i, number: 1) => root::int32;",
		"optional(e, number: 2) => root::int32;",
		"field(r, number: 3) => root::int32;",
	] {
		assert!(schema.contains(item), "{item} in {schema}");
	}

	let e = pds2_cli::encode(&schema, None, "root::p2::Old(note: \"x\")\n", &reg).unwrap_err();
	assert!(format!("{e:#}").contains("needs its required field"), "{e:#}");
	let e = pds2_cli::decode(&schema, "root::p2::Old", b"\x12\x01x", &reg).unwrap_err();
	assert!(format!("{e:#}").contains("is missing"), "{e:#}");
	let old = "root::p2::Old(id: 0)\n";
	assert_eq!(pds2_cli::encode(&schema, None, old, &reg).unwrap(), b"\x08\x00");
	assert_eq!(roundtrip(&schema, "root::p2::Old", b"\x08\x00"), old);

	let zeros = "root::p3::New(n: 0, kept: 0, a: \"\", d: 0.0)\n";
	assert_eq!(pds2_cli::encode(&schema, None, zeros, &reg).unwrap(), b"\x10\x00\x22\x00");
	let decoded = pds2_cli::decode(&schema, "root::p3::New", b"\x08\x00\x29\0\0\0\0\0\0\0\0", &reg).unwrap();
	assert!(!decoded.contains("n:") && !decoded.contains("d:"), "{decoded}");
	let negative = b"\x29\0\0\0\0\0\0\0\x80";
	assert_eq!(roundtrip(&schema, "root::p3::New", negative), "root::p3::New(d: -0.0)\n");
}
