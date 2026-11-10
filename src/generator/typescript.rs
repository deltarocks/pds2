use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::{js, JavaScript};
use genco::prelude::*;

use super::common::{self, option_inner, sanitize, scalar_of};
use super::GeneratorOutput;
use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, FieldOrConst, Type, TypeKind, TypeMap, Value};
use crate::specializer::expr::Elaborator;
use crate::specializer::{LowerCx, Names, NativeCodegen, NativeCodegenCtx, Registry, Scalar};

type Tokens = js::Tokens;

struct TsMeta<'a> {
	raw_names: BTreeMap<TypeId, String>,
	types: &'a TypeMap,
	reg: &'a Registry,
	inlining: RefCell<BTreeSet<TypeId>>,
}

impl<'a> TsMeta<'a> {
	fn new(types: &'a TypeMap, reg: &'a Registry) -> Self {
		Self {
			raw_names: types
				.0
				.iter()
				.map(|(id, ty)| (*id, ty.name.readable_name()))
				.collect(),
			types,
			reg,
			inlining: RefCell::default(),
		}
	}
	fn name_str(&self, id: TypeId) -> String {
		assert!(id.is_valid());
		format!("pds_{}", self.raw_names[&id])
	}
	fn module(&self, id: TypeId) -> js::Import {
		let name = self.name_str(id);
		js::import(format!("./{name}.ts"), name).into_wildcard()
	}
	fn item(&self, id: TypeId, item: impl Into<String>) -> Tokens {
		let module = self.module(id);
		let item = item.into();
		quote!($module.$item)
	}
	fn inline_ty(&self, id: TypeId) -> Option<Tokens> {
		let TypeKind::Native(native) = &self.types[id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		let codegen = self.reg.codegen::<JavaScript>(specializer.as_str())?;
		if !self.inlining.borrow_mut().insert(id) {
			return None;
		}
		let ty = codegen.inline_ty(id, native, self);
		self.inlining.borrow_mut().remove(&id);
		ty
	}
	fn nullable(&self, id: TypeId) -> bool {
		match &self.types[id].kind {
			TypeKind::Alias(t) => self.nullable(t.target),
			_ => self.types.is_void(id) || option_inner(self.types, id).is_some(),
		}
	}
	fn literal(&self, value: &Value, ty: TypeId) -> Tokens {
		literal(self.types, value, ty)
	}
}

pub(crate) fn literal(types: &TypeMap, value: &Value, ty: TypeId) -> Tokens {
	let wide = matches!(
		ty.is_valid().then(|| scalar_of(types, ty)).flatten(),
		Some(Scalar::I64 | Scalar::U64)
	);
	let int = |n: String| -> Tokens {
		if wide {
			quote!($(format!("{n}n")))
		} else {
			quote!($n)
		}
	};
	match value {
		Value::Signed(n) => int(n.to_string()),
		Value::Unsigned(n) => int(n.to_string()),
		Value::Bool(b) => quote!($(b.to_string())),
		Value::String(s) => quote!($(quoted(s.as_str()))),
		Value::UnionDescriminator(id) => {
			quote!({ kind: $(quoted(sanitize(id.as_str()))) })
		}
	}
}

impl Names<JavaScript> for TsMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		self.inline_ty(id).unwrap_or_else(|| self.item(id, "Ty"))
	}
	fn serialize(&self, id: TypeId) -> Tokens {
		self.item(id, "serialize")
	}
	fn deserialize(&self, id: TypeId) -> Tokens {
		self.item(id, "deserialize")
	}
	fn size_of(&self, id: TypeId) -> Tokens {
		self.item(id, "size_of")
	}
	fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		slot.as_str() == "Value"
			&& option_inner(self.types, owner).is_some_and(|inner| self.nullable(inner))
	}
	fn slot_ty(&self, owner: TypeId, slot: &Ident, target: TypeId) -> Tokens {
		let ty = Names::ty(self, target);
		if self.boxed(owner, slot) {
			quote!({ value: $ty })
		} else {
			ty
		}
	}
	fn slot_new(&self, owner: TypeId, slot: &Ident, value: Tokens) -> Tokens {
		if self.boxed(owner, slot) {
			quote!({ value: $value })
		} else {
			value
		}
	}
	fn scalar(&self, id: TypeId) -> Option<Scalar> {
		scalar_of(self.types, id)
	}
	fn item(&self, id: TypeId, name: &str) -> Tokens {
		TsMeta::item(self, id, name)
	}
}

fn read_reference(meta: &TsMeta, from_locals: bool, input: &Field) -> Tokens {
	let mut expr: Tokens = quote!(_0);
	for (i, item) in input.0.iter().enumerate() {
		expr = match &item.segment {
			FieldRefSegment::Arg(name) => quote!(args.$(name.as_str())),
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					quote!($(common::local(to)))
				} else {
					quote!($expr.$(to.as_str()))
				}
			}
			FieldRefSegment::Virtual(name) => {
				let f = meta.item(item.from.get(), format!("virtual_{}", name.as_str()));
				quote!($f($expr))
			}
		};
	}
	expr
}

fn read_or_const_reference(
	meta: &TsMeta,
	from_locals: bool,
	input: &FieldOrConst,
	param: TypeId,
) -> Tokens {
	match input {
		FieldOrConst::Field(field) => read_reference(meta, from_locals, field),
		FieldOrConst::Const(value) => meta.literal(value, param),
	}
}

fn args_passing(meta: &TsMeta, input: &Type) -> Tokens {
	let params = &meta.types[input.target].params;
	let args: Vec<Tokens> = input
		.args
		.iter()
		.map(|(name, field)| {
			let param = params
				.iter()
				.find(|(n, _)| n == name)
				.map(|(_, t)| *t)
				.unwrap_or_default();
			quote!($(name.as_str()): $(read_or_const_reference(meta, true, field, param)))
		})
		.collect();
	quote!({ $(for a in args join(, ) => $a) })
}

