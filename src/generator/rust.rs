use genco::lang::rust::import;
use genco::prelude::*;
use genco::quote;
use rust::{Import, Tokens};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ast::{BinOp, FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, Type, TypeKind, TypeMap, Value};

use super::boxing::{self, Boxed};
use super::common::{self, scalar_of};
use super::GeneratorOutput;
use crate::specializer::expr::Elaborator;
use crate::specializer::{LowerCx, Names, NativeCodegen, NativeCodegenCtx, Registry, Scalar};
use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::Rust;

#[derive(Clone)]
struct RustMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	types: &'a TypeMap,
	reg: &'a Registry,
	boxed: Rc<Boxed>,
	inlining: Rc<RefCell<BTreeSet<TypeId>>>,
}
impl<'a> RustMeta<'a> {
	fn new(source: &'a TypeMap, reg: &'a Registry, boxed: Boxed) -> Self {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		Self {
			raw_names: Rc::new(raw_names),
			types: source,
			reg,
			boxed: Rc::new(boxed),
			inlining: Rc::default(),
		}
	}
	fn inline_ty(&self, type_id: TypeId) -> Option<Tokens> {
		let TypeKind::Native(native) = &self.types[type_id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		let codegen = self.reg.codegen::<Rust>(specializer.as_str())?;
		if !self.inlining.borrow_mut().insert(type_id) {
			return None;
		}
		let ty = codegen.inline_ty(type_id, native, self);
		self.inlining.borrow_mut().remove(&type_id);
		ty
	}
	fn name_str(&self, type_id: TypeId) -> String {
		assert!(type_id.is_valid());
		format!("pds_{}", self.raw_names[&type_id])
	}
	fn name(&self, type_id: TypeId) -> Tokens {
		assert!(type_id.is_valid());
		quote!(pds_$(&self.raw_names[&type_id]))
	}
	fn imported(&self, type_id: TypeId, ty: impl AsRef<str>) -> Import {
		import(format!("super::{}", self.name_str(type_id)), ty.as_ref()).qualified()
	}
	fn ty(&self, type_id: TypeId) -> Tokens {
		assert!(type_id.is_valid());
		match self.inline_ty(type_id) {
			Some(ty) => ty,
			None => quote!($(self.imported(type_id, "Ty"))),
		}
	}
	fn virt(&self, type_id: TypeId, field: Ident) -> Import {
		self.imported(type_id, format!("virtual_{}", field.as_str()))
	}
	fn args(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "Args")
	}
	fn serialize(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "serialize")
	}
	fn deserialize(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "deserialize")
	}
	fn size_of(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "size_of")
	}
}

impl Names<Rust> for RustMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		RustMeta::ty(self, id)
	}
	fn serialize(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::serialize(self, id)))
	}
	fn deserialize(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::deserialize(self, id)))
	}
	fn size_of(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::size_of(self, id)))
	}
	fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		self.boxed.contains(&(owner, slot.clone()))
	}
	fn slot_ty(&self, owner: TypeId, slot: &Ident, target: TypeId) -> Tokens {
		let ty = RustMeta::ty(self, target);
		if Names::boxed(self, owner, slot) {
			quote!(Box<$ty>)
		} else {
			ty
		}
	}
	fn slot_new(&self, owner: TypeId, slot: &Ident, value: Tokens) -> Tokens {
		if Names::boxed(self, owner, slot) {
			quote!(Box::new($value))
		} else {
			value
		}
	}
	fn scalar(&self, id: TypeId) -> Option<Scalar> {
		scalar_of(self.types, id)
	}
	fn item(&self, id: TypeId, name: &str) -> Tokens {
		quote!($(self.imported(id, name)))
	}
}

