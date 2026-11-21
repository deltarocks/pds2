use genco::lang::rust::import;
use genco::prelude::*;
use genco::quote;
use rust::Tokens;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ast::{BinOp, FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, Type, TypeKind, TypeMap, Value};

use super::boxing::{self, Boxed};
use super::common::{self, scalar_of};
use super::naming::{self, Case, Modules, Scope};
use super::recursion;
use super::split::{Layout, Split};
use super::views;
use super::GeneratorOutput;
use crate::specializer::expr::Elaborator;
use crate::specializer::{LowerCx, Names, NativeCodegen, NativeCodegenCtx, Registry, Scalar};
use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::Rust;

#[derive(Clone)]
enum Mode {
	Owned,
	View(Rc<views::ViewSet>),
	OwnedInView,
}

struct MemberNames {
	fields: Scope,
	variants: Scope,
	types: Scope,
	args: Scope,
}

fn member_names(
	types: &TypeMap,
	reg: &Registry,
	modules: &Modules,
) -> Result<BTreeMap<TypeId, MemberNames>> {
	let mut out = BTreeMap::new();
	for (id, def) in &types.0 {
		let members = match &def.kind {
			TypeKind::Native(n) => n
				.specializer
				.as_ref()
				.and_then(|(s, _)| reg.layout(s.as_str()))
				.map(|l| l.members(n))
				.unwrap_or_default(),
			TypeKind::Alias(_) => Default::default(),
		};
		fn strs(v: &[Ident]) -> Vec<&str> {
			v.iter().map(Ident::as_str).collect()
		}
		let own = modules.path(*id).last().and_then(|m| m.ty.as_deref()).unwrap_or("Ty");
		let names = (|| -> Result<MemberNames> {
			Ok(MemberNames {
				fields: Scope::new(Case::Snake, strs(&members.fields), &[]).context("fields")?,
				variants: Scope::new(Case::Pascal, strs(&members.variants), &[]).context("variants")?,
				types: Scope::new(Case::Pascal, strs(&members.types), &["Ty", "Args", own])
					.context("inner types")?,
				args: Scope::new(Case::Snake, def.params.iter().map(|(n, _)| n.as_str()), &[])
					.context("arguments")?,
			})
		})()
		.with_context(|| format!("naming members of {}", def.name.readable_name()))?;
		out.insert(*id, names);
	}
	Ok(out)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Loc {
	krate: Option<usize>,
	path: Vec<String>,
}

impl Loc {
	fn view(mut self) -> Self {
		self.path.insert(0, "view".to_owned());
		self
	}
}

#[derive(Clone)]
struct RustMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	modules: Rc<Modules>,
	members: Rc<BTreeMap<TypeId, MemberNames>>,
	here: Rc<Loc>,
	layout: Option<Rc<Layout>>,
	deps: Rc<RefCell<BTreeMap<usize, BTreeSet<usize>>>>,
	types: &'a TypeMap,
	reg: &'a Registry,
	boxed: Rc<Boxed>,
	guarded: Rc<BTreeSet<TypeId>>,
	move_only: Rc<BTreeSet<TypeId>>,
	inlining: Rc<RefCell<BTreeSet<TypeId>>>,
	mode: Mode,
}

fn move_only(types: &TypeMap, reg: &Registry) -> Result<BTreeSet<TypeId>> {
	let mut set: BTreeSet<TypeId> = types
		.0
		.iter()
		.filter(|(_, def)| match &def.kind {
			TypeKind::Native(n) => n
				.specializer
				.as_ref()
				.and_then(|(s, _)| reg.codegen::<Rust>(s.as_str()))
				.is_some_and(|c| !c.value_traits(n)),
			TypeKind::Alias(_) => false,
		})
		.map(|(id, _)| *id)
		.collect();
	if set.is_empty() {
		return Ok(set);
	}
	let children = types
		.0
		.keys()
		.map(|id| Ok((*id, views::children(types, reg, *id)?)))
		.collect::<Result<BTreeMap<_, _>>>()?;
	loop {
		let before = set.len();
		for (id, kids) in &children {
			if kids.iter().any(|k| set.contains(k)) {
				set.insert(*id);
			}
		}
		if set.len() == before {
			return Ok(set);
		}
	}
}

