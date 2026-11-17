use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{rust, Rust};
use genco::prelude::*;
use pds2_core::ast::Ident;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction, Expr, Value};
use pds2_core::specializer::eval::{bits_of, int_of, signed, width, Target, Val};
use pds2_core::specializer::expr::{describe, Elaborator, ValueModel};
use pds2_core::specializer::reflect::{list_of, use_of};
use pds2_core::specializer::{
	construct_vec, list_value, pds_io, specialization_use, vec_of_value, visit_specialization,
	AstHir, BoxCost, Embed, LowerCx, Names, NativeCodegen, NativeCodegenCtx, NativeLayout, Scalar,
	SlotVisitor, VALUE,
};
use pds_bits_runtime::csn1;

use crate::pattern::{literal, pattern_arg, prefix_free, size_arg};
use crate::wire::{choose, read, skip, Encoder, Encoding, Input, Output};

pub fn runtime(item: &str) -> rust::Tokens {
	quote!($(rust::import("crate::pds::csn1", item)))
}

pub fn invalid() -> rust::Tokens {
	quote!($(rust::import("crate::pds", "Error"))::InvalidValue)
}

pub fn no_view<L: genco::lang::Lang>(ctx: &NativeCodegenCtx<L>) -> Result<()> {
	ensure!(!ctx.view, "CSN.1 natives have no views");
	Ok(())
}

fn unit_value(cx: &Elaborator, id: TypeId, value: &Expr) -> Result<Construction> {
	match value {
		Expr::Record(fields) if fields.is_empty() => Ok(Construction::Record(vec![])),
		v => bail!("{} carries no data, write it as `()`, got {}", cx.name(id), describe(v)),
	}
}

pub struct Uint;

struct UintShape {
	size: u32,
	scalar: Scalar,
}

fn uint_shape(native: &mir::NativeType) -> Result<UintShape> {
	let size = size_arg(native, "size")?;
	let scalar = native
		.scalar
		.as_ref()
		.and_then(|s| Scalar::parse(s.as_str()))
		.ok_or_else(|| anyhow!("uint needs `@scalar` of an integer or bool"))?;
	let max = match scalar {
		Scalar::Bool => 1,
		Scalar::F32 | Scalar::F64 | Scalar::String | Scalar::Bytes => {
			bail!("uint can't hold a {scalar:?}")
		}
		s => width(s).expect("integer scalar"),
	};
	ensure!(
		(1..=max as usize).contains(&size),
		"uint of {size} bits doesn't fit {scalar:?}"
	);
	Ok(UintShape {
		size: size as u32,
		scalar,
	})
}

fn prim(s: Scalar) -> &'static str {
	match s {
		Scalar::I8 => "i8",
		Scalar::U8 => "u8",
		Scalar::I16 => "i16",
		Scalar::U16 => "u16",
		Scalar::I32 => "i32",
		Scalar::U32 => "u32",
		Scalar::I64 => "i64",
		Scalar::U64 => "u64",
		Scalar::Bool => "bool",
		Scalar::F32 => "f32",
		Scalar::F64 => "f64",
		Scalar::String => "String",
		Scalar::Bytes => "Vec<u8>",
	}
}

fn uint_range(shape: &UintShape) -> (i128, i128) {
	let size = shape.size;
	if signed(shape.scalar) {
		(-(1i128 << (size - 1)), (1i128 << (size - 1)) - 1)
	} else {
		(0, (1i128 << size) - 1)
	}
}

impl AstHir for Uint {}

impl NativeLayout for Uint {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl NativeCodegen<Rust> for Uint {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let shape = uint_shape(ctx.native)?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let n = &shape.size.to_string();
		let ty = prim(shape.scalar);
		let fits = match shape.scalar {
			Scalar::Bool => true,
			s => Some(shape.size) == width(s),
		};
		let (lo, hi) = uint_range(&shape);
		let check = (!fits).then(|| {
			quote! {
				if !($(lo.to_string())..=$(hi.to_string())).contains(&(*_0 as i128)) {
					return Err($(invalid()));
				}
			}
		});
		let mask = format!("{:#x}", bits_of(-1, shape.size));
		let (write, read) = match shape.scalar {
			Scalar::Bool => (
				quote!(output.write(u64::from(*_0), 1);),
				quote!(Ok(input.read(1)? == 1)),
			),
			s if signed(s) => {
				let shift = &(64 - shape.size).to_string();
				(
					quote!(output.write(*_0 as i64 as u64 & $(&mask), $n);),
					quote!(Ok((((input.read($n)? << $shift) as i64) >> $shift) as $ty)),
				)
			}
			_ => (
				quote!(output.write(*_0 as u64, $n);),
				quote!(Ok(input.read($n)? as $ty)),
			),
		};
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$check
				$write
				Ok(())
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				$read
			}
			pub fn size_of(_0: &Ty) -> usize {
				$n
			}
		})
	}
}

