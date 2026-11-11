use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::decode::Input;
use pds2_core::specializer::encode::{write_varint, Encoder, Encoding, Target, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx, NativeLayout,
	SlotVisitor,
};
use pds2_core::ast::Ident;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, TypeKind, TypeMap, Value};

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

fn embedded_message(types: &TypeMap, u: &mir::Type) -> Option<(TypeId, Vec<TypeId>)> {
	let mut chain = vec![u.target];
	let mut id = u.target;
	while let TypeKind::Alias(t) = &types[id].kind {
		id = t.target;
		chain.push(id);
	}
	match &types[id].kind {
		TypeKind::Native(n) if is_message(n) => Some((id, chain)),
		_ => None,
	}
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
			if let Some(e) = crate::enumeration::view(n) {
				return wire_of(types, e.tag.target);
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
	fn members(&self, native: &mir::NativeType) -> Members {
		let mut out = Members::default();
		for f in view(native).into_iter().flatten() {
			match f.label {
				Label::Oneof(g) => {
					out.variants.push(f.name.clone());
					let g = Ident::new(g);
					if !out.types.contains(&g) {
						out.fields.push(g.clone());
						out.types.push(g);
					}
				}
				_ => out.fields.push(f.name.clone()),
			}
		}
		out
	}
}

impl ValueModel for Message {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let Expr::Record(given) = value else {
			bail!("{} is written as a record `(field: value, ...)`", cx.name(id));
		};
		let fields = view(native).ok_or_else(|| anyhow!("malformed message"))?;
		let mut groups = Vec::new();
		let mut out = Vec::new();
		for (n, e) in given {
			let f = fields
				.iter()
				.find(|f| f.name == n)
				.ok_or_else(|| anyhow!("{} has no field {n:?}", cx.name(id)))?;
			let e = match f.label {
				Label::Repeated => {
					let Expr::List(items) = e else {
						bail!("repeated field {n:?} is written as a list `[...]`");
					};
					Expr::List(
						items
							.iter()
							.enumerate()
							.map(|(i, item)| {
								cx.coerce(item.clone(), f.ty.target)
									.with_context(|| format!("item {i}"))
							})
							.collect::<Result<_>>()
							.with_context(|| format!("field {n:?}"))?,
					)
				}
				Label::Oneof(g) => {
					ensure!(!groups.contains(&g), "oneof {g:?} is set twice, by {n:?}");
					groups.push(g);
					cx.coerce(e.clone(), f.ty.target)
						.with_context(|| format!("field {n:?}"))?
				}
				Label::Single => cx
					.coerce(e.clone(), f.ty.target)
					.with_context(|| format!("field {n:?}"))?,
			};
			out.push((n.clone(), e));
		}
		Ok(Construction::Record(out))
	}
}