impl<'a> RustMeta<'a> {
	fn new(source: &'a TypeMap, reg: &'a Registry, crates: Option<&str>) -> Result<Self> {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		let modules = naming::rust_modules(source)?;
		let layout = crates
			.map(|name| Layout::new(source, reg, &modules, name))
			.transpose()?;
		Ok(Self {
			raw_names: Rc::new(raw_names),
			members: Rc::new(member_names(source, reg, &modules)?),
			modules: Rc::new(modules),
			here: Rc::default(),
			layout: layout.map(Rc::new),
			deps: Rc::default(),
			types: source,
			reg,
			boxed: Rc::new(boxing::analyze(source, reg)?),
			guarded: Rc::new(recursion::guarded(source, reg)?),
			move_only: Rc::new(move_only(source, reg)?),
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
	fn at(&self, here: Loc) -> Self {
		Self {
			here: Rc::new(here),
			inlining: Rc::default(),
			..self.clone()
		}
	}
	fn located(&self, id: TypeId) -> (Option<usize>, &[naming::Module]) {
		let path = self.modules.path(id);
		match &self.layout {
			None => (None, path),
			Some(l) if l.strip.contains(&id) => (Some(l.of[&id]), &path[1..]),
			Some(l) => (Some(l.of[&id]), path),
		}
	}
	fn owned_module(&self, id: TypeId) -> Loc {
		let (krate, path) = self.located(id);
		Loc {
			krate,
			path: path.iter().map(|m| m.ident.clone()).collect(),
		}
	}
	fn view_module(&self, id: TypeId) -> Loc {
		self.owned_module(id).view()
	}
	fn home(&self, id: TypeId) -> Loc {
		match self.mode {
			Mode::Owned => self.owned_module(id),
			_ => self.view_module(id),
		}
	}
	fn file(&self, id: TypeId, dir: &str) -> std::path::PathBuf {
		let (krate, path) = self.located(id);
		let stems: Vec<&str> = path.iter().map(|m| m.stem.as_str()).collect();
		self.dir_file(krate, &format!("{dir}{}.rs", stems.join("/")))
	}
	fn dir_file(&self, krate: Option<usize>, file: &str) -> std::path::PathBuf {
		match (krate, &self.layout) {
			(Some(k), Some(l)) => l.crates[k].file(file),
			_ => file.into(),
		}
	}
	fn rel(&self, to: &Loc) -> String {
		if self.here.krate == to.krate {
			let here: Vec<&str> = self.here.path.iter().map(String::as_str).collect();
			let to: Vec<&str> = to.path.iter().map(String::as_str).collect();
			return naming::rust_relative(&here, &to);
		}
		let layout = self.layout.as_ref().expect("only split output spans crates");
		let target = to.krate.expect("split output places every type in a crate");
		let head: Vec<String> = match self.here.krate {
			Some(from) => {
				self.deps.borrow_mut().entry(from).or_default().insert(target);
				vec![format!("::{}", layout.crates[target].ident)]
			}
			None => std::iter::once(format!("::{}", layout.umbrella))
				.chain(layout.crates[target].ns.clone())
				.collect(),
		};
		head.into_iter().chain(to.path.iter().cloned()).collect::<Vec<_>>().join("::")
	}
	fn path_to(&self, module: &Loc, item: &str) -> Tokens {
		quote!($(format!("{}::{item}", self.rel(module))))
	}
	fn members_of(&self, owner: TypeId) -> &MemberNames {
		let mut id = owner;
		while let TypeKind::Alias(t) = &self.types[id].kind {
			id = t.target;
		}
		&self.members[&id]
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
	fn item_path(&self, type_id: TypeId, mut module: Loc, item: &str) -> Tokens {
		if item != "Ty" {
			return self.path_to(&module, item);
		}
		let ty = self.modules.path(type_id).last().and_then(|m| m.ty.clone());
		module.path.pop();
		module.path.push(ty.expect("a type module has a type name"));
		quote!($(self.rel(&module)))
	}
	fn imported(&self, type_id: TypeId, ty: impl AsRef<str>) -> Tokens {
		assert!(type_id.is_valid());
		let module = match &self.mode {
			Mode::View(v) if !v.same(type_id) => self.view_module(type_id),
			_ => self.owned_module(type_id),
		};
		self.item_path(type_id, module, ty.as_ref())
	}
	fn owned_path(&self, type_id: TypeId, item: &str) -> Tokens {
		self.item_path(type_id, self.owned_module(type_id), item)
	}
	fn nominal_name(&self, type_id: TypeId) -> Option<&str> {
		let ty = self.modules.path(type_id).last()?.ty.as_deref()?;
		naming::nominal(ty).then_some(ty)
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
	pub fn new(types: &'a TypeMap, reg: &'a Registry, here: &[&str], crates: Option<&str>) -> Result<Self> {
		let meta = RustMeta::new(types, reg, crates)?;
		Ok(Self(meta.at(Loc {
			krate: None,
			path: here.iter().map(|s| s.to_string()).collect(),
		})))
	}
	pub fn ty(&self, id: TypeId) -> Tokens {
		Names::ty(&self.0, id)
	}
	pub fn item(&self, id: TypeId, name: &str) -> Tokens {
		Names::item(&self.0, id, name)
	}
	pub fn module(&self, id: TypeId) -> String {
		self.0.rel(&self.0.owned_module(id))
	}
	pub fn flat_name(&self, id: TypeId) -> &str {
		&self.0.raw_names[&id]
	}
	pub fn field(&self, owner: TypeId, name: &str) -> String {
		Names::field(&self.0, owner, name)
	}
	pub fn variant(&self, owner: TypeId, name: &str) -> String {
		Names::variant(&self.0, owner, name)
	}
	pub fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		Names::boxed(&self.0, owner, slot)
	}
	pub fn serialize(&self, id: TypeId) -> Tokens {
		Names::serialize(&self.0, id)
	}
	pub fn deserialize(&self, id: TypeId) -> Tokens {
		Names::deserialize(&self.0, id)
	}
	pub fn size_of(&self, id: TypeId) -> Tokens {
		Names::size_of(&self.0, id)
	}
	pub fn lower(&self, expr: &Expr) -> Result<Tokens> {
		lower(&self.0, expr, false)
	}
}

pub struct DefinitionRustCtx<'a> {
	meta: RustMeta<'a>,
	by_path: BTreeMap<crate::ast::Path, TypeId>,
}

impl<'a> DefinitionRustCtx<'a> {
	fn new(meta: RustMeta<'a>) -> Self {
		let by_path = meta
			.types
			.0
			.iter()
			.map(|(id, def)| (def.name.clone(), *id))
			.collect();
		Self { meta, by_path }
	}
	pub fn types(&self) -> &'a TypeMap {
		self.meta.types
	}
	pub fn reg(&self) -> &'a Registry {
		self.meta.reg
	}
	pub fn type_id(&self, path: &crate::ast::Path) -> Result<TypeId> {
		self.by_path
			.get(path)
			.copied()
			.ok_or_else(|| anyhow!("no generated type {path:?}"))
	}
	pub fn names_in(&self, namespace: &[Ident]) -> Result<RustNames<'a>> {
		let (krate, path) = self.namespace_dir(namespace)?;
		Ok(RustNames(self.meta.at(Loc {
			krate,
			path: path.iter().map(|m| m.ident.clone()).collect(),
		})))
	}
	fn namespace_dir(&self, namespace: &[Ident]) -> Result<Dir> {
		let id = self
			.meta
			.types
			.0
			.iter()
			.find(|(_, def)| def.name.path.starts_with(namespace))
			.map(|(id, _)| *id)
			.ok_or_else(|| {
				let path: Vec<&str> = namespace.iter().map(Ident::as_str).collect();
				anyhow!("namespace {:?} has no generated module", path.join("::"))
			})?;
		let (krate, located) = self.meta.located(id);
		let dropped = self.meta.modules.path(id).len() - located.len();
		Ok((krate, located[..namespace.len() - dropped].to_vec()))
	}
}