impl Encoder for Uint {
	fn min_size(&self, _cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		uint_shape(native).map_or(0, |s| s.size as usize)
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let shape = uint_shape(v.native)?;
		let e = Expr::Construct(v.id, v.c.clone());
		let n = int_of(&e).ok_or_else(|| anyhow!("{} is not a number", describe(&e)))?;
		let (lo, hi) = uint_range(&shape);
		ensure!(
			(lo..=hi).contains(&n),
			"{n} doesn't fit the {} bits of {}",
			shape.size,
			cx.name(v.id)
		);
		out.write(bits_of(n, shape.size), shape.size);
		Ok(())
	}
	fn decode(&self, _cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let shape = uint_shape(t.native)?;
		let raw = read(input, shape.size)?;
		Ok(Construction::Literal(match shape.scalar {
			Scalar::Bool => Value::Bool(raw != 0),
			s if signed(s) && raw >> (shape.size - 1) == 1 => {
				Value::Signed(i128::from(raw) - (1i128 << shape.size))
			}
			_ => Value::Signed(i128::from(raw)),
		}))
	}
}

pub struct Optional;

fn optional_patterns(native: &mir::NativeType) -> Result<(Vec<u8>, Vec<u8>)> {
	let present = pattern_arg(native, "present")?;
	let absent = pattern_arg(native, "absent")?;
	prefix_free(&[
		(&Ident::new("present"), &present),
		(&Ident::new("absent"), &absent),
	])?;
	Ok((present, absent))
}

impl AstHir for Optional {}

impl NativeLayout for Optional {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Boxable(BoxCost::Low), v);
	}
}

impl ValueModel for Optional {
	fn construct(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		if matches!(value, Expr::Const(Value::UnionDescriminator(n)) if n.as_str() == "none") {
			return Ok(Construction::Variant(Ident::new("none"), vec![]));
		}
		let inner = use_of(native, VALUE)?.target;
		Ok(Construction::Wrap(Box::new(cx.coerce(value.clone(), inner)?)))
	}
}

impl NativeCodegen<Rust> for Optional {
	fn inline_ty(&self, id: TypeId, native: &mir::NativeType, names: &dyn Names<Rust>) -> Option<rust::Tokens> {
		let value = specialization_use(native, VALUE)?.target;
		Some(quote!(Option<$(names.slot_ty(id, &Ident::new(VALUE), value))>))
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let Construction::Wrap(inner) = c else {
			return Ok(quote!(None));
		};
		let value = cx.names.slot_new(id, &Ident::new(VALUE), (cx.lower)(inner)?);
		Ok(quote!(Some($value)))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let (present, absent) = optional_patterns(ctx.native)?;
		let value_use = ctx.specialization_use(VALUE).context("optional value type")?;
		let value = value_use.target;
		let slot = Ident::new(VALUE);
		let read_value = ctx.slot_new(&slot, quote!($(ctx.deserialize_call(value_use))?));
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let (p, a) = (&literal(&present), &literal(&absent));
		let write = &runtime("write_pattern");
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = Option<$(ctx.slot_ty(&slot, value))>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Some(v) => {
						$write(output, $p);
						$(ctx.names.serialize(value))(v, output)
					}
					None => {
						$write(output, $a);
						Ok(())
					}
				}
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				Ok(match $(runtime("choose"))(input, &[$p, $a])? {
					0 => Some($read_value),
					_ => None,
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					Some(v) => $(present.len().to_string()) + $(ctx.names.size_of(value))(v),
					None => $(absent.len().to_string()),
				}
			}
		})
	}
}

impl Encoder for Optional {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		let Ok((present, absent)) = optional_patterns(native) else {
			return 0;
		};
		let value = specialization_use(native, VALUE).map_or(0, |v| cx.min_size(v.target));
		absent.len().min(present.len() + value)
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let (present, absent) = optional_patterns(v.native)?;
		match v.c {
			Construction::Wrap(inner) => {
				csn1::write_pattern(out, &present);
				cx.encode_use(use_of(v.native, VALUE)?, inner, &v.scope(), out)
			}
			_ => {
				csn1::write_pattern(out, &absent);
				Ok(())
			}
		}
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let (present, absent) = optional_patterns(t.native)?;
		Ok(match choose(input, &[&present, &absent])? {
			0 => Construction::Wrap(Box::new(cx.decode_use(use_of(t.native, VALUE)?, &t.scope(), input)?)),
			_ => Construction::Variant(Ident::new("none"), vec![]),
		})
	}
}