impl Encoder for Message {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let Plan {
			fields,
			wires,
			by_number,
			..
		} = plan(cx.types(), v.native)?;
		let given = entries(&fields, v.c)?;
		let scope = v.scope();
		let key = |out: &mut Vec<u8>, number: u32, wire: u8| {
			write_varint(out, (u64::from(number) << 3) | u64::from(wire))
		};
		let framed = |out: &mut Vec<u8>, ty: &mir::Type, value: &Expr| -> Result<()> {
			let mut buf = Vec::new();
			cx.encode_use(ty, value, &scope, &mut buf)?;
			write_varint(out, buf.len() as u64);
			out.extend(buf);
			Ok(())
		};
		for &i in &by_number {
			let f = &fields[i];
			let Some((_, value)) = given.iter().find(|(g, _)| g.name == f.name) else {
				continue;
			};
			let wire = wires[i];
			let field = || format!("field {:?}", f.name);
			match f.label {
				Label::Repeated => {
					let Expr::List(items) = value else {
						bail!("repeated field {:?} is not a list", f.name);
					};
					if wire == Wire::Len {
						for item in items {
							key(out, f.number, Wire::Len as u8);
							framed(out, f.ty, item).with_context(field)?;
						}
					} else if !items.is_empty() {
						let mut buf = Vec::new();
						for item in items {
							cx.encode_use(f.ty, item, &scope, &mut buf).with_context(field)?;
						}
						key(out, f.number, Wire::Len as u8);
						write_varint(out, buf.len() as u64);
						out.extend(buf);
					}
				}
				_ => {
					key(out, f.number, wire as u8);
					if wire == Wire::Len {
						framed(out, f.ty, value).with_context(field)?;
					} else {
						cx.encode_use(f.ty, value, &scope, out).with_context(field)?;
					}
				}
			}
		}
		Ok(())
	}

	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let Plan {
			fields,
			wires,
			groups,
			..
		} = plan(cx.types(), t.native)?;
		let scope = t.scope();
		let mut single: Vec<Option<Expr>> = vec![None; fields.len()];
		let mut repeated: Vec<Vec<Expr>> = vec![vec![]; fields.len()];
		let mut merged: Vec<Option<(usize, Vec<u8>)>> = vec![None; fields.len()];
		let mergeable: Vec<bool> = fields
			.iter()
			.map(|f| {
				!matches!(f.label, Label::Repeated) && embedded_message(cx.types(), f.ty).is_some()
			})
			.collect();
		while input.remaining() > 0 {
			let at = input.position();
			let key = input.varint()?;
			let (number, wire) = (key >> 3, (key & 7) as u8);
			let Some(i) = fields.iter().position(|f| u64::from(f.number) == number) else {
				skip(input, wire).with_context(|| format!("byte {at}: unknown field {number}"))?;
				cx.skip(format!("unknown field {number} of {}", cx.name(t.id)));
				continue;
			};
			let f = &fields[i];
			let field = || format!("field {:?}", f.name);
			if matches!(f.label, Label::Repeated) && wire == Wire::Len as u8 && wires[i] != Wire::Len {
				let mut packed = frame(input)?;
				while packed.remaining() > 0 {
					let item = cx.decode_use(f.ty, &scope, &mut packed).with_context(field)?;
					repeated[i].push(item);
				}
				continue;
			}
			ensure!(
				wire == wires[i] as u8,
				"byte {at}: field {:?} has wire type {wire}, expected {}",
				f.name,
				wires[i] as u8
			);
			if let Label::Oneof(g) = f.label {
				for (_, members) in groups.iter().filter(|(n, _)| *n == g) {
					for &j in members.iter().filter(|j| **j != i || !mergeable[i]) {
						single[j] = None;
						merged[j] = None;
					}
				}
			}
			if mergeable[i] {
				let mut sub = frame(input)?;
				let base = sub.position();
				merged[i]
					.get_or_insert_with(|| (base, vec![]))
					.1
					.extend_from_slice(sub.rest());
				continue;
			}
			let value = if wires[i] == Wire::Len {
				let mut sub = frame(input)?;
				let value = cx.decode_use(f.ty, &scope, &mut sub).with_context(field)?;
				ensure!(
					sub.remaining() == 0,
					"{}: {} bytes are left in its frame",
					field(),
					sub.remaining()
				);
				value
			} else {
				cx.decode_use(f.ty, &scope, input).with_context(field)?
			};
			match f.label {
				Label::Repeated => repeated[i].push(value),
				Label::Single | Label::Oneof(_) => single[i] = Some(value),
			}
		}
		for (i, m) in merged.into_iter().enumerate() {
			let Some((base, bytes)) = m else {
				continue;
			};
			let f = &fields[i];
			let field = || format!("field {:?}", f.name);
			let mut sub = Input::at(&bytes, base);
			let value = cx.decode_use(f.ty, &scope, &mut sub).with_context(field)?;
			ensure!(
				sub.remaining() == 0,
				"{}: {} bytes are left in its frame",
				field(),
				sub.remaining()
			);
			single[i] = Some(value);
		}
		let record = fields
			.iter()
			.zip(single.into_iter().zip(repeated))
			.filter_map(|(f, (one, many))| match f.label {
				Label::Repeated => (!many.is_empty()).then(|| (f.name.clone(), Expr::List(many))),
				_ => one.map(|v| (f.name.clone(), v)),
			})
			.collect();
		Ok(Construction::Record(record))
	}
}

fn frame<'b>(input: &mut Input<'b>) -> Result<Input<'b>> {
	let len = input.varint()?;
	input.sub(usize::try_from(len).map_err(|_| anyhow!("length {len} is too large"))?)
}

fn skip(input: &mut Input, wire: u8) -> Result<()> {
	match wire {
		0 => {
			input.varint()?;
		}
		1 => {
			input.take(8)?;
		}
		2 => {
			frame(input)?;
		}
		5 => {
			input.take(4)?;
		}
		w => bail!("wire type {w} can't be skipped"),
	}
	Ok(())
}

