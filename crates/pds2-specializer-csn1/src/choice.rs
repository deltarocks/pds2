use anyhow::{Context as _, Result, anyhow, bail, ensure};
use genco::lang::{Rust, rust};
use genco::prelude::*;
use pds_bits_runtime::csn1;
use pds2_core::ast::Ident;
use pds2_core::generator::common::sanitize;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, Value};
use pds2_core::specializer::eval::{Target, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel, describe};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx, NativeLayout,
	Projection, SlotVisitor, pds_io,
};

use crate::natives::{invalid, no_view, runtime};
use crate::pattern::{literal, pattern, prefix_free};
use crate::wire::{Encoder, Encoding, Input, Output, choose};

pub struct Choice;

pub struct Arm<'a> {
	pub name: &'a Ident,
	pub bits: Vec<u8>,
	pub ty: &'a mir::Type,
}

pub fn arms(native: &mir::NativeType) -> Result<Vec<Arm<'_>>> {
	let arms = native
		.body
		.iter()
		.zip(native.fields.iter())
		.map(|(item, (name, ty))| {
			let bits = match item.constant("bits") {
				Some(Value::String(s)) => pattern(s)?,
				other => bail!("choice arm {name:?} needs a `bits` string, got {other:?}"),
			};
			Ok(Arm { name, bits, ty })
		})
		.collect::<Result<Vec<_>>>()?;
	ensure!(!arms.is_empty(), "a choice needs at least one variant");
	prefix_free(
		&arms
			.iter()
			.map(|a| (a.name, a.bits.as_slice()))
			.collect::<Vec<_>>(),
	)?;
	Ok(arms)
}

impl AstHir for Choice {
	fn project(&self, ctx: &HirSpecializerCtx) -> Result<Projection> {
		let mut out = Projection::default();
		for f in ctx.body {
			let name = f.expect_name()?;
			ensure!(
				matches!(f.constant("bits"), Some(hir::Value::String(_))),
				"choice arm {name:?} needs a `bits` string"
			);
			let ty =
				f.ty.clone()
					.ok_or_else(|| anyhow!("choice arm {name:?} has no type"))?;
			out.fields.push((name.clone(), ty));
		}
		Ok(out)
	}
}

impl NativeLayout for Choice {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		for (name, ty) in &native.fields {
			v.slot(name, ty.target, Embed::Boxable(BoxCost::Low));
		}
	}
	fn members(&self, native: &mir::NativeType) -> Members {
		Members {
			variants: native.fields.iter().map(|(n, _)| n.clone()).collect(),
			..Default::default()
		}
	}
}

impl ValueModel for Choice {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let arm = |name: &Ident| {
			native
				.fields
				.iter()
				.find(|(n, _)| sanitize(n.as_str()) == sanitize(name.as_str()))
				.ok_or_else(|| {
					let known: Vec<&str> = native.fields.iter().map(|(n, _)| n.as_str()).collect();
					anyhow!(
						"{} has no variant {name:?}, expected one of {known:?}",
						cx.name(id)
					)
				})
		};
		match value {
			Expr::Const(Value::UnionDescriminator(name)) => {
				let (arm, ty) = arm(name)?;
				ensure!(
					cx.types.is_void(ty.target),
					"variant {name:?} carries a value, write it as `({name:?}: ...)`"
				);
				Ok(Construction::Variant(arm.clone(), vec![]))
			}
			Expr::Record(fields) => {
				let [(name, payload)] = fields.as_slice() else {
					bail!(
						"a {} value names one variant, `(variant: value)`, got {} fields",
						cx.name(id),
						fields.len()
					);
				};
				let (arm, ty) = arm(name)?;
				ensure!(
					!cx.types.is_void(ty.target),
					"variant {name:?} carries no value, write it as `{name:?}`"
				);
				let payload = cx
					.coerce(payload.clone(), ty.target)
					.with_context(|| format!("variant {name:?}"))?;
				Ok(Construction::Variant(arm.clone(), vec![payload]))
			}
			v => bail!("{} can't be written as {}", cx.name(id), describe(v)),
		}
	}
}

