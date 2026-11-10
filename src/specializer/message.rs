use anyhow::{anyhow, bail, ensure, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::union::{self, TagView};
use super::{AstHir, BoxCost, Embed, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor};
use crate::ast::Ident;
use crate::generator::rust::rname;
use crate::hir::{self, HirSpecializerCtx, TypeId};
use crate::mir::{self, TypeKind, TypeMap, Value};

pub struct Message;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wire {
	Varint = 0,
	I64 = 1,
	Len = 2,
	I32 = 5,
}

pub enum Label<'a> {
	Single,
	Repeated,
	Oneof(&'a str),
}

pub struct MessageField<'a> {
	pub name: &'a Ident,
	pub number: u32,
	pub label: Label<'a>,
	pub ty: &'a mir::Type,
}

fn is_message(native: &mir::NativeType) -> bool {
	matches!(&native.specializer, Some((s, _)) if s.as_str() == "message")
}

pub fn view(native: &mir::NativeType) -> Option<Vec<MessageField<'_>>> {
	if !is_message(native) {
		return None;
	}
	native
		.body
		.iter()
		.zip(native.fields.iter())
		.map(|(item, (name, ty))| {
			let number = match item.constant("number")? {
				Value::Signed(n) => u32::try_from(*n).ok()?,
				Value::Unsigned(n) => u32::try_from(*n).ok()?,
				_ => return None,
			};
			let label = match item.kind.as_str() {
				"field" => Label::Single,
				"repeated" => Label::Repeated,
				"oneof" => match item.constant("group")? {
					Value::String(g) => Label::Oneof(g),
					_ => return None,
				},
				_ => return None,
			};
			Some(MessageField {
				name,
				number,
				label,
				ty,
			})
		})
		.collect()
}

pub fn wire_of(types: &TypeMap, id: TypeId) -> Result<Wire> {
	let name = || types[id].name.readable_name();
	match &types[id].kind {
		TypeKind::Alias(t) => wire_of(types, t.target),
		TypeKind::Native(n) => {
			if let Some(attr) = n.attr("protobuf") {
				return Ok(match attr {
					[w] if w.as_str() == "varint" => Wire::Varint,
					[w] if w.as_str() == "i64" => Wire::I64,
					[w] if w.as_str() == "len" => Wire::Len,
					[w] if w.as_str() == "i32" => Wire::I32,
					w => bail!("{}: unknown protobuf wire type {w:?}", name()),
				});
			}
			if is_message(n) {
				return Ok(Wire::Len);
			}
			if let Some(u) = union::view(n) {
				let payloadless = u.arms.iter().all(|(_, _, t)| types.is_void(t.target))
					&& u.default.is_none_or(|(_, t)| types.is_void(t.target));
				if let (TagView::Inline(tag), true) = (u.tag, payloadless) {
					return wire_of(types, tag.target);
				}
			}
			bail!("{} has no protobuf wire type", name())
		}
	}
}

impl AstHir for Message {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ctx.body
			.iter()
			.map(|f| {
				let name = f.expect_name()?;
				ensure!(
					matches!(f.constant("number"), Some(hir::Value::Number(n)) if *n > 0),
					"{name:?} needs a positive `number`"
				);
				if f.kind.as_str() == "oneof" {
					ensure!(
						matches!(f.constant("group"), Some(hir::Value::String(_))),
						"oneof {name:?} needs a `group` string"
					);
				}
				let ty =
					f.ty.clone()
						.ok_or_else(|| anyhow!("{name:?} has no type"))?;
				Ok((name.clone(), ty))
			})
			.collect()
	}
}

impl NativeLayout for Message {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		for f in view(native).into_iter().flatten() {
			let embed = match f.label {
				Label::Repeated => Embed::Indirect,
				_ => Embed::Boxable(BoxCost::Low),
			};
			v.slot(f.name, f.ty.target, embed);
		}
	}
}

struct Plan<'a> {
	fields: Vec<MessageField<'a>>,
	wires: Vec<Wire>,
	groups: Vec<(&'a str, Vec<usize>)>,
	by_number: Vec<usize>,
}