fn entries<'f, 'c>(
	fields: &'f [MessageField<'f>],
	c: &'c Construction,
) -> Result<Vec<(&'f MessageField<'f>, &'c Expr)>> {
	let Construction::Record(given) = c else {
		bail!("message is constructed from a record, got {c:?}");
	};
	given
		.iter()
		.map(|(n, e)| {
			let f = fields
				.iter()
				.find(|f| f.name == n)
				.ok_or_else(|| anyhow!("no message field {n:?}"))?;
			Ok((f, e))
		})
		.collect()
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
		let group_ty = |g: &str| ctx.member_type(g);
		let group_field = |g: &str| ctx.field(g);
		let has_args = !ctx.params.is_empty();

		let mut decl = Vec::new();
		let mut seen = Vec::new();
		for f in &fields {
			let name = ctx.field(f.name.as_str());
			decl.push(match f.label {
				Label::Single => quote!(pub $name: Option<$(ctx.slot_ty(f.name, f.ty.target))>,),
				Label::Repeated => quote!(pub $name: Vec<$(names.ty(f.ty.target))>,),
				Label::Oneof(g) => {
					if seen.contains(&g) {
						continue;
					}
					seen.push(g);
					quote!(pub $(group_field(g)): Option<$(group_ty(g))>,)
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
							$(ctx.variant(fields[*i].name.as_str()))($(ctx.slot_ty(fields[*i].name, fields[*i].ty.target))),
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
		let extend = |f: &MessageField| -> Result<Option<rust::Tokens>> {
			let Some((msg, chain)) = embedded_message(ctx.types, f.ty) else {
				return Ok(None);
			};
			let pass = if msg == f.ty.target {
				(ctx.pass_args)(f.ty)
			} else {
				ensure!(
					ctx.types[msg].params.is_empty(),
					"{:?} reaches a message with arguments through an alias",
					f.name
				);
				quote!()
			};
			let check = chain.iter().any(|t| names.guarded(*t)).then(|| {
				let check = rust::import("crate::pds::stack", "check").qualified();
				quote!($check()?;)
			});
			let target = if ctx.boxed(f.name) {
				quote!(&mut **v)
			} else {
				quote!(v)
			};
			Ok(Some(quote! {
				$check
				$(names.item(msg, "deserialize_extend"))(&mut $(pb("len"))(input)?, $pass $target)?;
			}))
		};
		let arms: Vec<rust::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| -> Result<rust::Tokens> {
				let name = ctx.field(f.name.as_str());
				let w = wires[i] as u8;
				let expect = quote!($(pb("expect"))(wire, $(w.to_string()))?;);
				let merge = match f.label {
					Label::Repeated => None,
					_ => extend(f)?,
				};
				let body = match (&f.label, merge) {
					(Label::Single, None) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$name = Some($value);)
					}
					(Label::Single, Some(extend)) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote! {
							$expect
							match &mut out.$(&name) {
								Some(v) => { $extend }
								None => out.$(&name) = Some($value),
							}
						}
					}
					(Label::Oneof(g), extend) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						let v = ctx.variant(f.name.as_str());
						let (field, ty) = (group_field(g), group_ty(g));
						match extend {
							None => quote!($expect out.$field = Some($ty::$v($value));),
							Some(extend) => quote! {
								$expect
								match &mut out.$(&field) {
									Some($(&ty)::$(&v)(v)) => { $extend }
									_ => out.$(&field) = Some($(&ty)::$(&v)($value)),
								}
							},
						}
					}
					(Label::Repeated, _) if wires[i] == Wire::Len => {
						quote!($expect out.$name.push($(read(i, quote!(input))));)
					}
					(Label::Repeated, _) => {
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
				Ok(quote!($(f.number.to_string()) => { $body }))
			})
			.collect::<Result<_>>()?;

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
			let name = ctx.field(f.name.as_str());
			let n = f.number.to_string();
			let w = (wires[i] as u8).to_string();
			let key = quote!($(pb("write_key"))(output, $(&n), $(&w)););
			let key_size = quote!($(pb("key_size"))($(&n)));
			match f.label {
				Label::Single | Label::Oneof(_) => {
					let pattern = match f.label {
						Label::Oneof(g) => quote!(Some($(group_ty(g))::$(ctx.variant(f.name.as_str()))(v)) = &_0.$(group_field(g))),
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
			pub struct $(ctx.nominal()) {
				$(for d in decl join($['\r']) => $d)
			}
			$(for e in enums join($['\r']) => $e)
			pub fn deserialize_extend(input: &mut $pinput, $((ctx.args_param)()) out: &mut Ty) -> $presult<()> {
				while input.remaining() > 0 {
					let (number, wire) = $(pb("read_key"))(input)?;
					match number {
						$(for a in arms join($['\r']) => $a)
						_ => $(pb("skip"))(input, wire)?,
					}
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Ty::default();
				deserialize_extend(input, $(if has_args => args,) &mut out)?;
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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let fields = view(native).ok_or_else(|| anyhow!("malformed message"))?;
		let mut parts = Vec::new();
		for (f, e) in entries(&fields, c)? {
			let value = (cx.lower)(e)?;
			let field = cx.names.field(id, f.name.as_str());
			parts.push(match f.label {
				Label::Single => {
					quote!($field: Some($(cx.names.slot_new(id, f.name, value))),)
				}
				Label::Repeated => quote!($field: $value,),
				Label::Oneof(g) => {
					let member = cx.names.slot_new(id, f.name, value);
					let group = cx.names.item(id, &cx.names.member_type(id, g));
					let variant = cx.names.variant(id, f.name.as_str());
					quote!($(cx.names.field(id, g)): Some($group::$variant($member)),)
				}
			});
		}
		Ok(quote!($(cx.names.ty(id)) { $(for p in parts => $p) ..Default::default() }))
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
		let pb = &ctx.runtime("protobuf");
		let pinput = &ctx.runtime("Input");
		let poutput = &ctx.runtime("Output");
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
		let extend = |f: &MessageField, into: js::Tokens| -> Result<Option<js::Tokens>> {
			let Some((msg, _)) = embedded_message(ctx.types, f.ty) else {
				return Ok(None);
			};
			let pass = if msg == f.ty.target {
				args_of(f.ty)
			} else {
				ensure!(
					ctx.types[msg].params.is_empty(),
					"{:?} reaches a message with arguments through an alias",
					f.name
				);
				quote!()
			};
			let ext = names.item(msg, "deserialize_extend");
			Ok(Some(quote!($ext($pb.len(input)$pass, $into);)))
		};
		let arms: Vec<js::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| -> Result<js::Tokens> {
				let name = f.name.as_str();
				let w = (wires[i] as u8).to_string();
				let expect = quote!($pb.expect(wire, $(&w)););
				let body = match f.label {
					Label::Single => {
						let set = quote!(out.$name = $(read(i, quote!(input))););
						match extend(f, quote!(out.$name))? {
							None => quote!($expect $set),
							Some(merge) => quote! {
								$expect
								if (out.$name === undefined) { $set } else { $merge }
							},
						}
					}
					Label::Oneof(g) => {
						let set = quote!(out.$g = { kind: $(quoted(name)), value: $(read(i, quote!(input))) };);
						match extend(f, quote!(out.$g.value))? {
							None => quote!($expect $set),
							Some(merge) => quote! {
								$expect
								if (out.$g?.kind === $(quoted(name))) { $merge } else { $set }
							},
						}
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
				Ok(quote!(case $(f.number.to_string()): { $body break; }))
			})
			.collect::<Result<_>>()?;

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
			export function deserialize_extend(input: $pinput$(if has_args => , $((ctx.args_param)())), out: Ty): void {
				while (input.remaining() > 0) {
					const [number, wire] = $pb.readKey(input);
					switch (number) {
						$(for a in arms join($['\r']) => $a)
						default:
							$pb.skip(input, wire);
					}
				}
			}
			export function deserialize(input: $pinput$(if has_args => , $((ctx.args_param)()))): Ty {
				const out: Ty = {};
				deserialize_extend(input$(if has_args => , args), out);
				return out;
			}
			export function size_of(_0: Ty): number {
				let n = 0;
				$(for s in sizes join($['\r']) => $s)
				return n;
			}
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<js::Tokens> {
		let fields = view(native).ok_or_else(|| anyhow!("malformed message"))?;
		let mut parts = Vec::new();
		for (f, e) in entries(&fields, c)? {
			let value = (cx.lower)(e)?;
			parts.push(match f.label {
				Label::Single | Label::Repeated => quote!($(f.name.as_str()): $value),
				Label::Oneof(g) => {
					quote!($g: { kind: $(quoted(f.name.as_str())), value: $value })
				}
			});
		}
		Ok(quote!({ $(for p in parts join(, ) => $p) }))
	}
}