impl NativeCodegen<Rust> for Choice {
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let Construction::Variant(name, payload) = c else {
			bail!("choice is constructed from a variant, got {c:?}");
		};
		let variant = cx.names.variant(id, name.as_str());
		Ok(match payload.as_slice() {
			[] => quote!($(cx.names.ty(id))::$variant),
			[value] => {
				let value = cx.names.slot_new(id, name, (cx.lower)(value)?);
				quote!($(cx.names.ty(id))::$variant($value))
			}
			_ => bail!("variant {name:?} holds one value"),
		})
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		no_view(ctx)?;
		let arms = arms(ctx.native)?;
		let names = ctx.names;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let write = &runtime("write_pattern");
		let mut decl = Vec::new();
		let mut ser = Vec::new();
		let mut de = Vec::new();
		let mut size = Vec::new();
		for (i, arm) in arms.iter().enumerate() {
			let v = &ctx.variant(arm.name.as_str());
			let p = &literal(&arm.bits);
			let len = &arm.bits.len().to_string();
			let index = i.to_string();
			if ctx.types.is_void(arm.ty.target) {
				decl.push(quote!($v,));
				ser.push(quote!(Ty::$v => $write(output, $p),));
				de.push(quote!($index => Ty::$v,));
				size.push(quote!(Ty::$v => $len,));
			} else {
				let target = arm.ty.target;
				let read = ctx.slot_new(arm.name, quote!($(ctx.deserialize_call(arm.ty))?));
				decl.push(quote!($v($(ctx.slot_ty(arm.name, target))),));
				ser.push(quote! {
					Ty::$v(v) => {
						$write(output, $p);
						$(names.serialize(target))(v, output)?;
					}
				});
				de.push(quote!($index => Ty::$v($read),));
				size.push(quote!(Ty::$v(v) => $len + $(names.size_of(target))(v),));
			}
		}
		let patterns: Vec<rust::Tokens> = arms.iter().map(|a| literal(&a.bits)).collect();
		Ok(quote! {
			$((ctx.args_struct)())
			$(ctx.derive())
			pub enum $(ctx.nominal()) {
				$(for d in decl join($['\r']) => $d)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					$(for s in ser join($['\r']) => $s)
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<Ty> {
				Ok(match $(runtime("choose"))(input, &[$(for p in patterns join(, ) => $p)])? {
					$(for d in de join($['\r']) => $d)
					_ => return Err($(invalid())),
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					$(for s in size join($['\r']) => $s)
				}
			}
		})
	}
}

impl Encoder for Choice {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		arms(native).map_or(0, |arms| {
			arms.iter()
				.map(|a| a.bits.len() + cx.min_size(a.ty.target))
				.min()
				.unwrap_or(0)
		})
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let Construction::Variant(name, payload) = v.c else {
			bail!("{} is constructed from a variant", cx.name(v.id));
		};
		let arms = arms(v.native)?;
		let arm = arms
			.iter()
			.find(|a| a.name == name)
			.ok_or_else(|| anyhow!("{} has no variant {name:?}", cx.name(v.id)))?;
		csn1::write_pattern(out, &arm.bits);
		match payload.as_slice() {
			[] => Ok(()),
			[value] => cx
				.encode_use(arm.ty, value, &v.scope(), out)
				.with_context(|| format!("variant {name:?}")),
			_ => bail!("variant {name:?} holds one value"),
		}
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let arms = arms(t.native)?;
		let patterns: Vec<&[u8]> = arms.iter().map(|a| a.bits.as_slice()).collect();
		let arm = &arms[choose(input, &patterns)?];
		if cx.types().is_void(arm.ty.target) {
			return Ok(Construction::Variant(arm.name.clone(), vec![]));
		}
		let value = cx
			.decode_use(arm.ty, &t.scope(), input)
			.with_context(|| format!("variant {:?}", arm.name))?;
		Ok(Construction::Variant(arm.name.clone(), vec![value]))
	}
}