pub struct Repeated;

fn repeated_patterns(native: &mir::NativeType) -> Result<(Vec<u8>, Vec<u8>)> {
	let more = pattern_arg(native, "more")?;
	let end = pattern_arg(native, "end")?;
	prefix_free(&[(&Ident::new("more"), &more), (&Ident::new("end"), &end)])?;
	Ok((more, end))
}

impl AstHir for Repeated {}

impl NativeLayout for Repeated {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}

impl ValueModel for Repeated {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		list_value(cx, id, native, value)
	}
}

impl NativeCodegen<Rust> for Repeated {
	fn inline_ty(&self, _id: TypeId, native: &mir::NativeType, names: &dyn Names<Rust>) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		construct_vec(cx, c)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let (more, end) = repeated_patterns(ctx.native)?;
		let value_use = ctx.specialization_use(VALUE).context("repeated value type")?;
		let value = value_use.target;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let (m, e) = (&literal(&more), &literal(&end));
		let write = &runtime("write_pattern");
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $(vec_of_value(ctx.native, ctx.names).context("repeated type")?);
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				for v in _0.iter() {
					$write(output, $m);
					$(ctx.names.serialize(value))(v, output)?;
				}
				$write(output, $e);
				Ok(())
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Vec::new();
				while $(runtime("choose"))(input, &[$m, $e])? == 0 {
					out.push($(ctx.deserialize_call(value_use))?);
				}
				Ok(out)
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = $(end.len().to_string());
				for v in _0.iter() {
					n += $(more.len().to_string()) + $(ctx.names.size_of(value))(v);
				}
				n
			}
		})
	}
}

impl Encoder for Repeated {
	fn min_size(&self, _cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		repeated_patterns(native).map_or(0, |(_, end)| end.len())
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let (more, end) = repeated_patterns(v.native)?;
		let value = use_of(v.native, VALUE)?;
		for (i, item) in list_of(v.c)?.iter().enumerate() {
			csn1::write_pattern(out, &more);
			cx.encode_use(value, item, &v.scope(), out)
				.with_context(|| format!("item {i}"))?;
		}
		csn1::write_pattern(out, &end);
		Ok(())
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let (more, end) = repeated_patterns(t.native)?;
		let value = use_of(t.native, VALUE)?;
		let mut items = Vec::new();
		while choose(input, &[&more, &end])? == 0 {
			let item = cx
				.decode_use(value, &t.scope(), input)
				.with_context(|| format!("item {}", items.len()))?;
			items.push(item);
		}
		Ok(Construction::List(items))
	}
}

pub struct Fixed;

impl AstHir for Fixed {}

impl NativeLayout for Fixed {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl ValueModel for Fixed {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		unit_value(cx, id, value)
	}
}

impl NativeCodegen<Rust> for Fixed {
	fn construct(
		&self,
		_cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		_c: &Construction,
	) -> Result<rust::Tokens> {
		Ok(quote!(()))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let bits = pattern_arg(ctx.native, "bits")?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let p = &literal(&bits);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = ();
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(runtime("write_pattern"))(output, $p);
				Ok(())
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				if $(runtime("accept"))(input, $p)? {
					Ok(())
				} else {
					Err($(invalid()))
				}
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(bits.len().to_string())
			}
		})
	}
}

impl Encoder for Fixed {
	fn min_size(&self, _cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		pattern_arg(native, "bits").map_or(0, |b| b.len())
	}
	fn encode(&self, _cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		csn1::write_pattern(out, &pattern_arg(v.native, "bits")?);
		Ok(())
	}
	fn decode(&self, _cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		choose(input, &[&pattern_arg(t.native, "bits")?])?;
		Ok(Construction::Record(vec![]))
	}
}

pub struct Spare;

impl AstHir for Spare {}

impl NativeLayout for Spare {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl ValueModel for Spare {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		unit_value(cx, id, value)
	}
}

