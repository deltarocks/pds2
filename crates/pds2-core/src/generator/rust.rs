use genco::lang::rust::import;
use genco::prelude::*;
use genco::quote;
use rust::Tokens;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ast::{BinOp, FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, Type, TypeKind, TypeMap, Value};

use super::boxing::{self, Boxed};
use super::common::{self, scalar_of};
use super::recursion;
use super::views;
use super::GeneratorOutput;
use crate::specializer::expr::Elaborator;
use crate::specializer::{LowerCx, Names, NativeCodegen, NativeCodegenCtx, Registry, Scalar};
use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::Rust;

#[derive(Clone)]
enum Mode {
	Owned,
	View(Rc<views::ViewSet>),
	OwnedInView,
}

#[derive(Clone)]
struct RustMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	types: &'a TypeMap,
	reg: &'a Registry,
	boxed: Rc<Boxed>,
	guarded: Rc<BTreeSet<TypeId>>,
	inlining: Rc<RefCell<BTreeSet<TypeId>>>,
	mode: Mode,
}
impl<'a> RustMeta<'a> {
	fn new(source: &'a TypeMap, reg: &'a Registry) -> Result<Self> {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		Ok(Self {
			raw_names: Rc::new(raw_names),
			types: source,
			reg,
			boxed: Rc::new(boxing::analyze(source, reg)?),
			guarded: Rc::new(recursion::guarded(source, reg)?),
			inlining: Rc::default(),
			mode: Mode::Owned,
		})
	}
	fn with_mode(&self, mode: Mode) -> Self {
		Self {
			mode,
			inlining: Rc::default(),
			..self.clone()
		}
	}
	fn view(&self) -> Option<&views::ViewSet> {
		match &self.mode {
			Mode::View(v) => Some(v),
			_ => None,
		}
	}
	fn owned_meta(&self) -> Self {
		match self.mode {
			Mode::View(_) => self.with_mode(Mode::OwnedInView),
			_ => self.clone(),
		}
	}
	fn inline_ty(&self, type_id: TypeId) -> Option<Tokens> {
		if self.view().is_some() {
			return None;
		}
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
	fn imported(&self, type_id: TypeId, ty: impl AsRef<str>) -> Tokens {
		match &self.mode {
			Mode::View(v) if v.same(type_id) => {
				quote!($(format!("super::super::{}::{}", self.name_str(type_id), ty.as_ref())))
			}
			Mode::OwnedInView => {
				quote!($(format!("super::super::{}::{}", self.name_str(type_id), ty.as_ref())))
			}
			_ => {
				let i = import(format!("super::{}", self.name_str(type_id)), ty.as_ref()).qualified();
				quote!($i)
			}
		}
	}
	fn owned_path(&self, type_id: TypeId, item: &str) -> Tokens {
		match self.mode {
			Mode::Owned => self.imported(type_id, item),
			_ => quote!($(format!("super::super::{}::{}", self.name_str(type_id), item))),
		}
	}
	fn ty(&self, type_id: TypeId) -> Tokens {
		assert!(type_id.is_valid());
		if let Some(v) = self.view() {
			if v.same(type_id) {
				return self.imported(type_id, "Ty");
			}
			return quote!($(self.imported(type_id, "Ty"))::<'i>);
		}
		match self.inline_ty(type_id) {
			Some(ty) => ty,
			None => self.imported(type_id, "Ty"),
		}
	}
	fn virt(&self, type_id: TypeId, field: Ident) -> Tokens {
		self.imported(type_id, format!("virtual_{}", field.as_str()))
	}
	fn args(&self, type_id: TypeId) -> Tokens {
		self.imported(type_id, "Args")
	}
	fn serialize(&self, type_id: TypeId) -> Tokens {
		self.imported(type_id, self.entry(type_id, "serialize"))
	}
	fn deserialize(&self, type_id: TypeId) -> Tokens {
		self.imported(type_id, self.entry(type_id, "deserialize"))
	}
	fn entry(&self, type_id: TypeId, name: &str) -> String {
		if self.guarded.contains(&type_id) {
			format!("{name}_guarded")
		} else {
			name.to_owned()
		}
	}
	fn size_of(&self, type_id: TypeId) -> Tokens {
		self.imported(type_id, "size_of")
	}
}

pub struct RustNames<'a>(RustMeta<'a>);

impl<'a> RustNames<'a> {
	pub fn new(types: &'a TypeMap, reg: &'a Registry) -> Result<Self> {
		Ok(Self(RustMeta::new(types, reg)?))
	}
	pub fn ty(&self, id: TypeId) -> Tokens {
		Names::ty(&self.0, id)
	}
	pub fn item(&self, id: TypeId, name: &str) -> Tokens {
		Names::item(&self.0, id, name)
	}
	pub fn module(&self, id: TypeId) -> String {
		self.0.name_str(id)
	}
	pub fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		Names::boxed(&self.0, owner, slot)
	}
	pub fn lower(&self, expr: &Expr) -> Result<Tokens> {
		lower(&self.0, expr, false)
	}
}