/// Emit an identifier, raw-prefixed when it collides with a rust keyword
/// (protodef loves fields named `type`).
pub(crate) fn rname(i: &Ident) -> Tokens {
	const KEYWORDS: &[&str] = &[
		"as", "async", "await", "box", "break", "const", "continue", "do", "dyn", "else", "enum",
		"extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
		"mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe",
		"use", "where", "while", "yield", "abstract", "become", "final", "macro", "override",
		"priv", "try", "typeof", "unsized", "virtual", "gen",
	];
	const UNRAWABLE: &[&str] = &["self", "Self", "super", "crate"];
	if KEYWORDS.contains(&i.as_str()) {
		quote!($(format!("r#{}", i.as_str())))
	} else if UNRAWABLE.contains(&i.as_str()) {
		quote!($(format!("{}_", i.as_str())))
	} else {
		quote!($i)
	}
}

fn generate_read_or_const_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &Expr,
	target: Option<&Tokens>,
) -> Tokens {
	let input = match input {
		Expr::Field(field) => field,
		Expr::Const(mir::Value::Signed(n)) => {
			return quote!(&($(n.to_string())).try_into().expect("constant fits"))
		}
		Expr::Const(mir::Value::Unsigned(n)) => {
			return quote!(&($(n.to_string())).try_into().expect("constant fits"))
		}
		Expr::Const(mir::Value::Bool(b)) => return quote!(&$(b.to_string())),
		Expr::Const(mir::Value::String(s)) => {
			return quote!(&$(quoted(s.as_str())).to_owned())
		}
		Expr::Const(mir::Value::UnionDescriminator(id)) => {
			unreachable!("union discriminator constant {id:?} as an argument")
		}
		e => {
			let value = lower(types, e, from_locals).expect("validated before codegen");
			return quote!(&($value));
		}
	};
	generate_read_reference(types, from_locals, input, target)
}
fn generate_read_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &Field,
	target: Option<&Tokens>,
) -> Tokens {
	let mut indexing = vec![];
	let computed = input
		.0
		.iter()
		.any(|f| matches!(f.segment, FieldRefSegment::Virtual(_)));
	for (i, field) in input.0.iter().enumerate() {
		indexing.push(match &field.segment {
			FieldRefSegment::Arg(name) => {
				assert_eq!(i, 0);
				quote! {
					let _0 = args.$(rname(name));
				}
			}
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					// deserialize locals are f_-prefixed to avoid shadowing
					// `input`/`args`
					quote!(let _0: &$(types.ty(field.to.get())) = &$(common::local(to));)
				} else {
					quote!(let _0: &$(types.ty(field.to.get())) = &_0.$(rname(to));)
				}
			}
			FieldRefSegment::Virtual(name) => {
				let local = format!("__v{i}");
				quote! {
					let $(&local): $(types.ty(field.to.get())) = $(types.virt(field.from.get(), name.clone()))(_0);
					let _0 = &$(&local);
				}
			}
		})
	}
	if let Some(target) = target {
		indexing.push(quote!(Into::<$target>::into(*_0)));
		quote!({$(for i in indexing => $i)})
	} else if computed {
		indexing.push(quote!(_0.clone()));
		quote!(&{$(for i in indexing => $i)})
	} else {
		indexing.push(quote!(_0));
		quote!({$(for i in indexing => $i)})
	}
}

fn generate_args_passing(types: &RustMeta, input: &Type) -> Tokens {
	let mut args = vec![];
	for (name, field) in input.args.iter() {
		args.push(
			quote!($(rname(name)): $(generate_read_or_const_reference(types, true, field, None))),
		)
	}
	quote! {
		$(types.args(input.target)) {
			$(for arg in args join(,) => $arg)
		}
	}
}

fn literal(s: Scalar, value: &Value) -> Result<Tokens> {
	let suffix = match s {
		Scalar::I8 => "i8",
		Scalar::U8 => "u8",
		Scalar::I16 => "i16",
		Scalar::U16 => "u16",
		Scalar::I32 => "i32",
		Scalar::U32 => "u32",
		Scalar::I64 => "i64",
		Scalar::U64 => "u64",
		Scalar::F32 => "f32",
		Scalar::F64 => "f64",
		Scalar::Bool => "",
	};
	Ok(match (s, value) {
		(Scalar::Bool, Value::Bool(b)) => quote!($(b.to_string())),
		(Scalar::Bool, v) => bail!("{v:?} is not a bool literal"),
		(_, Value::Signed(n)) => quote!($(format!("{n}_{suffix}"))),
		(_, Value::Unsigned(n)) => quote!($(format!("{n}_{suffix}"))),
		(s, v) => bail!("{v:?} is not a {s:?} literal"),
	})
}