fn plan<'a>(types: &TypeMap, native: &'a mir::NativeType) -> Result<Plan<'a>> {
	let fields = view(native).ok_or_else(|| anyhow!("malformed message"))?;
	let wires = fields
		.iter()
		.map(|f| wire_of(types, f.ty.target))
		.collect::<Result<Vec<_>>>()?;
	let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
	for (i, f) in fields.iter().enumerate() {
		if let Label::Oneof(g) = f.label {
			match groups.iter_mut().find(|(n, _)| *n == g) {
				Some((_, members)) => members.push(i),
				None => groups.push((g, vec![i])),
			}
		}
	}
	let mut by_number: Vec<usize> = (0..fields.len()).collect();
	by_number.sort_by_key(|i| fields[*i].number);
	Ok(Plan {
		fields,
		wires,
		groups,
		by_number,
	})
}

impl NativeCodegen<Rust> for Message {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let Plan {
			fields,
			wires,
			groups,
			by_number,
		} = plan(ctx.types, ctx.native)?;
		let names = ctx.names;
		let pb = |f: &str| rust::import("crate::pds::protobuf", f);
		let pinput = &rust::import("crate::pds", "Input");
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let group_ty = |g: &str| rname(&Ident::new(g));

		let mut decl = Vec::new();
		let mut seen = Vec::new();
		for f in &fields {
			let name = rname(f.name);
			decl.push(match f.label {
				Label::Single => quote!(pub $name: Option<$(ctx.slot_ty(f.name, f.ty.target))>,),
				Label::Repeated => quote!(pub $name: Vec<$(names.ty(f.ty.target))>,),
				Label::Oneof(g) => {
					if seen.contains(&g) {
						continue;
					}
					seen.push(g);
					quote!(pub $(group_ty(g)): Option<$(group_ty(g))>,)
				}
			});
		}
		let enums: Vec<rust::Tokens> = groups
			.iter()
			.map(|(g, members)| {
				quote! {
					#[derive(Debug, Clone, PartialEq)]
					pub enum $(group_ty(g)) {
						$(for i in members join($['\r']) =>
							$(rname(fields[*i].name))($(ctx.slot_ty(fields[*i].name, fields[*i].ty.target))),
						)
					}
				}
			})
			.collect();

