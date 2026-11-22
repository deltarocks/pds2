use anyhow::{Context as _, Result, anyhow, bail, ensure};
use genco::lang::{JavaScript, Rust, js, rust};
use genco::prelude::*;

use pds2_core::ast::Ident;
use pds2_core::generator::common::scalar_of;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, TypeKind, TypeMap, Value};
use pds2_core::specializer::eval::{Target, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx, NativeLayout,
	Scalar, SlotVisitor,
};
use pds2_specializer_bytes::{Encoder, Encoding, Input, write_varint};

pub struct Message;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wire {
	Varint = 0,
	I64 = 1,
	Len = 2,
	I32 = 5,
}

pub enum Label<'a> {
	Required,
	Implicit,
	Optional,
	Repeated,
	Oneof(&'a str),
}

#[derive(Clone, Copy)]
pub enum Zero {
	Scalar(Scalar),
	Enum(TypeId),
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
				"field" => Label::Required,
				"implicit" => Label::Implicit,
				"optional" => Label::Optional,
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

pub fn zero_of(types: &TypeMap, id: TypeId) -> Result<Zero> {
	if let Some(s) = scalar_of(types, id) {
		return Ok(Zero::Scalar(s));
	}
	let mut target = id;
	while let TypeKind::Alias(t) = &types[target].kind {
		target = t.target;
	}
	match &types[target].kind {
		TypeKind::Native(n) if crate::enumeration::view(n).is_some() => Ok(Zero::Enum(target)),
		_ => bail!(
			"{} has no zero value, an implicit field holds a scalar or an enum",
			types[id].name.readable_name()
		),
	}
}

pub fn zero(elab: &Elaborator, id: TypeId) -> Result<Expr> {
	let value = match zero_of(elab.types, id)? {
		Zero::Scalar(Scalar::Bool) => Value::Bool(false),
		Zero::Scalar(Scalar::String) => Value::String(String::new()),
		Zero::Scalar(Scalar::Bytes) => Value::Bytes(vec![]),
		Zero::Scalar(Scalar::F32 | Scalar::F64) => Value::Float(0.0),
		Zero::Scalar(_) | Zero::Enum(_) => Value::Signed(0),
	};
	elab.build(id, &Expr::Const(value))
}

fn is_zero(wire: Wire, payload: &[u8]) -> bool {
	match wire {
		Wire::Varint => payload.iter().all(|b| b & 0x7f == 0),
		Wire::I32 | Wire::I64 => payload.iter().all(|b| *b == 0),
		Wire::Len => payload.is_empty(),
	}
}

fn reaches_required(types: &TypeMap, msg: TypeId, visited: &mut Vec<TypeId>) -> bool {
	if visited.contains(&msg) {
		return false;
	}
	visited.push(msg);
	let TypeKind::Native(n) = &types[msg].kind else {
		return false;
	};
	view(n).into_iter().flatten().any(|f| match f.label {
		Label::Required => true,
		Label::Optional | Label::Oneof(_) => {
			embedded_message(types, f.ty).is_some_and(|(m, _)| reaches_required(types, m, visited))
		}
		Label::Implicit | Label::Repeated => false,
	})
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
			bail!(
				"{} is written as a record `(field: value, ...)`",
				cx.name(id)
			);
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
				Label::Required | Label::Implicit | Label::Optional => cx
					.coerce(e.clone(), f.ty.target)
					.with_context(|| format!("field {n:?}"))?,
			};
			out.push((n.clone(), e));
		}
		for f in fields.iter().filter(|f| matches!(f.label, Label::Required)) {
			ensure!(
				given.iter().any(|(n, _)| n == f.name),
				"{} needs its required field {:?}",
				cx.name(id),
				f.name
			);
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
				ensure!(
					!matches!(f.label, Label::Required),
					"required field {:?} is not set",
					f.name
				);
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
							cx.encode_use(f.ty, item, &scope, &mut buf)
								.with_context(field)?;
						}
						key(out, f.number, Wire::Len as u8);
						write_varint(out, buf.len() as u64);
						out.extend(buf);
					}
				}
				Label::Implicit => {
					let mut buf = Vec::new();
					cx.encode_use(f.ty, value, &scope, &mut buf)
						.with_context(field)?;
					if is_zero(wire, &buf) {
						continue;
					}
					key(out, f.number, wire as u8);
					if wire == Wire::Len {
						write_varint(out, buf.len() as u64);
					}
					out.extend(buf);
				}
				Label::Required | Label::Optional | Label::Oneof(_) => {
					key(out, f.number, wire as u8);
					if wire == Wire::Len {
						framed(out, f.ty, value).with_context(field)?;
					} else {
						cx.encode_use(f.ty, value, &scope, out)
							.with_context(field)?;
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
			if matches!(f.label, Label::Repeated)
				&& wire == Wire::Len as u8
				&& wires[i] != Wire::Len
			{
				let mut packed = frame(input)?;
				while packed.remaining() > 0 {
					let item = cx
						.decode_use(f.ty, &scope, &mut packed)
						.with_context(field)?;
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
			let (value, zero) = if wires[i] == Wire::Len {
				let mut sub = frame(input)?;
				let zero = sub.remaining() == 0;
				let value = cx.decode_use(f.ty, &scope, &mut sub).with_context(field)?;
				ensure!(
					sub.remaining() == 0,
					"{}: {} bytes are left in its frame",
					field(),
					sub.remaining()
				);
				(value, zero)
			} else {
				let payload = input.peek();
				let value = cx.decode_use(f.ty, &scope, input).with_context(field)?;
				(
					value,
					is_zero(wires[i], &payload[..payload.len() - input.remaining()]),
				)
			};
			match f.label {
				Label::Repeated => repeated[i].push(value),
				Label::Implicit if zero => single[i] = None,
				Label::Required | Label::Implicit | Label::Optional | Label::Oneof(_) => {
					single[i] = Some(value)
				}
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
		for (f, one) in fields.iter().zip(&single) {
			ensure!(
				!matches!(f.label, Label::Required) || one.is_some(),
				"required field {:?} of {} is missing",
				f.name,
				cx.name(t.id)
			);
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
	zeros: Vec<Option<Zero>>,
}

fn plan<'a>(types: &TypeMap, native: &'a mir::NativeType) -> Result<Plan<'a>> {
	let fields = view(native).ok_or_else(|| anyhow!("malformed message"))?;
	let wires = fields
		.iter()
		.map(|f| wire_of(types, f.ty.target))
		.collect::<Result<Vec<_>>>()?;
	let zeros = fields
		.iter()
		.map(|f| match f.label {
			Label::Implicit => zero_of(types, f.ty.target)
				.with_context(|| format!("field {:?}", f.name))
				.map(Some),
			_ => Ok(None),
		})
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
		zeros,
	})
}

fn whole(types: &TypeMap, fields: &[MessageField]) -> Vec<bool> {
	fields
		.iter()
		.map(|f| match f.label {
			Label::Required | Label::Optional | Label::Oneof(_) => embedded_message(types, f.ty)
				.is_some_and(|(m, _)| reaches_required(types, m, &mut vec![])),
			Label::Implicit | Label::Repeated => false,
		})
		.collect()
}

impl NativeCodegen<Rust> for Message {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let Plan {
			fields,
			wires,
			groups,
			by_number,
			zeros,
		} = plan(ctx.types, ctx.native)?;
		let whole = whole(ctx.types, &fields);
		let names = ctx.names;
		let pb = |f: &str| rust::import("crate::pds::protobuf", f);
		let pinput = &rust::import("crate::pds", "Input");
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let perror = &rust::import("crate::pds", "Error");
		let group_ty = |g: &str| ctx.member_type(g);
		let group_field = |g: &str| ctx.field(g);
		let has_args = !ctx.params.is_empty();
		let required: Vec<usize> = (0..fields.len())
			.filter(|i| matches!(fields[*i].label, Label::Required))
			.collect();
		let seen = |i: usize| -> rust::Tokens {
			let k = required
				.iter()
				.position(|r| *r == i)
				.expect("a required field");
			quote!(seen[$(k.to_string())] = true;)
		};
		let pending = |i: usize| format!("pending_{i}");
		let clear = |i: usize| -> rust::Tokens {
			let Label::Oneof(g) = fields[i].label else {
				return quote!();
			};
			let others = groups
				.iter()
				.filter(|(n, _)| *n == g)
				.flat_map(|(_, m)| m.iter().copied())
				.filter(|j| *j != i && whole[*j]);
			quote!($(for j in others => $(pending(j)) = None;))
		};

		let mut decl = Vec::new();
		let mut seen_groups = Vec::new();
		for f in &fields {
			let name = ctx.field(f.name.as_str());
			decl.push(match f.label {
				Label::Required | Label::Implicit => {
					quote!(pub $name: $(ctx.slot_ty(f.name, f.ty.target)),)
				}
				Label::Optional => quote!(pub $name: Option<$(ctx.slot_ty(f.name, f.ty.target))>,),
				Label::Repeated => quote!(pub $name: Vec<$(names.ty(f.ty.target))>,),
				Label::Oneof(g) => {
					if seen_groups.contains(&g) {
						continue;
					}
					seen_groups.push(g);
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

		let decode = |i: usize, from: rust::Tokens| -> rust::Tokens {
			let f = &fields[i];
			quote!($(names.deserialize(f.ty.target))($from, $((ctx.pass_args)(f.ty)))?)
		};
		let read = |i: usize, from: rust::Tokens| -> rust::Tokens {
			if wires[i] == Wire::Len {
				quote!({ let mut sub = $(pb("len"))($from)?; $(decode(i, quote!(&mut sub))) })
			} else {
				decode(i, from)
			}
		};
		let extend = |f: &MessageField, from: rust::Tokens| -> Result<Option<rust::Tokens>> {
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
				$(names.item(msg, "deserialize_extend"))($from, $pass $target)?;
			}))
		};
		let framed = || quote!(&mut $(pb("len"))(input)?);
		let arms: Vec<rust::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| -> Result<rust::Tokens> {
				let name = ctx.field(f.name.as_str());
				let w = wires[i] as u8;
				let expect = quote!($(pb("expect"))(wire, $(w.to_string()))?;);
				if whole[i] {
					let mark = matches!(f.label, Label::Required).then(|| seen(i));
					let replace = match f.label {
						Label::Oneof(g) => {
							let (field, ty) = (group_field(g), group_ty(g));
							let v = ctx.variant(f.name.as_str());
							Some(quote! {
								if !matches!(out.$(&field), Some($(&ty)::$(&v)(_))) {
									out.$(&field) = None;
								}
							})
						}
						_ => None,
					};
					return Ok(quote! {
						$(f.number.to_string()) => {
							$expect
							$(clear(i))
							$replace
							$(pending(i)).get_or_insert_with(Vec::new).extend_from_slice($(pb("len"))(input)?.rest());
							$mark
						}
					});
				}
				let merge = match f.label {
					Label::Repeated | Label::Implicit => None,
					_ => extend(f, framed())?,
				};
				let body = match (&f.label, merge) {
					(Label::Implicit, _) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$name = $value;)
					}
					(Label::Required, None) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$name = $value; $(seen(i)))
					}
					(Label::Required, Some(extend)) => quote! {
						$expect
						{
							let v = &mut out.$(&name);
							$extend
						}
						$(seen(i))
					},
					(Label::Optional, None) => {
						let value = ctx.slot_new(f.name, read(i, quote!(input)));
						quote!($expect out.$name = Some($value);)
					}
					(Label::Optional, Some(extend)) => {
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
							None => quote!($expect $(clear(i)) out.$field = Some($ty::$v($value));),
							Some(extend) => quote! {
								$expect
								$(clear(i))
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
		let finish: Vec<rust::Tokens> = (0..fields.len())
			.filter(|i| whole[*i])
			.map(|i| -> Result<rust::Tokens> {
				let f = &fields[i];
				let name = ctx.field(f.name.as_str());
				let extend = extend(f, quote!(input))?
					.ok_or_else(|| anyhow!("{:?} is merged but holds no message", f.name))?;
				let value = ctx.slot_new(f.name, decode(i, quote!(input)));
				let apply = match f.label {
					Label::Required => quote! {
						if check {
							out.$(&name) = $value;
						} else {
							let v = &mut out.$(&name);
							$extend
						}
					},
					Label::Optional => quote! {
						match &mut out.$(&name) {
							Some(v) => { $extend }
							None => out.$(&name) = Some($value),
						}
					},
					Label::Oneof(g) => {
						let v = ctx.variant(f.name.as_str());
						let (field, ty) = (group_field(g), group_ty(g));
						quote! {
							match &mut out.$(&field) {
								Some($(&ty)::$(&v)(v)) => { $extend }
								_ => out.$(&field) = Some($(&ty)::$(&v)($value)),
							}
						}
					}
					Label::Implicit | Label::Repeated => bail!("{:?} can't be merged", f.name),
				};
				Ok(quote! {
					if let Some(bytes) = $(pending(i)) {
						let input = &mut $pinput::new(&bytes);
						$apply
					}
				})
			})
			.collect::<Result<_>>()?;
		let pendings: Vec<rust::Tokens> = (0..fields.len())
			.filter(|i| whole[*i])
			.map(|i| quote!(let mut $(pending(i)): Option<Vec<u8>> = None;))
			.collect();
		let body = quote! {
			$(for p in pendings join($['\r']) => $p)
			while input.remaining() > 0 {
				let (number, wire) = $(pb("read_key"))(input)?;
				match number {
					$(for a in arms join($['\r']) => $a)
					_ => $(pb("skip"))(input, wire)?,
				}
			}
			$(for f in finish join($['\r']) => $f)
		};
		let functions = if required.is_empty() {
			quote! {
				pub fn deserialize_extend(input: &mut $pinput, $((ctx.args_param)()) out: &mut Ty) -> $presult<()> {
					$body
					Ok(())
				}
				pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
					let mut out = Ty::default();
					deserialize_extend(input, $(if has_args => args,) &mut out)?;
					Ok(out)
				}
			}
		} else {
			quote! {
				fn deserialize_into(input: &mut $pinput, $((ctx.args_param)()) out: &mut Ty, check: bool) -> $presult<()> {
					let mut seen = [false; $(required.len().to_string())];
					$body
					if check && seen.contains(&false) {
						return Err($perror::InvalidValue);
					}
					Ok(())
				}
				pub fn deserialize_extend(input: &mut $pinput, $((ctx.args_param)()) out: &mut Ty) -> $presult<()> {
					deserialize_into(input, $(if has_args => args,) out, false)
				}
				pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
					let mut out = Ty::default();
					deserialize_into(input, $(if has_args => args,) &mut out, true)?;
					Ok(out)
				}
			}
		};

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
		let nonzero = |i: usize| -> rust::Tokens {
			match zeros[i] {
				Some(Zero::Scalar(Scalar::F32 | Scalar::F64)) => quote!(v.to_bits() != 0),
				_ => quote!(*v != <$(names.ty(fields[i].ty.target))>::default()),
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
				Label::Required => {
					ser.push(quote!({ let v = &_0.$(&name); $key $(write(i, quote!(v))) }));
					sizes.push(
						quote!({ let v = &_0.$(&name); n += $key_size + $(size(i, quote!(v))); }),
					);
				}
				Label::Implicit => {
					let set = nonzero(i);
					ser.push(
						quote!({ let v = &_0.$(&name); if $(&set) { $key $(write(i, quote!(v))) } }),
					);
					sizes.push(
						quote!({ let v = &_0.$(&name); if $(&set) { n += $key_size + $(size(i, quote!(v))); } }),
					);
				}
				Label::Optional | Label::Oneof(_) => {
					let pattern = match f.label {
						Label::Oneof(g) => {
							quote!(Some($(group_ty(g))::$(ctx.variant(f.name.as_str()))(v)) = &_0.$(group_field(g)))
						}
						_ => quote!(Some(v) = &_0.$(&name)),
					};
					ser.push(quote!(if let $(&pattern) { $key $(write(i, quote!(v))) }));
					sizes.push(
						quote!(if let $(&pattern) { n += $key_size + $(size(i, quote!(v))); }),
					);
				}
				Label::Repeated if wires[i] == Wire::Len => {
					ser.push(quote!(for v in &_0.$(&name) { $key $(write(i, quote!(v))) }));
					sizes.push(
						quote!(for v in &_0.$(&name) { n += $key_size + $(size(i, quote!(v))); }),
					);
				}
				Label::Repeated => {
					let total =
						quote!(_0.$(&name).iter().map(|v| $(size(i, quote!(v)))).sum::<usize>());
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
			$functions
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
				Label::Required | Label::Implicit => {
					quote!($field: $(cx.names.slot_new(id, f.name, value)),)
				}
				Label::Optional => {
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
			zeros,
		} = plan(ctx.types, ctx.native)?;
		let whole = whole(ctx.types, &fields);
		let names = ctx.names;
		let pb = &ctx.runtime("protobuf");
		let pinput = &ctx.runtime("Input");
		let poutput = &ctx.runtime("Output");
		let perror = &ctx.runtime("PdsError");
		let has_args = !ctx.params.is_empty();
		let args_of = |t: &mir::Type| -> js::Tokens {
			let a = (ctx.pass_args)(t);
			if a.is_empty() { quote!() } else { quote!(, $a) }
		};
		let zero_lit = |i: usize| -> Result<js::Tokens> {
			(ctx.lower)(&zero(ctx.elab, fields[i].ty.target)?, false)
		};
		let pending = |i: usize| format!("pending_{i}");
		let clear = |i: usize| -> js::Tokens {
			let Label::Oneof(g) = fields[i].label else {
				return quote!();
			};
			let others = groups
				.iter()
				.filter(|(n, _)| *n == g)
				.flat_map(|(_, m)| m.iter().copied())
				.filter(|j| *j != i && whole[*j]);
			quote!($(for j in others => $(pending(j)) = undefined;))
		};

		let mut decl = Vec::new();
		let mut seen = Vec::new();
		for f in &fields {
			let name = f.name.as_str();
			decl.push(match f.label {
				Label::Required | Label::Implicit => quote!($name: $(names.ty(f.ty.target));),
				Label::Optional => quote!($name?: $(names.ty(f.ty.target));),
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

		let decode = |i: usize, from: js::Tokens| -> js::Tokens {
			let f = &fields[i];
			quote!($(names.deserialize(f.ty.target))($from$(args_of(f.ty))))
		};
		let read = |i: usize, from: js::Tokens| -> js::Tokens {
			if wires[i] == Wire::Len {
				decode(i, quote!($pb.len($from)))
			} else {
				decode(i, from)
			}
		};
		let extend =
			|f: &MessageField, from: js::Tokens, into: js::Tokens| -> Result<Option<js::Tokens>> {
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
				Ok(Some(quote!($ext($from$pass, $into);)))
			};
		let arms: Vec<js::Tokens> = fields
			.iter()
			.enumerate()
			.map(|(i, f)| -> Result<js::Tokens> {
				let name = f.name.as_str();
				let w = (wires[i] as u8).to_string();
				let expect = quote!($pb.expect(wire, $(&w)););
				let framed = quote!($pb.len(input));
				let body = if whole[i] {
					let replace = match f.label {
						Label::Oneof(g) => Some(quote! {
							if (out.$g?.kind !== $(quoted(name))) {
								out.$g = undefined;
							}
						}),
						_ => None,
					};
					quote! {
						$expect
						$(clear(i))
						$replace
						($(pending(i)) ??= new $poutput()).write($pb.len(input).read_rest());
					}
				} else {
					match f.label {
						Label::Implicit => quote!($expect out.$name = $(read(i, quote!(input)));),
						Label::Required | Label::Optional => {
							let set = quote!(out.$name = $(read(i, quote!(input))););
							match extend(f, framed, quote!(out.$name))? {
								None => quote!($expect $set),
								Some(merge) => quote! {
									$expect
									if (out.$name === undefined) { $set } else { $merge }
								},
							}
						}
						Label::Oneof(g) => {
							let set = quote!(out.$g = { kind: $(quoted(name)), value: $(read(i, quote!(input))) };);
							match extend(f, framed, quote!(out.$g.value))? {
								None => quote!($expect $(clear(i)) $set),
								Some(merge) => quote! {
									$expect
									$(clear(i))
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
					}
				};
				Ok(quote!(case $(f.number.to_string()): { $body break; }))
			})
			.collect::<Result<_>>()?;
		let finish: Vec<js::Tokens> = (0..fields.len())
			.filter(|i| whole[*i])
			.map(|i| -> Result<js::Tokens> {
				let f = &fields[i];
				let name = f.name.as_str();
				let value = decode(i, quote!(sub));
				let apply = match f.label {
					Label::Required | Label::Optional => {
						let merge = extend(f, quote!(sub), quote!(out.$name))?
							.ok_or_else(|| anyhow!("{name:?} is merged but holds no message"))?;
						quote!(if (out.$name === undefined) { out.$name = $value; } else { $merge })
					}
					Label::Oneof(g) => {
						let merge = extend(f, quote!(sub), quote!(out.$g.value))?
							.ok_or_else(|| anyhow!("{name:?} is merged but holds no message"))?;
						quote! {
							if (out.$g?.kind === $(quoted(name))) { $merge } else {
								out.$g = { kind: $(quoted(name)), value: $value };
							}
						}
					}
					Label::Implicit | Label::Repeated => bail!("{name:?} can't be merged"),
				};
				Ok(quote! {
					if ($(pending(i)) !== undefined) {
						const sub = new $pinput($(pending(i)).into_inner());
						$apply
					}
				})
			})
			.collect::<Result<_>>()?;
		let pendings: Vec<js::Tokens> = (0..fields.len())
			.filter(|i| whole[*i])
			.map(|i| quote!(let $(pending(i)): $poutput | undefined = undefined;))
			.collect();
		let defaults = (0..fields.len())
			.filter(|i| matches!(fields[*i].label, Label::Implicit))
			.map(|i| Ok(quote!($(fields[i].name.as_str()): $(zero_lit(i)?))))
			.collect::<Result<Vec<js::Tokens>>>()?;
		let required: Vec<&str> = fields
			.iter()
			.filter(|f| matches!(f.label, Label::Required))
			.map(|f| f.name.as_str())
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
		let nonzero = |i: usize| -> Result<js::Tokens> {
			Ok(match zeros[i] {
				Some(Zero::Scalar(Scalar::F32 | Scalar::F64)) => quote!(!Object.is(v, 0)),
				Some(Zero::Scalar(Scalar::Bytes)) => quote!(v.length !== 0),
				Some(Zero::Enum(e)) => {
					let tag = match &ctx.types[e].kind {
						TypeKind::Native(n) => crate::enumeration::view(n).map(|v| v.tag.target),
						TypeKind::Alias(_) => None,
					}
					.ok_or_else(|| anyhow!("malformed enum"))?;
					let lit =
						(ctx.lower)(&ctx.elab.build(tag, &Expr::Const(Value::Signed(0)))?, false)?;
					quote!($(names.item(e, "number"))(v) !== $lit)
				}
				Some(Zero::Scalar(_)) => quote!(v !== $(zero_lit(i)?)),
				None => bail!("{:?} is not implicit", fields[i].name),
			})
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
				Label::Required => {
					ser.push(quote!({ const v = _0.$name; $key $(write(i, quote!(v))) }));
					sizes.push(
						quote!({ const v = _0.$name; n += $key_size + $(size(i, quote!(v))); }),
					);
				}
				Label::Implicit => {
					let set = nonzero(i)?;
					ser.push(quote! {
						{ const v = _0.$name; if ($(&set)) { $key $(write(i, quote!(v))) } }
					});
					sizes.push(quote! {
						{ const v = _0.$name; if ($(&set)) { n += $key_size + $(size(i, quote!(v))); } }
					});
				}
				Label::Optional => {
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
					ser.push(
						quote!(for (const v of _0.$name ?? []) { $key $(write(i, quote!(v))) }),
					);
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

		let out_decl = if required.is_empty() {
			quote!(const out: Ty = { $(for d in defaults join(, ) => $d) };)
		} else {
			quote!(const out = { $(for d in defaults join(, ) => $d) } as Ty;)
		};
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = {
				$(for d in decl join($['\r']) => $d)
			};
			export function serialize(_0: Ty, output: $poutput): void {
				$(for s in ser join($['\r']) => $s)
			}
			export function deserialize_extend(input: $pinput$(if has_args => , $((ctx.args_param)())), out: Ty): void {
				$(for p in pendings join($['\r']) => $p)
				while (input.remaining() > 0) {
					const [number, wire] = $pb.readKey(input);
					switch (number) {
						$(for a in arms join($['\r']) => $a)
						default:
							$pb.skip(input, wire);
					}
				}
				$(for f in finish join($['\r']) => $f)
			}
			export function deserialize(input: $pinput$(if has_args => , $((ctx.args_param)()))): Ty {
				$out_decl
				deserialize_extend(input$(if has_args => , args), out);
				$(for r in required join($['\r']) =>
					if (out.$r === undefined) {
						throw new $perror("InvalidValue", $(quoted(format!("missing required field {r}"))));
					}
				)
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
		let given = entries(&fields, c)?;
		let mut parts = Vec::new();
		for (f, e) in &given {
			let value = (cx.lower)(e)?;
			parts.push(match f.label {
				Label::Oneof(g) => {
					quote!($g: { kind: $(quoted(f.name.as_str())), value: $value })
				}
				_ => quote!($(f.name.as_str()): $value),
			});
		}
		for f in &fields {
			if matches!(f.label, Label::Implicit) && !given.iter().any(|(g, _)| g.name == f.name) {
				let value = (cx.lower)(&zero(cx.elab, f.ty.target)?)?;
				parts.push(quote!($(f.name.as_str()): $value));
			}
		}
		Ok(quote!({ $(for p in parts join(, ) => $p) }))
	}
}
