use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::{js, JavaScript};
use genco::prelude::*;

use std::rc::Rc;

use super::common::{self, sanitize, scalar_of};
use super::naming::{self, Modules};
use super::GeneratorOutput;
use crate::ast::{BinOp, FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, Type, TypeKind, TypeMap, Value};
use crate::specializer::expr::Elaborator;
use crate::specializer::{
	specialization_use, LowerCx, Names, NativeCodegen, NativeCodegenCtx, Registry, Scalar,
};

type Tokens = js::Tokens;

struct TsMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	modules: Rc<Modules>,
	here: Vec<String>,
	types: &'a TypeMap,
	reg: &'a Registry,
	inlining: RefCell<BTreeSet<TypeId>>,
}

impl<'a> TsMeta<'a> {
	fn new(types: &'a TypeMap, reg: &'a Registry) -> Result<Self> {
		Ok(Self {
			raw_names: Rc::new(
				types
					.0
					.iter()
					.map(|(id, ty)| (*id, ty.name.readable_name()))
					.collect(),
			),
			modules: Rc::new(naming::raw_modules(types)?),
			here: vec![],
			types,
			reg,
			inlining: RefCell::default(),
		})
	}
	fn at(&self, id: TypeId) -> Self {
		let path = self.modules.path(id);
		Self {
			raw_names: self.raw_names.clone(),
			modules: self.modules.clone(),
			here: path[..path.len() - 1].iter().map(|m| m.stem.clone()).collect(),
			types: self.types,
			reg: self.reg,
			inlining: RefCell::default(),
		}
	}
	fn up(&self) -> String {
		"../".repeat(self.here.len() + 1)
	}
	fn name_str(&self, id: TypeId) -> String {
		assert!(id.is_valid());
		format!("pds_{}", self.raw_names[&id])
	}
	fn module(&self, id: TypeId) -> js::Import {
		let path = self.modules.path(id);
		let (dir, file) = path.split_at(path.len() - 1);
		let here: Vec<&str> = self.here.iter().map(String::as_str).collect();
		let dir: Vec<&str> = dir.iter().map(|m| m.stem.as_str()).collect();
		let (up, rest) = naming::relative(&here, &dir);
		let prefix = if up == 0 { "./".to_owned() } else { "../".repeat(up) };
		let target = rest
			.into_iter()
			.chain([file[0].stem.as_str()])
			.collect::<Vec<_>>()
			.join("/");
		let alias = std::iter::once("pds")
			.chain(path.iter().map(|m| m.ident.as_str()))
			.collect::<Vec<_>>()
			.join("$");
		js::import(format!("{prefix}{target}.ts"), alias).into_wildcard()
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
	fn native_codegen(&self, id: TypeId) -> Option<(&mir::NativeType, &dyn NativeCodegen<JavaScript>)> {
		let TypeKind::Native(native) = &self.types[id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		Some((native, self.reg.codegen::<JavaScript>(specializer.as_str())?))
	}
	fn nullable(&self, id: TypeId) -> bool {
		match &self.types[id].kind {
			TypeKind::Alias(t) => self.nullable(t.target),
			_ => {
				self.types.is_void(id)
					|| self.native_codegen(id).is_some_and(|(n, c)| c.nullable(n))
			}
		}
	}
	fn literal(&self, value: &Value, ty: TypeId) -> Tokens {
		literal(self.types, value, ty)
	}
}

pub(crate) fn literal(types: &TypeMap, value: &Value, ty: TypeId) -> Tokens {
	let scalar = ty.is_valid().then(|| scalar_of(types, ty)).flatten();
	let wide = matches!(scalar, Some(Scalar::I64 | Scalar::U64));
	let bytes = |b: &[u8]| {
		let items = b.iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
		quote!(new Uint8Array([$items]))
	};
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
		Value::Float(f) => quote!($(format!("{f:?}"))),
		Value::Bytes(b) => bytes(b),
		Value::String(s) if scalar == Some(Scalar::Bytes) => bytes(s.as_bytes()),
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
		self.native_codegen(owner).is_some_and(|(n, c)| {
			c.wraps_nullable(n, slot)
				&& specialization_use(n, slot.as_str()).is_some_and(|u| self.nullable(u.target))
		})
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
	fn runtime(&self, item: &str) -> Tokens {
		quote!($(js::import(format!("{}pds.ts", self.up()), item)))
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
	input: &Expr,
	param: TypeId,
) -> Tokens {
	match input {
		Expr::Field(field) => read_reference(meta, from_locals, field),
		Expr::Const(value) => meta.literal(value, param),
		e => lower(meta, e, from_locals).expect("validated before codegen"),
	}
}

fn wrap(s: Scalar, x: Tokens) -> Tokens {
	match s {
		Scalar::I8 => quote!((($x) << 24 >> 24)),
		Scalar::U8 => quote!((($x) & 0xff)),
		Scalar::I16 => quote!((($x) << 16 >> 16)),
		Scalar::U16 => quote!((($x) & 0xffff)),
		Scalar::I32 => quote!((($x) | 0)),
		Scalar::U32 => quote!((($x) $(">>>") 0)),
		Scalar::I64 => quote!(BigInt.asIntN(64, $x)),
		Scalar::U64 => quote!(BigInt.asUintN(64, $x)),
		Scalar::F32 => quote!(Math.fround($x)),
		Scalar::F64 | Scalar::Bool | Scalar::String | Scalar::Bytes => x,
	}
}

fn numeric_cast(from: Scalar, to: Scalar, x: Tokens) -> Tokens {
	let wide = |s: Scalar| matches!(s, Scalar::I64 | Scalar::U64);
	let float = |s: Scalar| matches!(s, Scalar::F32 | Scalar::F64);
	if float(to) {
		return wrap(to, if wide(from) { quote!(Number($x)) } else { x });
	}
	let x = if float(from) { quote!(Math.trunc($x)) } else { x };
	match (wide(from), wide(to)) {
		(true, true) => wrap(to, x),
		(false, true) => wrap(to, quote!(BigInt($x))),
		(true, false) => wrap(to, quote!(Number(BigInt.asIntN(32, $x)))),
		(false, false) => wrap(to, x),
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
		Expr::Record(_) => bail!("untyped record"),
		Expr::Serialized(_) => bail!("serialized(...) was not evaluated"),
		Expr::Deserialized(_) => bail!("deserialized(...) was not evaluated"),
		Expr::List(items) => {
			let items = items.iter().map(rec).collect::<Result<Vec<_>>>()?;
			quote!([$(for i in items join(, ) => $i)])
		}
		Expr::Construct(id, _) if meta.types.is_void(*id) => quote!(null),
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
			let numeric = |t: TypeId| scalar_of(meta.types, t).filter(|s| s.is_number());
			match (numeric(from), numeric(*to)) {
				(Some(f), Some(t)) => numeric_cast(f, t, rec(e)?),
				_ => {
					let (n, codegen) = native_codegen(meta, from)?;
					codegen.cast(&cx, from, n, rec(e)?, *to)?
				}
			}
		}
		Expr::Binary(op, a, b) => {
			let ty = expr
				.ty()
				.ok_or_else(|| anyhow!("untyped arithmetic"))?;
			let s = scalar_of(meta.types, ty)
				.filter(|s| s.is_number())
				.ok_or_else(|| anyhow!("arithmetic on a non-number"))?;
			let (a, b) = (rec(a)?, rec(b)?);
			match (op, s) {
				(BinOp::Mul, Scalar::I32 | Scalar::U32) => wrap(s, quote!(Math.imul($a, $b))),
				(BinOp::Div, Scalar::F32 | Scalar::F64 | Scalar::I64 | Scalar::U64) => wrap(s, quote!(($a / $b))),
				(BinOp::Div, _) => wrap(s, quote!(Math.trunc($a / $b))),
				_ => wrap(s, quote!(($a $(op.symbol()) $b))),
			}
		}
		Expr::Eq(a, b) => {
			let scalar = |e: &Expr| {
				e.ty()
					.and_then(|t| scalar_of(meta.types, t))
					.is_some_and(|s| s != Scalar::Bytes)
			};
			if scalar(a) && scalar(b) {
				quote!(($(rec(a)?) === $(rec(b)?)))
			} else {
				quote!($(Names::runtime(meta, "equal"))($(rec(a)?), $(rec(b)?)))
			}
		}
	})
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let root = TsMeta::new(input, reg)?;
	let virtuals_used = common::virtuals_used(input);
	let elab = Elaborator { types: input, reg };
	let elaborated = common::elaborate(&elab)?;
	for def in input.0.values() {
		for e in def.exprs() {
			if !matches!(e, Expr::Field(_) | Expr::Const(_)) {
				lower(&root, e, true).with_context(|| format!("in {:?}", def.name))?;
			}
		}
	}

	for (id, ty) in &input.0 {
		let meta = root.at(*id);
		let pinput = &Names::runtime(&meta, "Input");
		let poutput = &Names::runtime(&meta, "Output");
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
								read: &|v, from_locals| {
									read_or_const_reference(&meta, from_locals, v, TypeId::default())
								},
								lower: &|e, from_locals| lower(&meta, e, from_locals),
								view: false,
								take: false,
								type_name: None,
								declared: &Default::default(),
							})
							.with_context(|| format!("typescript codegen for {:?}", ty.name))?
					}
					None => {
						let native_import =
							&js::import(format!("{}pds_native.ts", meta.up()), meta.name_str(*id));
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
			root.modules.file(*id, "ts").into(),
			body.to_file_string().expect("formatting should succeed"),
		);
	}

	for (path, children) in root.modules.dirs(input.0.keys().copied()) {
		let exports = children.iter().map(|(m, is_dir)| {
			let file = if *is_dir {
				format!("./{}/mod.ts", m.stem)
			} else {
				format!("./{}.ts", m.stem)
			};
			quote!(export * as $(&m.ident) from $(quoted(file));)
		});
		let index: Tokens = quote!($(for e in exports join($['\r']) => $e));
		let stems: Vec<&str> = path.iter().map(|m| m.stem.as_str()).collect();
		let file = if stems.is_empty() {
			"mod.ts".to_owned()
		} else {
			format!("{}/mod.ts", stems.join("/"))
		};
		output.insert(file.into(), index.to_file_string().expect("formatting should succeed"));
	}
	Ok(())
}