		let read = |i: usize, from: rust::Tokens| -> rust::Tokens {
			let f = &fields[i];
			let de = names.deserialize(f.ty.target);
			let pass = (ctx.pass_args)(f.ty);
			if wires[i] == Wire::Len {
				quote!({ let mut sub = $(pb("len"))($from)?; $de(&mut sub, $pass)? })
			} else {
				quote!($de($from, $pass)?)
			}
		};
		let arms: Vec<rust::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| {
				let name = rname(f.name);
				let w = wires[i] as u8;
				let expect = quote!($(pb("expect"))(wire, $(w.to_string()))?;);
				let body = match f.label {
					Label::Single => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$name = Some($value);)
					}
					Label::Oneof(g) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$(group_ty(g)) = Some($(group_ty(g))::$name($value));)
					}
					Label::Repeated if wires[i] == Wire::Len => {
						quote!($expect out.$name.push($(read(i, quote!(input))));)
					}
					Label::Repeated => {
						let de = names.deserialize(f.ty.target);
						let pass = (ctx.pass_args)(f.ty);
						quote! {
							if wire == 2 {
								let mut sub = $(pb("len"))(input)?;
								while sub.remaining() > 0 {
									out.$(&name).push($(&de)(&mut sub, $(&pass))?);
								}
							} else {
								$expect
								out.$(&name).push($(read(i, quote!(input))));
							}
						}
					}
				};
				quote!($(f.number.to_string()) => { $body })
			})
			.collect();

		let size = |i: usize, v: rust::Tokens| -> rust::Tokens {
			let size = names.size_of(fields[i].ty.target);
			if wires[i] == Wire::Len {
				quote!($(pb("len_size"))($size($v)))
			} else {
				quote!($size($v))
			}
		};
		let write = |i: usize, v: rust::Tokens| -> rust::Tokens {
			let f = &fields[i];
			let ser = names.serialize(f.ty.target);
			if wires[i] == Wire::Len {
				quote! {
					$(pb("write_len"))(output, $(names.size_of(f.ty.target))($(&v)));
					$ser($(&v), output)?;
				}
			} else {
				quote!($ser($v, output)?;)
			}
		};
		let mut ser = Vec::new();
		let mut sizes = Vec::new();
		for &i in &by_number {
			let f = &fields[i];
			let name = rname(f.name);
			let n = f.number.to_string();
			let w = (wires[i] as u8).to_string();
			let key = quote!($(pb("write_key"))(output, $(&n), $(&w)););
			let key_size = quote!($(pb("key_size"))($(&n)));
			match f.label {
				Label::Single | Label::Oneof(_) => {
					let pattern = match f.label {
						Label::Oneof(g) => quote!(Some($(group_ty(g))::$(&name)(v)) = &_0.$(group_ty(g))),
						_ => quote!(Some(v) = &_0.$(&name)),
					};
					ser.push(quote!(if let $(&pattern) { $key $(write(i, quote!(v))) }));
					sizes.push(quote!(if let $(&pattern) { n += $key_size + $(size(i, quote!(v))); }));
				}
				Label::Repeated if wires[i] == Wire::Len => {
					ser.push(quote!(for v in &_0.$(&name) { $key $(write(i, quote!(v))) }));
					sizes.push(quote!(for v in &_0.$(&name) { n += $key_size + $(size(i, quote!(v))); }));
				}
				Label::Repeated => {
					let total = quote!(_0.$(&name).iter().map(|v| $(size(i, quote!(v)))).sum::<usize>());
					ser.push(quote! {
						if !_0.$(&name).is_empty() {
							$(pb("write_key"))(output, $(&n), 2);
							$(pb("write_len"))(output, $(&total));
							for v in &_0.$(&name) { $(write(i, quote!(v))) }
						}
					});
					sizes.push(quote! {
						if !_0.$(&name).is_empty() {
							n += $key_size + $(pb("len_size"))($(&total));
						}
					});
				}
			}
		}

		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq, Default)]
			pub struct Ty {
				$(for d in decl join($['\r']) => $d)
			}
			$(for e in enums join($['\r']) => $e)
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Ty::default();
				while input.remaining() > 0 {
					let (number, wire) = $(pb("read_key"))(input)?;
					match number {
						$(for a in arms join($['\r']) => $a)
						_ => $(pb("skip"))(input, wire)?,
					}
				}
				Ok(out)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(for s in ser join($['\r']) => $s)
				Ok(())
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = 0;
				$(for s in sizes join($['\r']) => $s)
				n
			}
		})
	}
}

impl NativeCodegen<JavaScript> for Message {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let Plan {
			fields,
			wires,
			groups,
			by_number,
		} = plan(ctx.types, ctx.native)?;
		let names = ctx.names;
		let pb = &js::import("../pds.ts", "protobuf");
		let pinput = &js::import("../pds.ts", "Input");
		let poutput = &js::import("../pds.ts", "Output");
		let has_args = !ctx.params.is_empty();
		let args_of = |t: &mir::Type| -> js::Tokens {
			let a = (ctx.pass_args)(t);
			if a.is_empty() {
				quote!()
			} else {
				quote!(, $a)
			}
		};

		let mut decl = Vec::new();
		let mut seen = Vec::new();
		for f in &fields {
			let name = f.name.as_str();
			decl.push(match f.label {
				Label::Single => quote!($name?: $(names.ty(f.ty.target));),
				Label::Repeated => quote!($name?: Array<$(names.ty(f.ty.target))>;),
				Label::Oneof(g) => {
					if seen.contains(&g) {
						continue;
					}
					seen.push(g);
					let members = &groups.iter().find(|(n, _)| *n == g).expect("collected").1;
					quote! {
						$g?:
							$(for i in members join($['\r']) =>
								| { kind: $(quoted(fields[*i].name.as_str())); value: $(names.ty(fields[*i].ty.target)) }
							);
					}
				}
			});
		}

