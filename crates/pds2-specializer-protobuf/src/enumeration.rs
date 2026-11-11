use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::decode::Input;
use pds2_core::specializer::encode::{int_of, Encoder, Encoding, Target, Val};
use pds2_core::specializer::expr::{describe, Elaborator, ValueModel};
use pds2_core::specializer::{
	specialization_use, AstHir, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx,
	NativeLayout, SlotVisitor,
};
use pds2_core::ast::Ident;
use pds2_core::generator::common::scalar_of;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, Value};

pub struct Enum;

const TAG: &str = "Tag";

pub struct EnumView<'a> {
	pub tag: &'a mir::Type,
	pub values: Vec<(&'a Ident, i128)>,
	pub unknown: Ident,
}

pub fn view(native: &mir::NativeType) -> Option<EnumView<'_>> {
	if !matches!(&native.specializer, Some((s, _)) if s.as_str() == "enum") {
		return None;
	}
	let tag = specialization_use(native, TAG)?;
	let values = native
		.body
		.iter()
		.map(|item| {
			let n = match item.constant("number")? {
				Value::Signed(n) => *n,
				Value::Unsigned(n) => i128::try_from(*n).ok()?,
				_ => return None,
			};
			Some((item.name.as_ref()?, n))
		})
		.collect::<Option<Vec<_>>>()?;
	let mut unknown = "Unknown".to_owned();
	while values.iter().any(|(n, _)| n.as_str() == unknown) {
		unknown.push('_');
	}
	Some(EnumView {
		tag,
		values,
		unknown: Ident::new(&unknown),
	})
}

fn view_of(native: &mir::NativeType) -> Result<EnumView<'_>> {
	view(native).ok_or_else(|| anyhow!("malformed enum"))
}

impl AstHir for Enum {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ensure!(
			ctx.types.contains_key(&Ident::new(TAG)),
			"enum needs a `Tag:` type binding"
		);
		for f in ctx.body {
			let name = f.expect_name()?;
			ensure!(
				matches!(f.constant("number"), Some(hir::Value::Number(_))),
				"enum value {name:?} needs an integer `number`"
			);
			ensure!(f.ty.is_none(), "enum value {name:?} carries no type");
		}
		Ok(vec![])
	}
}

impl NativeLayout for Enum {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		pds2_core::specializer::visit_specialization(native, TAG, Embed::Fixed, v);
	}
	fn members(&self, native: &mir::NativeType) -> Members {
		let Some(e) = view(native) else {
			return Members::default();
		};
		Members {
			variants: e
				.values
				.iter()
				.map(|(v, _)| (*v).clone())
				.chain([e.unknown.clone()])
				.collect(),
			..Default::default()
		}
	}
}

fn number_of(cx: &Elaborator, e: &EnumView, c: &Construction) -> Result<Expr> {
	match c {
		Construction::Variant(n, payload) if *n == e.unknown => payload
			.first()
			.cloned()
			.ok_or_else(|| anyhow!("unknown enum value without its number")),
		Construction::Variant(n, _) => {
			let (_, number) = e
				.values
				.iter()
				.find(|(v, _)| *v == n)
				.ok_or_else(|| anyhow!("no enum value {n:?}"))?;
			cx.build(e.tag.target, &Expr::Const(Value::Signed(*number)))
		}
		c => bail!("enum is constructed from a value name, got {c:?}"),
	}
}

impl ValueModel for Enum {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let e = view_of(native)?;
		match value {
			Expr::Const(Value::UnionDescriminator(name)) => {
				let (v, _) = e.values.iter().find(|(v, _)| *v == name).ok_or_else(|| {
					let known: Vec<&str> = e.values.iter().map(|(v, _)| v.as_str()).collect();
					anyhow!("{} has no value {name:?}, expected one of {known:?}", cx.name(id))
				})?;
				Ok(Construction::Variant((*v).clone(), vec![]))
			}
			Expr::Const(_) => {
				let n = int_of(value)
					.ok_or_else(|| anyhow!("{} can't be written as {}", cx.name(id), describe(value)))?;
				if let Some((v, _)) = e.values.iter().find(|(_, m)| *m == n) {
					return Ok(Construction::Variant((*v).clone(), vec![]));
				}
				let tag = cx.build(e.tag.target, value)?;
				Ok(Construction::Variant(e.unknown.clone(), vec![tag]))
			}
			v => bail!("{} is written as a value name or number, got {}", cx.name(id), describe(v)),
		}
	}
	fn casts(&self, _cx: &Elaborator, _id: TypeId, native: &mir::NativeType) -> Vec<TypeId> {
		view(native).map(|e| vec![e.tag.target]).unwrap_or_default()
	}
	fn literal(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<Expr> {
		let e = view_of(native)?;
		match c {
			Construction::Variant(n, payload) if *n == e.unknown => match payload.as_slice() {
				[number] => cx.literal(number),
				_ => bail!("unknown enum value without its number"),
			},
			c => cx.construction_literal(c),
		}
	}
}