fn native_codegen<'m>(
	meta: &RustMeta<'m>,
	id: TypeId,
) -> Result<(&'m mir::NativeType, &'m dyn NativeCodegen<Rust>)> {
	let TypeKind::Native(n) = &meta.types[id].kind else {
		bail!("{} is not a native", meta.name_str(id));
	};
	let (spec, _) = n
		.specializer
		.as_ref()
		.ok_or_else(|| anyhow!("{} has no specializer", meta.name_str(id)))?;
	let codegen = meta
		.reg
		.codegen::<Rust>(spec.as_str())
		.ok_or_else(|| anyhow!("no rust codegen for specializer {spec:?}"))?;
	Ok((n, codegen))
}

fn lower(meta: &RustMeta, expr: &Expr, from_locals: bool) -> Result<Tokens> {
	let rec = |e: &Expr| lower(meta, e, from_locals);
	let cx = LowerCx {
		names: meta,
		types: meta.types,
		lower: &rec,
	};
	Ok(match expr {
		Expr::Var(name, _) => quote!($(name.as_str()).clone()),
		Expr::Field(f) => {
			quote!(($(generate_read_reference(meta, from_locals, f, None))).clone())
		}
		Expr::Const(v) => bail!("untyped constant {v:?}"),
		Expr::Construct(id, c) => match (scalar_of(meta.types, *id), c) {
			(Some(s), Construction::Literal(v)) => literal(s, v)?,
			_ => {
				let (n, codegen) = native_codegen(meta, *id)?;
				codegen.construct(&cx, *id, n, c)?
			}
		},
		Expr::Cast(e, to) => {
			let from = e
				.ty()
				.ok_or_else(|| anyhow!("cast of an untyped expression"))?;
			let from = Elaborator {
				types: meta.types,
				reg: meta.reg,
			}
			.resolve(from);
			let numeric = |t: TypeId| scalar_of(meta.types, t).is_some_and(|s| s != Scalar::Bool);
			if numeric(from) && numeric(*to) {
				quote!((($(rec(e)?)) as $(meta.ty(*to))))
			} else {
				let (n, codegen) = native_codegen(meta, from)?;
				codegen.cast(&cx, from, n, rec(e)?, *to)?
			}
		}
		Expr::Binary(op, a, b) => {
			let ty = expr
				.ty()
				.ok_or_else(|| anyhow!("untyped arithmetic"))?;
			let (a, b) = (rec(a)?, rec(b)?);
			match scalar_of(meta.types, ty) {
				Some(Scalar::F32 | Scalar::F64) => quote!(($a $(op.symbol()) $b)),
				Some(Scalar::Bool) | None => bail!("arithmetic on a non-number"),
				Some(_) => {
					let method = match op {
						BinOp::Add => "wrapping_add",
						BinOp::Sub => "wrapping_sub",
						BinOp::Mul => "wrapping_mul",
					};
					quote!(($a).$method($b))
				}
			}
		}
		Expr::Eq(a, b) => quote!(($(rec(a)?) == $(rec(b)?))),
	})
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let boxed = boxing::analyze(input, reg)?;
	let meta = RustMeta::new(input, reg, boxed);
	let virtuals_used = common::virtuals_used(input);
	let elab = Elaborator { types: input, reg };
	let elaborated = common::elaborate(&elab)?;
	for def in input.0.values() {
		for e in def.exprs() {
			if !matches!(e, Expr::Field(_) | Expr::Const(_)) {
				lower(&meta, e, true).with_context(|| format!("in {:?}", def.name))?;
			}
		}
	}

	let mut out_module = vec![];

	// pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports, unused_parens, clippy::needless_borrow, clippy::just_underscores_and_digits, clippy::identity_op)]" NL);
	// pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
	for (id, ty) in &input.0 {
		let pinput = &import("crate::pds", "Input");
		let poutput = &import("crate::pds", "Output");
		let presult = &import("crate::pds", "Result");
		let _name = ty.name.readable_name();
		let has_args = !ty.params.is_empty();
		// Params with no inferred type (only ever bound to constants) fall back to a
		// plain integer.
		let param_ty = |par: TypeId| -> Tokens {
			if par.is_valid() {
				quote!($(meta.ty(par)))
			} else {
				quote!(usize)
			}
		};
		let args_struct = quote! {
			$(if has_args =>
				pub struct Args<'a> {
					$(for (name, par) in ty.params.iter() join($['\r']) => pub $(rname(name)): &'a $(param_ty(*par)),)
				}
			)
		};
		// args to pass when referencing `target` from generated code
		let args_of = |target: TypeId, ty: &Type| -> Tokens {
			if input[target].params.is_empty() {
				quote!()
			} else {
				quote!($(generate_args_passing(&meta, ty)),)
			}
		};
		let def = if !ty.kind.is_native() {
			let mod_contents = match &ty.kind {
				TypeKind::Alias(target) => {
					quote! {
						pub type Ty = $(meta.ty(target.target));
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(meta.serialize(target.target))(_0, output)
						}
						pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
							$(meta.deserialize(target.target))(
								input,
								$(args_of(target.target, target))
							)
						}
						pub fn size_of(_0: &Ty) -> usize {
							$(meta.size_of(target.target))(_0)
						}
					}
				}
				TypeKind::Native(_) => unreachable!("handled by the native branch"),
			};
			mod_contents
		} else {
			let TypeKind::Native(native) = &ty.kind else {
				unreachable!();
			};
			match native {
				mir::NativeType {
					specializer: Some((specializer, variant)),
					..
				} => {
					let resolve = |n: &str| {
						meta.raw_names
							.iter()
							.find(|(_, v)| v.as_str() == n)
							.map(|(k, _)| *k)
					};
					let Some(codegen) = reg.codegen::<Rust>(specializer.as_str()) else {
						anyhow::bail!("no rust codegen for specializer {specializer:?}");
					};
					let args_struct_gen = || args_struct.clone();
					let args_param = || {
						if has_args {
							quote!(args: Args<'_>,)
						} else {
							quote!()
						}
					};
					let pass_args = |u: &Type| args_of(u.target, u);
					let virtual_used =
						|name: &str| virtuals_used.contains(&(*id, Ident::new(name)));
					codegen
						.generate(&NativeCodegenCtx {
							id: *id,
							variant,
							native,
							params: &ty.params,
							names: &meta,
							resolve: &resolve,
							args_struct: &args_struct_gen,
							args_param: &args_param,
							pass_args: &pass_args,
							types: input,
							elab: &elab,
							exprs: elaborated.exprs(*id),
							cast_to: elaborated.cast_to(*id),
							virtual_used: &virtual_used,
							read: &|v, from_locals| {
								generate_read_or_const_reference(&meta, from_locals, v, None)
							},
							lower: &|e, from_locals| lower(&meta, e, from_locals),
						})
						.with_context(|| format!("codegen for {:?}", ty.name))?
				}
				_ => {
					let import = import(
						"crate::pds_native",
						format!("pds_{}", ty.name.readable_name()),
					);
					quote! {
						pub use $import::*;
					}
				}
			}
		};
		output.insert(
			format!("{}.rs", meta.name_str(*id)).into(),
			def.to_file_string().expect("formatting should succeed"),
		);
		out_module.push(quote! {
			pub mod $(meta.name(*id));
		})
	}

	output.insert(
		"mod.rs".into(),
		quote! {
			$(for m in out_module join($['\r']) => $m)
		}
		.to_file_string()
		.expect("formatting should succeed"),
	);
	Ok(())
}