impl NativeCodegen<Rust> for Spare {
	fn construct(
		&self,
		_cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		_c: &Construction,
	) -> Result<rust::Tokens> {
		Ok(quote!(()))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let size = &size_arg(ctx.native, "size")?.to_string();
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = ();
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(runtime("write_zeros"))(output, $size);
				Ok(())
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				input.skip($size)
			}
			pub fn size_of(_0: &Ty) -> usize {
				$size
			}
		})
	}
}

impl Encoder for Spare {
	fn min_size(&self, _cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		size_arg(native, "size").unwrap_or(0)
	}
	fn encode(&self, _cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		csn1::write_zeros(out, size_arg(v.native, "size")?);
		Ok(())
	}
	fn decode(&self, _cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		skip(input, size_arg(t.native, "size")?)?;
		Ok(Construction::Record(vec![]))
	}
}

pub struct Padded;

fn padded_octets(native: &mir::NativeType) -> Result<Option<usize>> {
	match native.const_args.iter().any(|(n, _)| n.as_str() == "octets") {
		true => size_arg(native, "octets").map(Some),
		false => Ok(None),
	}
}

impl AstHir for Padded {}

impl NativeLayout for Padded {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Fixed, v);
	}
}

impl ValueModel for Padded {
	fn construct(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let inner = use_of(native, VALUE)?.target;
		Ok(Construction::Wrap(Box::new(cx.coerce(value.clone(), inner)?)))
	}
}

impl NativeCodegen<Rust> for Padded {
	fn inline_ty(&self, _id: TypeId, native: &mir::NativeType, names: &dyn Names<Rust>) -> Option<rust::Tokens> {
		Some(names.ty(specialization_use(native, VALUE)?.target))
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let Construction::Wrap(inner) = c else {
			bail!("padded is constructed from its value, got {c:?}");
		};
		(cx.lower)(inner)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let octets = padded_octets(ctx.native)?;
		let value_use = ctx.specialization_use(VALUE).context("padded value type")?;
		let value = value_use.target;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let read_value = ctx.deserialize_call(value_use);
		let (serialize, deserialize, size_of) = match octets {
			None => (
				quote! {
					$(ctx.names.serialize(value))(_0, output)?;
					$(runtime("write_padding"))(output);
					Ok(())
				},
				quote! {
					let v = $read_value?;
					$(runtime("skip_padding"))(input);
					Ok(v)
				},
				quote!($(ctx.names.size_of(value))(_0).next_multiple_of(8)),
			),
			Some(octets) => {
				let bits = &(octets * 8).to_string();
				(
					quote! {
						let end = output.len() + $bits;
						$(ctx.names.serialize(value))(_0, output)?;
						if output.len() > end {
							return Err($(invalid()));
						}
						$(runtime("write_padding_to"))(output, end);
						Ok(())
					},
					quote! {
						let input = &mut input.take($bits)?;
						$read_value
					},
					quote!($bits),
				)
			}
		};
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $(ctx.names.ty(value));
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$serialize
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				$deserialize
			}
			pub fn size_of(_0: &Ty) -> usize {
				$size_of
			}
		})
	}
}

impl Encoder for Padded {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		match padded_octets(native) {
			Ok(Some(octets)) => octets * 8,
			_ => specialization_use(native, VALUE).map_or(0, |v| cx.min_size(v.target)),
		}
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let Construction::Wrap(inner) = v.c else {
			bail!("{} is constructed from its value", cx.name(v.id));
		};
		let value = use_of(v.native, VALUE)?;
		match padded_octets(v.native)? {
			None => {
				cx.encode_use(value, inner, &v.scope(), out)?;
				csn1::write_padding(out);
			}
			Some(octets) => {
				let start = out.len();
				cx.encode_use(value, inner, &v.scope(), out)?;
				ensure!(
					out.len() - start <= octets * 8,
					"{} takes {} bits, more than its {octets} octets",
					cx.name(v.id),
					out.len() - start
				);
				csn1::write_padding_to(out, start + octets * 8);
			}
		}
		Ok(())
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let value = use_of(t.native, VALUE)?;
		let inner = match padded_octets(t.native)? {
			None => {
				let inner = cx.decode_use(value, &t.scope(), input)?;
				input.rest();
				inner
			}
			Some(octets) => {
				let at = input.position();
				let sub = input
					.take(octets * 8)
					.map_err(|_| anyhow!("bit {at}: {octets} octets needed, {} bits left", input.remaining()))?;
				cx.decode_use(value, &t.scope(), &sub)?
			}
		};
		Ok(Construction::Wrap(Box::new(inner)))
	}
}