		let read = |i: usize, from: js::Tokens| -> js::Tokens {
			let f = &fields[i];
			let de = names.deserialize(f.ty.target);
			if wires[i] == Wire::Len {
				quote!($de($pb.len($from)$(args_of(f.ty))))
			} else {
				quote!($de($from$(args_of(f.ty))))
			}
		};
		let arms: Vec<js::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| {
				let name = f.name.as_str();
				let w = (wires[i] as u8).to_string();
				let expect = quote!($pb.expect(wire, $(&w)););
				let body = match f.label {
					Label::Single => quote!($expect out.$name = $(read(i, quote!(input)));),
					Label::Oneof(g) => {
						quote!($expect out.$g = { kind: $(quoted(name)), value: $(read(i, quote!(input))) };)
					}
					Label::Repeated if wires[i] == Wire::Len => {
						quote!($expect (out.$name ??= []).push($(read(i, quote!(input))));)
					}
					Label::Repeated => quote! {
						if (wire === 2) {
							const sub = $pb.len(input);
							const items = (out.$name ??= []);
							while (sub.remaining() > 0) {
								items.push($(read(i, quote!(sub))));
							}
						} else {
							$expect
							(out.$name ??= []).push($(read(i, quote!(input))));
						}
					},
				};
				quote!(case $(f.number.to_string()): { $body break; })
			})
			.collect();

		let size = |i: usize, v: js::Tokens| -> js::Tokens {
			let size = names.size_of(fields[i].ty.target);
			if wires[i] == Wire::Len {
				quote!($pb.lenSize($size($v)))
			} else {
				quote!($size($v))
			}
		};
		let write = |i: usize, v: js::Tokens| -> js::Tokens {
			let f = &fields[i];
			let ser = names.serialize(f.ty.target);
			if wires[i] == Wire::Len {
				quote!($pb.writeLen(output, $(names.size_of(f.ty.target))($(&v))); $ser($(&v), output);)
			} else {
				quote!($ser($v, output);)
			}
		};
		let mut ser = Vec::new();
		let mut sizes = Vec::new();
		for &i in &by_number {
			let f = &fields[i];
			let name = f.name.as_str();
			let n = f.number.to_string();
			let w = (wires[i] as u8).to_string();
			let key = quote!($pb.writeKey(output, $(&n), $(&w)););
			let key_size = quote!($pb.keySize($(&n)));
			match f.label {
				Label::Single => {
					ser.push(quote! {
						if (_0.$name !== undefined) { const v = _0.$name; $key $(write(i, quote!(v))) }
					});
					sizes.push(quote! {
						if (_0.$name !== undefined) { const v = _0.$name; n += $key_size + $(size(i, quote!(v))); }
					});
				}
				Label::Oneof(g) => {
					let test = quote!(_0.$g?.kind === $(quoted(name)));
					ser.push(quote! {
						if ($(&test)) { const v = _0.$g.value; $key $(write(i, quote!(v))) }
					});
					sizes.push(quote! {
						if ($(&test)) { const v = _0.$g.value; n += $key_size + $(size(i, quote!(v))); }
					});
				}
				Label::Repeated if wires[i] == Wire::Len => {
					ser.push(quote!(for (const v of _0.$name ?? []) { $key $(write(i, quote!(v))) }));
					sizes.push(quote! {
						for (const v of _0.$name ?? []) { n += $key_size + $(size(i, quote!(v))); }
					});
				}
				Label::Repeated => {
					let total =
						quote!(_0.$name.reduce((t: number, v) => t + $(size(i, quote!(v))), 0));
					let nonempty = quote!(_0.$name !== undefined && _0.$name.length > 0);
					ser.push(quote! {
						if ($(&nonempty)) {
							$pb.writeKey(output, $(&n), 2);
							$pb.writeLen(output, $(&total));
							for (const v of _0.$name) { $(write(i, quote!(v))) }
						}
					});
					sizes.push(quote! {
						if ($(&nonempty)) { n += $key_size + $pb.lenSize($(&total)); }
					});
				}
			}
		}

		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = {
				$(for d in decl join($['\r']) => $d)
			};
			export function serialize(_0: Ty, output: $poutput): void {
				$(for s in ser join($['\r']) => $s)
			}
			export function deserialize(input: $pinput$(if has_args => , $((ctx.args_param)()))): Ty {
				const out: Ty = {};
				while (input.remaining() > 0) {
					const [number, wire] = $pb.readKey(input);
					switch (number) {
						$(for a in arms join($['\r']) => $a)
						default:
							$pb.skip(input, wire);
					}
				}
				return out;
			}
			export function size_of(_0: Ty): number {
				let n = 0;
				$(for s in sizes join($['\r']) => $s)
				return n;
			}
		})
	}
}