impl Names<Rust> for RustMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		RustMeta::ty(self, id)
	}
	fn serialize(&self, id: TypeId) -> Tokens {
		RustMeta::serialize(self, id)
	}
	fn deserialize(&self, id: TypeId) -> Tokens {
		RustMeta::deserialize(self, id)
	}
	fn size_of(&self, id: TypeId) -> Tokens {
		RustMeta::size_of(self, id)
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
		self.imported(id, name)
	}
	fn owned(&self, id: TypeId) -> Tokens {
		self.owned_path(id, "Ty")
	}
	fn view_fn(&self, id: TypeId) -> Tokens {
		if self.view().is_some_and(|v| v.same(id)) {
			quote!(::core::clone::Clone::clone)
		} else {
			self.imported(id, "view")
		}
	}
	fn into_view_fn(&self, id: TypeId) -> Tokens {
		let Some(v) = self.view() else {
			return self.imported(id, "into_view");
		};
		match v.into(id) {
			views::Into::Module => {
				let target = v.into_target(id);
				let item = if v.moves(target) { "into_view" } else { "view" };
				let i = import(format!("super::{}", self.name_str(target)), item).qualified();
				quote!($i)
			}
			views::Into::Take => quote!(::core::mem::take),
			views::Into::Copy | views::Into::Alias(_) => quote!(::core::clone::Clone::clone),
		}
	}
}