impl Encoder for Enum {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let e = view_of(v.native)?;
		let n = number_of(cx.elab, &e, v.c)?;
		cx.encode_use(e.tag, &n, &v.scope(), out).context("enum number")
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let e = view_of(t.native)?;
		let tag = cx.decode_use(e.tag, &t.scope(), input).context("enum number")?;
		let n = int_of(&tag).ok_or_else(|| anyhow!("enum number {} is not a number", describe(&tag)))?;
		Ok(match e.values.iter().find(|(_, m)| *m == n) {
			Some((v, _)) => Construction::Variant((*v).clone(), vec![]),
			None => Construction::Variant(e.unknown.clone(), vec![tag]),
		})
	}
	fn cast(&self, cx: &Encoding, v: &Val, to: TypeId) -> Result<Expr> {
		let e = view_of(v.native)?;
		ensure!(
			cx.elab.resolve(to) == cx.elab.resolve(e.tag.target),
			"{} can't be cast to {}",
			cx.name(v.id),
			cx.name(to)
		);
		number_of(cx.elab, &e, v.c)
	}
}

fn literals<L: Lang>(ctx: &NativeCodegenCtx<L>, e: &EnumView) -> Result<Vec<Tokens<L>>> {
	ensure!(
		scalar_of(ctx.types, e.tag.target).is_some_and(|s| s.is_number()),
		"enum `Tag` should be an integer scalar"
	);
	e.values
		.iter()
		.map(|(_, n)| {
			let lit = ctx.elab.build(e.tag.target, &Expr::Const(Value::Signed(*n)))?;
			(ctx.lower)(&lit, false)
		})
		.collect()
}

impl NativeCodegen<Rust> for Enum {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let e = view_of(ctx.native)?;
		let lits = literals(ctx, &e)?;
		let tag = e.tag.target;
		let names = ctx.names;
		let unknown = ctx.variant(e.unknown.as_str());
		let v = |v: &Ident| ctx.variant(v.as_str());
		let (pinput, poutput, presult) = pds2_core::specializer::pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let tag_ty = names.ty(tag);
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum $(ctx.nominal()) {
				$(for (n, _) in &e.values join($['\r']) => $(v(n)),)
				$(&unknown)($(&tag_ty)),
			}
			pub fn number(_0: &Ty) -> $(&tag_ty) {
				match _0 {
					$(for ((n, _), lit) in e.values.iter().zip(&lits) join($['\r']) => Ty::$(v(n)) => $lit,)
					Ty::$(&unknown)(n) => n.clone(),
				}
			}
			pub fn from_number(n: $(&tag_ty)) -> Ty {
				match n {
					$(for ((n, _), lit) in e.values.iter().zip(&lits) join($['\r']) => $lit => Ty::$(v(n)),)
					n => Ty::$(&unknown)(n),
				}
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(names.serialize(tag))(&number(_0), output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				Ok(from_number($(ctx.deserialize_call(e.tag))?))
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(names.size_of(tag))(&number(_0))
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
		let e = view_of(native)?;
		let ty = cx.names.ty(id);
		Ok(match c {
			Construction::Variant(n, payload) if *n == e.unknown => {
				let [n] = payload.as_slice() else {
					bail!("unknown enum value without its number");
				};
				quote!($ty::$(cx.names.variant(id, e.unknown.as_str()))($((cx.lower)(n)?)))
			}
			Construction::Variant(n, _) => quote!($ty::$(cx.names.variant(id, n.as_str()))),
			c => bail!("enum is constructed from a value name, got {c:?}"),
		})
	}
	fn cast(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		value: rust::Tokens,
		_to: TypeId,
	) -> Result<rust::Tokens> {
		Ok(quote!($(cx.names.item(id, "number"))(&$value)))
	}
}

impl NativeCodegen<JavaScript> for Enum {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let e = view_of(ctx.native)?;
		let lits = literals(ctx, &e)?;
		let tag = e.tag.target;
		let names = ctx.names;
		let tag_ty = names.ty(tag);
		let pinput = &ctx.runtime("Input");
		let poutput = &ctx.runtime("Output");
		let pass = (ctx.pass_args)(e.tag);
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty =
				$(for (v, _) in &e.values => | $(quoted(v.as_str())))
				| $(&tag_ty);
			export function number(_0: Ty): $(&tag_ty) {
				switch (_0) {
					$(for ((v, _), lit) in e.values.iter().zip(&lits) join($['\r']) => case $(quoted(v.as_str())): return $lit;)
					default: return _0;
				}
			}
			export function from_number(n: $(&tag_ty)): Ty {
				switch (n) {
					$(for ((v, _), lit) in e.values.iter().zip(&lits) join($['\r']) => case $lit: return $(quoted(v.as_str()));)
					default: return n;
				}
			}
			export function serialize(_0: Ty, output: $poutput): void {
				$(names.serialize(tag))(number(_0), output);
			}
			export function deserialize(input: $pinput): Ty {
				return from_number($(names.deserialize(tag))(input$(if !pass.is_empty() => , $pass)));
			}
			export function size_of(_0: Ty): number {
				return $(names.size_of(tag))(number(_0));
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
		let e = view_of(native)?;
		Ok(match c {
			Construction::Variant(n, payload) if *n == e.unknown => {
				let [n] = payload.as_slice() else {
					bail!("unknown enum value without its number");
				};
				(cx.lower)(n)?
			}
			Construction::Variant(n, _) => quote!($(quoted(n.as_str()))),
			c => bail!("enum is constructed from a value name, got {c:?}"),
		})
	}
	fn cast(
		&self,
		cx: &LowerCx<JavaScript>,
		id: TypeId,
		_native: &mir::NativeType,
		value: js::Tokens,
		_to: TypeId,
	) -> Result<js::Tokens> {
		Ok(quote!($(cx.names.item(id, "number"))($value)))
	}
}