fn native_codegen<'m>(
	meta: &TsMeta<'m>,
	id: TypeId,
) -> Result<(&'m mir::NativeType, &'m dyn NativeCodegen<JavaScript>)> {
	let TypeKind::Native(n) = &meta.types[id].kind else {
		bail!("{} is not a native", meta.name_str(id));
	};
	let (spec, _) = n
		.specializer
		.as_ref()
		.ok_or_else(|| anyhow!("{} has no specializer", meta.name_str(id)))?;
	let codegen = meta
		.reg
		.codegen::<JavaScript>(spec.as_str())
		.ok_or_else(|| anyhow!("no typescript codegen for specializer {spec:?}"))?;
	Ok((n, codegen))
}

fn lower(meta: &TsMeta, expr: &Expr, from_locals: bool) -> Result<Tokens> {
	let rec = |e: &Expr| lower(meta, e, from_locals);
	let cx = LowerCx {
		names: meta,
		types: meta.types,
		lower: &rec,
	};
	Ok(match expr {
		Expr::Var(name, _) => quote!($(name.as_str())),
		Expr::Field(f) => read_reference(meta, from_locals, f),
		Expr::Const(v) => bail!("untyped constant {v:?}"),
		Expr::Construct(id, c) => match (scalar_of(meta.types, *id), c) {
			(Some(_), Construction::Literal(v)) => literal(meta.types, v, *id),
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
			let (n, codegen) = native_codegen(meta, from)?;
			codegen.cast(&cx, from, n, rec(e)?, *to)?
		}
		Expr::Eq(a, b) => {
			let scalar = |e: &Expr| e.ty().is_some_and(|t| scalar_of(meta.types, t).is_some());
			if scalar(a) && scalar(b) {
				quote!(($(rec(a)?) === $(rec(b)?)))
			} else {
				quote!($(js::import("../pds.ts", "equal"))($(rec(a)?), $(rec(b)?)))
			}
		}
	})
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let meta = TsMeta::new(input, reg);
	let virtuals_used = common::virtuals_used(input);
	let elab = Elaborator { types: input, reg };
	let elaborated = common::elaborate(&elab)?;

	let pinput = &js::import("../pds.ts", "Input");
	let poutput = &js::import("../pds.ts", "Output");

	let mut modules = Vec::new();
	for (id, ty) in &input.0 {
		let has_args = !ty.params.is_empty();
		let param_ty = |par: TypeId| -> Tokens {
			if par.is_valid() {
				Names::ty(&meta, par)
			} else {
				quote!(number)
			}
		};
		let args_struct = if has_args {
			quote! {
				export type Args = {
					$(for (name, par) in ty.params.iter() join($['\r']) => $(name.as_str()): $(param_ty(*par));)
				};
			}
		} else {
			quote!()
		};
		let args_param = || -> Tokens {
			if has_args {
				quote!(args: Args)
			} else {
				quote!()
			}
		};
		let args_of = |target: TypeId, use_: &Type| -> Tokens {
			if input[target].params.is_empty() {
				quote!()
			} else {
				quote!(, $(args_passing(&meta, use_)))
			}
		};
		let body: Tokens = match &ty.kind {
			TypeKind::Alias(target) => {
				quote! {
					export type Ty = $(Names::ty(&meta, target.target));
					export function serialize(_0: Ty, output: $poutput): void {
						$(meta.item(target.target, "serialize"))(_0, output);
					}
					export function deserialize(input: $pinput): Ty {
						return $(meta.item(target.target, "deserialize"))(input$(args_of(target.target, target)));
					}
					export function size_of(_0: Ty): number {
						return $(meta.item(target.target, "size_of"))(_0);
					}
				}
			}
			TypeKind::Native(native) => {
				match &native.specializer {
					Some((specializer, variant)) => {
						let resolve = |n: &str| {
							meta.raw_names
								.iter()
								.find(|(_, v)| v.as_str() == n)
								.map(|(k, _)| *k)
						};
						let Some(codegen) = reg.codegen::<JavaScript>(specializer.as_str()) else {
							anyhow::bail!("no typescript codegen for specializer {specializer:?}");
						};
						let args_struct_gen = || args_struct.clone();
						let pass_args = |u: &Type| -> Tokens {
							if input[u.target].params.is_empty() {
								quote!()
							} else {
								args_passing(&meta, u)
							}
						};
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
								read_ref: &|f, from_locals| read_reference(&meta, from_locals, f),
								read_arg: &|v, from_locals| {
									read_or_const_reference(&meta, from_locals, v, TypeId::default())
								},
								lower: &|e, from_locals| lower(&meta, e, from_locals),
							})
							.with_context(|| format!("typescript codegen for {:?}", ty.name))?
					}
					None => {
						let native_import = &js::import("../pds_native.ts", meta.name_str(*id));
						quote! {
							export type Ty = ReturnType<typeof $native_import.deserialize>;
							export const serialize = $native_import.serialize;
							export const deserialize = $native_import.deserialize;
							export const size_of = $native_import.size_of;
						}
					}
				}
			}
		};
		output.insert(
			format!("{}.ts", meta.name_str(*id)).into(),
			body.to_file_string().expect("formatting should succeed"),
		);
		modules.push(meta.name_str(*id));
	}

	let index: Tokens = quote! {
		$(for m in &modules join($['\r']) => export * as $m from $(quoted(format!("./{m}.ts")));)
	};
	output.insert(
		"mod.ts".into(),
		index.to_file_string().expect("formatting should succeed"),
	);
	Ok(())
}