fn definition_extras(
	meta: &RustMeta,
	reg: &Registry,
	definitions: &[crate::definition::DefinitionUse],
) -> Result<BTreeMap<Dir, Vec<Tokens>>> {
	let mut out: BTreeMap<Dir, Vec<Tokens>> = BTreeMap::new();
	if definitions.is_empty() {
		return Ok(out);
	}
	let ctx = DefinitionRustCtx::new(meta.clone());
	for (name, uses) in crate::definition::group(definitions) {
		let definition = reg
			.definition(name)
			.ok_or_else(|| anyhow!("definition {name:?} is not registered"))?;
		let extras = definition
			.generate_rust(&uses, &ctx)
			.with_context(|| format!("generating `{name}` definitions"))?;
		for extra in extras {
			let dir = ctx.namespace_dir(&extra.namespace)?;
			out.entry(dir).or_default().push(extra.tokens);
		}
	}
	Ok(out)
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
				self.path_to(&self.view_module(target), item)
			}
			views::Into::Take => quote!(::core::mem::take),
			views::Into::Copy | views::Into::Alias(_) => quote!(::core::clone::Clone::clone),
		}
	}
	fn runtime(&self, item: &str) -> Tokens {
		quote!($(import("crate::pds", item)))
	}
	fn field(&self, owner: TypeId, name: &str) -> String {
		self.members_of(owner).fields.get(name)
	}
	fn variant(&self, owner: TypeId, name: &str) -> String {
		self.members_of(owner).variants.get(name)
	}
	fn member_type(&self, owner: TypeId, name: &str) -> String {
		self.members_of(owner).types.get(name)
	}
	fn arg(&self, owner: TypeId, name: &str) -> String {
		self.members[&owner].args.get(name)
	}
	fn guarded(&self, id: TypeId) -> bool {
		self.guarded.contains(&id)
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
					let _0 = args.$(Names::arg(types, from, name.as_str()));
				}
			}
			FieldRefSegment::Down { to, .. } => {
				if from_local {
					// deserialize locals are f_-prefixed to avoid shadowing
					// `input`/`args`
					quote!(let _0: &$to_ty = &$(common::local(to));)
				} else {
					quote!(let _0: &$to_ty = &_0.$(Names::field(types, from, to.as_str()));)
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
		args.push(quote!(
			$(Names::arg(types, input.target, name.as_str())): $(generate_read_or_const_reference(types, true, field, None))
		))
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
		Expr::Call(f, _) => bail!("{f:?}(...) was not evaluated"),
		Expr::Typed(..) => bail!("typed expression was not elaborated"),
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
	generate(input, output, reg, None, None, &[])
}

pub fn generate(
	input: &mir::TypeMap,
	output: &mut GeneratorOutput,
	reg: &Registry,
	view: Option<&str>,
	split: Option<&Split>,
	definitions: &[crate::definition::DefinitionUse],
) -> Result<()> {
	ensure!(
		definitions.is_empty() || split.is_none(),
		"custom definitions can't be generated with --crates yet"
	);
	let meta = RustMeta::new(input, reg, split.map(|s| s.name.as_str()))?;
	let mut extras = definition_extras(&meta, reg, definitions)?;
	let ids: Vec<TypeId> = input.0.keys().copied().collect();
	emit(&meta, input, reg, &ids, "", &mut extras, output)?;
	if let Some(namespace) = view {
		views(&meta, input, reg, namespace, output)?;
	}
	match (split, &meta.layout) {
		(Some(split), Some(layout)) => layout.finish(split, &meta.deps.borrow(), output),
		_ => Ok(()),
	}
}

type Dir = (Option<usize>, Vec<naming::Module>);

fn mod_files(
	meta: &RustMeta,
	ids: &[TypeId],
	exports: &BTreeMap<TypeId, bool>,
	reexports: &BTreeMap<TypeId, TypeId>,
	dir: &str,
	extras: &mut BTreeMap<Dir, Vec<Tokens>>,
	output: &mut GeneratorOutput,
) -> Result<()> {
	let mut dirs: BTreeMap<Dir, BTreeMap<naming::Module, bool>> = BTreeMap::new();
	if meta.layout.is_none() {
		dirs.entry((None, vec![])).or_default();
	}
	for id in ids {
		let (krate, path) = meta.located(*id);
		dirs.entry((krate, vec![])).or_default();
		for i in 0..path.len() {
			dirs.entry((krate, path[..i].to_vec()))
				.or_default()
				.insert(path[i].clone(), i + 1 < path.len());
		}
	}
	let mut uses: BTreeMap<Dir, Vec<Tokens>> = BTreeMap::new();
	for (id, nominal) in exports {
		let (krate, path) = meta.located(*id);
		let (parent, last) = path.split_at(path.len() - 1);
		let m = &last[0];
		let ty = m.ty.as_ref().expect("a type module has a type name");
		uses.entry((krate, parent.to_vec())).or_default().push(if *nominal {
			quote!(pub use self::$(&m.ident)::$ty;)
		} else {
			quote!(pub use self::$(&m.ident)::Ty as $ty;)
		});
	}
	let mut aliased: BTreeMap<(Dir, naming::Module), TypeId> = BTreeMap::new();
	for (alias, target) in reexports {
		let (krate, path) = meta.located(*alias);
		let (parent, last) = path.split_at(path.len() - 1);
		aliased.insert(((krate, parent.to_vec()), last[0].clone()), *target);
	}
	for ((krate, path), children) in dirs {
		let mut idents = BTreeSet::new();
		for m in children.keys() {
			if !idents.insert(m.ident.as_str()) {
				bail!("two modules are named {:?} in {}", m.ident, meta.dir_file(krate, dir).display());
			}
		}
		let loc = Loc {
			krate,
			path: path.iter().map(|m| m.ident.clone()).collect(),
		};
		let at = match meta.mode {
			Mode::Owned => meta.at(loc),
			_ => meta.at(loc.view()),
		};
		let key = (krate, path);
		let reexports = uses.remove(&key).unwrap_or_default();
		let extra = extras.remove(&key).unwrap_or_default();
		let decls = children
			.iter()
			.map(|(m, is_dir)| {
				if let Some(target) = aliased.get(&(key.clone(), m.clone())) {
					ensure!(!is_dir, "alias {:?} has nested modules", m.ident);
					return Ok(quote!(pub use $(at.rel(&at.home(*target))) as $(&m.ident);));
				}
				if !m.needs_path() {
					return Ok(quote!(pub mod $(&m.ident);));
				}
				let file = if *is_dir {
					format!("{}/mod.rs", m.stem)
				} else {
					format!("{}.rs", m.stem)
				};
				Ok(quote!(#[path = $(quoted(file))] pub mod $(&m.ident);))
			})
			.collect::<Result<Vec<_>>>()?;
		let stems: Vec<&str> = key.1.iter().map(|m| m.stem.as_str()).collect();
		let file = match (krate, stems.is_empty()) {
			(Some(_), true) if dir.is_empty() => meta.dir_file(krate, "lib.rs"),
			(_, true) => meta.dir_file(krate, &format!("{dir}mod.rs")),
			(_, false) => meta.dir_file(krate, &format!("{dir}{}/mod.rs", stems.join("/"))),
		};
		let tokens: Tokens = quote! {
			$(for d in decls join($['\r']) => $d)
			$(for u in reexports join($['\r']) => $u)
			$(for e in extra join($['\r']) => $e)
		};
		output.insert(file, tokens.to_file_string().expect("formatting should succeed"));
	}
	if let Some(((_, path), _)) = extras.iter().next() {
		let path: Vec<&str> = path.iter().map(|m| m.ident.as_str()).collect();
		bail!("definition output for module {:?} has no place to go", path.join("::"));
	}
	Ok(())
}

pub fn view_generator(
	input: &mir::TypeMap,
	output: &mut GeneratorOutput,
	reg: &Registry,
	namespace: &str,
) -> Result<()> {
	views(&RustMeta::new(input, reg, None)?, input, reg, namespace, output)
}

fn views(
	owned: &RustMeta,
	input: &mir::TypeMap,
	reg: &Registry,
	namespace: &str,
	output: &mut GeneratorOutput,
) -> Result<()> {
	let set = Rc::new(views::analyze(input, reg, &owned.boxed, namespace)?);
	if let Some(id) = set.included.iter().find(|id| owned.move_only.contains(id)) {
		bail!(
			"views can't include {}: it holds a move-only value (such as a resource)",
			input[*id].name.readable_name()
		);
	}
	let meta = owned.with_mode(Mode::View(set.clone()));
	for id in input.0.keys() {
		if let Some(m) = meta.located(*id).1.first().filter(|m| m.ident == "view") {
			bail!("a top-level {:?} module clashes with the generated views", m.ident);
		}
	}
	let ids: Vec<TypeId> = set.included.iter().copied().filter(|id| set.emitted(*id)).collect();
	emit(&meta, input, reg, &ids, "view/", &mut BTreeMap::new(), output)?;
	let mut tops: BTreeMap<Option<usize>, BTreeSet<String>> = BTreeMap::new();
	for id in &ids {
		let (krate, path) = meta.located(*id);
		let names = tops.entry(krate).or_default();
		names.insert(path[0].ident.clone());
		names.extend(path[0].ty.clone());
	}
	for (name, id) in &set.roots {
		let alias = Scope::empty(Case::Snake).get(name);
		let krate = meta.located(*id).0;
		if tops.get(&krate).is_some_and(|t| t.contains(&alias)) {
			continue;
		}
		let root = meta.at(Loc {
			krate,
			path: vec!["view".to_owned()],
		});
		let module = if set.same(*id) {
			root.owned_module(*id)
		} else {
			root.view_module(*id)
		};
		output
			.entry(meta.dir_file(krate, "view/mod.rs"))
			.or_default()
			.push_str(&format!("pub use {} as {alias};\n", root.rel(&module)));
	}
	if meta.layout.is_none() {
		output
			.get_mut(&std::path::PathBuf::from("mod.rs"))
			.ok_or_else(|| anyhow!("views need the owned modules generated first"))?
			.push_str("pub mod view;\n");
	}
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
	extras: &mut BTreeMap<Dir, Vec<Tokens>>,
	output: &mut GeneratorOutput,
) -> Result<()> {
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

	let view = meta.view();
	let lt = &if view.is_some() { quote!(<'i>) } else { quote!() };
	let mut exports = BTreeMap::new();
	let mut reexports = BTreeMap::new();

	for id in ids {
		let ty = &input[*id];
		if let TypeKind::Alias(target) = &ty.kind {
			if ty.params.is_empty() && target.args.is_empty() && !meta.guarded.contains(id) {
				reexports.insert(*id, target.target);
				exports.insert(*id, false);
				continue;
			}
		}
		let meta = &meta.at(meta.home(*id));
		let declared = Cell::new(false);
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
					$(for (name, par) in ty.params.iter() join($['\r']) => pub $(Names::arg(meta, *id, name.as_str())): &'a $(param_ty(*par)),)
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
						type_name: meta.nominal_name(*id),
						declared: &declared,
						move_only: meta.move_only.contains(id),
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
		let def = match meta.nominal_name(*id) {
			Some(name) if declared.get() => quote!($def $['\r'] pub type Ty$lt = $name$lt;),
			_ => def,
		};
		if !take {
			exports.insert(*id, declared.get());
		}
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
			meta.file(*id, dir),
			def.to_file_string().expect("formatting should succeed"),
		);
	}
	mod_files(meta, ids, &exports, &reexports, dir, extras, output)
}