/// Emit an identifier, raw-prefixed when it collides with a rust keyword
/// (protodef loves fields named `type`).
pub fn rname(i: &Ident) -> Tokens {
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
		Expr::Const(v) => unreachable!("untyped constant {v:?} as an argument"),
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
	let mut owned = false;
	for (i, field) in input.0.iter().enumerate() {
		let from = field.from.get();
		let from_local = i == 0 && from_locals;
		if let Some(v) = types.view() {
			let read_through = !from_local && !matches!(field.segment, FieldRefSegment::Arg(_));
			if read_through && from.is_valid() && !v.borrows(from) {
				owned = true;
			}
		}
		let to_ty = if owned {
			types.owned_path(field.to.get(), "Ty")
		} else {
			types.ty(field.to.get())
		};
		indexing.push(match &field.segment {
			FieldRefSegment::Arg(name) => {
				assert_eq!(i, 0);
				quote! {
					let _0 = args.$(rname(name));
				}
			}
			FieldRefSegment::Down { to, .. } => {
				if from_local {
					// deserialize locals are f_-prefixed to avoid shadowing
					// `input`/`args`
					quote!(let _0: &$to_ty = &$(common::local(to));)
				} else {
					quote!(let _0: &$to_ty = &_0.$(rname(to));)
				}
			}
			FieldRefSegment::Virtual(name) => {
				let local = format!("__v{i}");
				quote! {
					let $(&local): $to_ty = $(types.virt(from, name.clone()))(_0);
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
		Scalar::Bool | Scalar::String | Scalar::Bytes => "",
	};
	let bytes = |b: &[u8]| quote!(vec![$(for x in b join(, ) => $(format!("{x}_u8")))]);
	Ok(match (s, value) {
		(Scalar::Bool, Value::Bool(b)) => quote!($(b.to_string())),
		(Scalar::String, Value::String(v)) => quote!(String::from($(quoted(v.as_str())))),
		(Scalar::Bytes, Value::String(v)) => bytes(v.as_bytes()),
		(Scalar::Bytes, Value::Bytes(v)) => bytes(v),
		(Scalar::F32 | Scalar::F64, Value::Float(f)) if f.is_finite() => {
			quote!($(format!("{f:?}_{suffix}")))
		}
		(s, Value::Signed(n)) if s.is_number() => quote!($(format!("{n}_{suffix}"))),
		(s, Value::Unsigned(n)) if s.is_number() => quote!($(format!("{n}_{suffix}"))),
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
	let owned = meta.owned_meta();
	let cx = LowerCx {
		names: &owned,
		types: meta.types,
		lower: &rec,
	};
	let meta_for_values = &owned;
	Ok(match expr {
		Expr::Var(name, _) => quote!($(name.as_str()).clone()),
		Expr::Field(f) => {
			quote!(($(generate_read_reference(meta, from_locals, f, None))).clone())
		}
		Expr::Const(v) => bail!("untyped constant {v:?}"),
		Expr::Record(_) => bail!("untyped record"),
		Expr::Serialized(_) => bail!("serialized(...) was not evaluated"),
		Expr::Deserialized(_) => bail!("deserialized(...) was not evaluated"),
		Expr::List(items) => {
			let items = items.iter().map(rec).collect::<Result<Vec<_>>>()?;
			quote!(vec![$(for i in items join(, ) => $i)])
		}
		Expr::Construct(id, _) if meta.types.is_void(*id) => quote!(()),
		Expr::Construct(id, c) => match (scalar_of(meta.types, *id), c) {
			(Some(s), Construction::Literal(v)) => literal(s, v)?,
			_ => {
				let (n, codegen) = native_codegen(meta_for_values, *id)?;
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
			let numeric = |t: TypeId| scalar_of(meta.types, t).is_some_and(Scalar::is_number);
			if numeric(from) && numeric(*to) {
				quote!((($(rec(e)?)) as $(meta.ty(*to))))
			} else {
				let (n, codegen) = native_codegen(meta_for_values, from)?;
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
				Some(Scalar::Bool | Scalar::String | Scalar::Bytes) | None => {
					bail!("arithmetic on a non-number")
				}
				Some(_) => {
					let method = match op {
						BinOp::Add => "wrapping_add",
						BinOp::Sub => "wrapping_sub",
						BinOp::Mul => "wrapping_mul",
						BinOp::Div => "wrapping_div",
					};
					quote!(($a).$method($b))
				}
			}
		}
		Expr::Eq(a, b) => quote!(($(rec(a)?) == $(rec(b)?))),
	})
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let meta = RustMeta::new(input, reg)?;
	let ids: Vec<TypeId> = input.0.keys().copied().collect();
	let modules = emit(&meta, input, reg, &ids, "", output)?;
	output.insert(
		"mod.rs".into(),
		quote! {
			$(for m in modules join($['\r']) => $m)
		}
		.to_file_string()
		.expect("formatting should succeed"),
	);
	Ok(())
}

pub fn view_generator(
	input: &mir::TypeMap,
	output: &mut GeneratorOutput,
	reg: &Registry,
	namespace: &str,
) -> Result<()> {
	let owned = RustMeta::new(input, reg)?;
	let set = Rc::new(views::analyze(input, reg, &owned.boxed, namespace)?);
	let meta = owned.with_mode(Mode::View(set.clone()));
	let ids: Vec<TypeId> = set.included.iter().copied().filter(|id| set.emitted(*id)).collect();
	let modules = emit(&meta, input, reg, &ids, "view/", output)?;
	let aliases = set.roots.iter().map(|(name, id)| {
		let home = if set.same(*id) { "super" } else { "self" };
		quote!(pub use $home::$(meta.name(*id)) as $(name.as_str());)
	});
	output.insert(
		"view/mod.rs".into(),
		quote! {
			$(for m in modules join($['\r']) => $m)
			$(for a in aliases join($['\r']) => $a)
		}
		.to_file_string()
		.expect("formatting should succeed"),
	);
	output
		.get_mut(&std::path::PathBuf::from("mod.rs"))
		.ok_or_else(|| anyhow!("views need the owned modules generated first"))?
		.push_str("pub mod view;\n");
	Ok(())
}

fn native_view(meta: &RustMeta, id: TypeId) -> Tokens {
	quote! {
		pub use $(format!("crate::pds_native::view::{}", meta.name_str(id)))::*;
	}
}

fn emit(
	meta: &RustMeta,
	input: &mir::TypeMap,
	reg: &Registry,
	ids: &[TypeId],
	dir: &str,
	output: &mut GeneratorOutput,
) -> Result<Vec<Tokens>> {
	let virtuals_used = common::virtuals_used(input);
	let elab = Elaborator { types: input, reg };
	let elaborated = common::elaborate(&elab)?;
	for id in ids {
		let def = &input[*id];
		for e in def.exprs() {
			if !matches!(e, Expr::Field(_) | Expr::Const(_)) {
				lower(meta, e, true).with_context(|| format!("in {:?}", def.name))?;
			}
		}
	}

	let mut out_module = vec![];
	let view = meta.view();
	let lt = &if view.is_some() { quote!(<'i>) } else { quote!() };

	for id in ids {
		let ty = &input[*id];
		let pinput = &import("crate::pds", "Input");
		let poutput = &import("crate::pds", "Output");
		let presult = &import("crate::pds", "Result");
		let has_args = !ty.params.is_empty();
		let kind = view.map(|v| v.kind(*id));
		let forward = kind == Some(views::Kind::Native);
		let take = kind == Some(views::Kind::Same);
		// Params with no inferred type (only ever bound to constants) fall back to a
		// plain integer.
		let param_lt = |par: TypeId| par.is_valid() && view.is_some_and(|v| !v.same(par));
		let param_ty = |par: TypeId| -> Tokens {
			if !par.is_valid() {
				quote!(usize)
			} else if view.is_some() && !param_lt(par) {
				meta.owned_path(par, "Ty")
			} else {
				meta.ty(par)
			}
		};
		let args_lt = !forward && ty.params.iter().any(|(_, p)| param_lt(*p));
		let args_struct = quote! {
			$(if has_args =>
				pub struct Args<'a$(if args_lt => , 'i)> {
					$(for (name, par) in ty.params.iter() join($['\r']) => pub $(rname(name)): &'a $(param_ty(*par)),)
				}
			)
		};
		let args_param = &if !has_args {
			quote!()
		} else if args_lt {
			quote!(args: Args<'_, 'i>,)
		} else {
			quote!(args: Args<'_>,)
		};
		// args to pass when referencing `target` from generated code
		let args_of = |target: TypeId, ty: &Type| -> Tokens {
			if input[target].params.is_empty() {
				quote!()
			} else {
				quote!($(generate_args_passing(meta, ty)),)
			}
		};
		let def = if forward {
			native_view(meta, *id)
		} else if !ty.kind.is_native() {
			let mod_contents = match &ty.kind {
				TypeKind::Alias(target) => {
					quote! {
						pub type Ty$lt = $(meta.ty(target.target));
						pub fn serialize$lt(_0: &Ty$lt, output: &mut $poutput) -> $presult<()> {
							$(meta.serialize(target.target))(_0, output)
						}
						pub fn deserialize$lt(input: &mut $pinput$lt) -> $presult<Ty$lt> {
							$(meta.deserialize(target.target))(
								input,
								$(args_of(target.target, target))
							)
						}
						pub fn size_of$lt(_0: &Ty$lt) -> usize {
							$(meta.size_of(target.target))(_0)
						}
						$(if view.is_some() =>
							pub fn view<'i>(_0: &'i $(meta.owned_path(*id, "Ty"))) -> Ty<'i> {
								$(meta.imported(target.target, "view"))(_0)
							}
						)
						$(if view.is_some_and(|v| v.moves(*id)) =>
							pub fn into_view<'i>(_0: &'i mut $(meta.owned_path(*id, "Ty"))) -> Ty<'i> {
								$(meta.into_view_fn(target.target))(_0)
							}
						)
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
					let args_param_gen = || args_param.clone();
					let pass_args = |u: &Type| args_of(u.target, u);
					let virtual_used =
						|name: &str| virtuals_used.contains(&(*id, Ident::new(name)));
					let read = |v: &Expr, from_locals| {
						generate_read_or_const_reference(meta, from_locals, v, None)
					};
					let lower_expr = |e: &Expr, from_locals| lower(meta, e, from_locals);
					let ctx = NativeCodegenCtx {
						id: *id,
						variant,
						native,
						params: &ty.params,
						names: meta,
						resolve: &resolve,
						args_struct: &args_struct_gen,
						args_param: &args_param_gen,
						pass_args: &pass_args,
						types: input,
						elab: &elab,
						exprs: elaborated.exprs(*id),
						cast_to: elaborated.cast_to(*id),
						virtual_used: &virtual_used,
						read: &read,
						lower: &lower_expr,
						view: view.is_some(),
						take,
					};
					let into_view = || -> Result<Tokens> {
						codegen
							.into_view(&ctx)?
							.ok_or_else(|| anyhow!("specializer {specializer:?} has no into_view"))
					};
					if take {
						into_view().with_context(|| format!("into_view for {:?}", ty.name))?
					} else {
						let module = codegen
							.generate(&ctx)
							.with_context(|| format!("codegen for {:?}", ty.name))?;
						match view {
							Some(v) if v.moves(*id) => {
								let into = into_view().with_context(|| format!("into_view for {:?}", ty.name))?;
								quote!($module $['\r'] $into)
							}
							_ => module,
						}
					}
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
		let def = if meta.guarded.contains(id) && !forward && !take {
			let check = &import("crate::pds::stack", "check").qualified();
			quote! {
				$def
				pub fn serialize_guarded$lt(_0: &Ty$lt, output: &mut $poutput) -> $presult<()> {
					$check()?;
					serialize(_0, output)
				}
				pub fn deserialize_guarded$lt(input: &mut $pinput$lt, $args_param) -> $presult<Ty$lt> {
					$check()?;
					deserialize(input, $(if has_args => args,))
				}
			}
		} else {
			def
		};
		output.insert(
			format!("{dir}{}.rs", meta.name_str(*id)).into(),
			def.to_file_string().expect("formatting should succeed"),
		);
		out_module.push(quote! {
			pub mod $(meta.name(*id));
		})
	}
	Ok(out_module)
}
