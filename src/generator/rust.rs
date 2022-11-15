use std::collections::BTreeMap;
use std::rc::Rc;

use crate::ast::{Ident, ReferenceSegment};
use crate::format::SpecializerObj;
use crate::hir::TypeId;
use crate::mir::{self, Field, Type, TypeKind, TypeMap};
use crate::pi;

use super::GeneratorOutput;
use anyhow::{anyhow, Result};
use dprint_core::formatting::PrintItems;
use jrsonnet_evaluator::function::{builtin, CallLocation, FuncVal};
use jrsonnet_evaluator::gc::GcHashMap;
use jrsonnet_evaluator::typed::{Any, Typed};
use jrsonnet_evaluator::{tb, ContextBuilder, IStr, ObjValue, ObjValueBuilder, Val};
use jrsonnet_gcmodule::{Cc, Trace};

#[builtin(fields(
	meta: RustMeta
))]
fn builtin_ty(this: &builtin_ty, id: TypeId) -> jrsonnet_evaluator::Result<String> {
	let ty = this.meta.ty(id);
	Ok(ty)
}
#[builtin(fields(
	meta: RustMeta
))]
fn builtin_ty_size(this: &builtin_ty_size, id: TypeId) -> jrsonnet_evaluator::Result<String> {
	let ty = this.meta.size_of(id);
	Ok(ty)
}
#[builtin(fields(
	meta: RustMeta
))]
fn builtin_ty_read(this: &builtin_ty_read, id: TypeId) -> jrsonnet_evaluator::Result<String> {
	let ty = this.meta.deserialize(id);
	Ok(ty)
}
#[builtin(fields(
	meta: RustMeta
))]
fn builtin_ty_write(this: &builtin_ty_write, id: TypeId) -> jrsonnet_evaluator::Result<String> {
	let ty = this.meta.serialize(id);
	Ok(ty)
}

fn make_jr_pds(meta: &RustMeta) -> ObjValue {
	let mut out = ObjValueBuilder::new();
	out.member("tyTy".into())
		.value(Val::Func(FuncVal::Builtin(Cc::new(tb!(builtin_ty {
			meta: meta.clone()
		})))))
		.expect("no conflict");
	out.member("tySize".into())
		.value(Val::Func(FuncVal::Builtin(Cc::new(tb!(builtin_ty_size {
			meta: meta.clone()
		})))))
		.expect("no conflict");
	out.member("tyRead".into())
		.value(Val::Func(FuncVal::Builtin(Cc::new(tb!(builtin_ty_read {
			meta: meta.clone()
		})))))
		.expect("no conflict");
	out.member("tyWrite".into())
		.value(Val::Func(FuncVal::Builtin(Cc::new(tb!(
			builtin_ty_write { meta: meta.clone() }
		)))))
		.expect("no conflict");
	out.build()
}

#[derive(Clone, Trace)]
struct RustMeta {
	#[trace(skip)]
	raw_names: Rc<BTreeMap<TypeId, String>>,
}
impl RustMeta {
	fn new(source: &TypeMap) -> Self {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		Self {
			raw_names: Rc::new(raw_names),
		}
	}
	fn name(&self, type_id: TypeId) -> String {
		assert!(type_id.is_valid());
		format!("pds_{}", self.raw_names[&type_id])
	}
	fn ty(&self, type_id: TypeId) -> String {
		format!("super::{}::Ty", self.name(type_id))
	}
	fn virt(&self, type_id: TypeId, field: Ident) -> String {
		format!("super::{}::virtual_{:?}", self.name(type_id), field)
	}
	fn args(&self, type_id: TypeId) -> String {
		format!("super::{}::Args", self.name(type_id))
	}
	fn serialize(&self, type_id: TypeId) -> String {
		format!("super::{}::serialize", self.name(type_id))
	}
	fn deserialize(&self, type_id: TypeId) -> String {
		format!("super::{}::deserialize", self.name(type_id))
	}
	fn size_of(&self, type_id: TypeId) -> String {
		format!("super::{}::size_of", self.name(type_id))
	}
}

fn generate_read_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &Field,
	target: Option<&str>,
) -> PrintItems {
	let mut pi = PrintItems::new();
	pi!(pi => "&{" INDENT NL);
	for (i, field) in input.0.iter().enumerate() {
		match &field.segment {
			ReferenceSegment::Arg(name) => {
				assert_eq!(i, 0);
				pi!(pi =>
					"let _0 = &args;" NL
					"let _0: &" {=types.ty(field.to.get())} " = &_0." {?name} ";" NL
				);
			}
			ReferenceSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &" {?to} ";" NL)
				} else {
					pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &_0." {?to} ";" NL)
				}
			}
			ReferenceSegment::Virtual(name) => {
				pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &" {=types.virt(field.from.get(), name.clone())} "(&_0);" NL);
			}
		}
	}
	if let Some(target) = target {
		pi!(pi => "Into::<" {=target} ">::into(")
	}
	pi!(pi => "(*_0)");
	if target.is_some() {
		pi!(pi => ")")
	}
	pi!(pi => NL);
	pi!(pi => DEDENT "}");
	pi
}

fn generate_args_passing(types: &RustMeta, input: &Type) -> PrintItems {
	let mut pi = PrintItems::new();
	pi!(pi => {=types.args(input.target)} " {" INDENT NL);
	for (name, field) in input.args.iter() {
		pi!(pi => {?name} ": " {<generate_read_reference(types, true, field, None)} "," NL);
	}
	pi!(pi => DEDENT "}");
	pi
}

pub fn generator(
	input: &mir::TypeMap,
	output: &mut GeneratorOutput,
	spec: &SpecializerObj,
) -> Result<()> {
	let meta = RustMeta::new(input);
	let pds = make_jr_pds(&meta);

	let mut pi = PrintItems::new();
	pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports, unused_parens, clippy::needless_borrow, clippy::just_underscores_and_digits, clippy::identity_op)]" NL);
	pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
	for (id, ty) in &input.0 {
		let name = ty.name.readable_name();
		if !ty.kind.is_native() {
			pi!(pi => "/// " {?ty.name} NL);
			pi!(pi => "pub mod " {=meta.name(*id)} " {" INDENT NL);
			pi!(pi => "use std::convert::TryInto;" NL);
			pi!(pi => "use super::{Input, Output, Result};" NL);

			let has_args = !ty.params.is_empty();
			if has_args {
				pi!(pi => "pub struct Args<'a> {" INDENT NL);
				for (name, par) in ty.params.iter() {
					pi!(pi => "pub " {?name} ": &'a " {=meta.ty(*par)} "," NL);
				}
				pi!(pi => DEDENT "}" NL);
			}
			match &ty.kind {
				TypeKind::Container { fields, .. } => {
					pi!(pi =>
						"pub struct Ty {" INDENT NL
							{forin field in fields.iter() =>
								{ifelse field.load_from.is_none() =>
									{"pub " {?field.name} ": " {=meta.ty(field.ty.target)} "," NL}
									{"// " {?field.name} " - virtual field" NL}
								}
							}
						DEDENT "}" NL

						"pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
							{forin field in fields.iter() =>
								{ifelse field.load_from.is_none() =>
									{{=meta.serialize(field.ty.target)}"(&_0." {?field.name} ", output)?;" NL}
									{{=meta.serialize(field.ty.target)}"(" INDENT NL
										{<generate_read_reference(&meta, false, field.load_from.as_ref().unwrap(), None)} "," NL
										"output," NL
									DEDENT ")?;" NL}
								}
							}
							"Ok(())" NL
						DEDENT "}" NL

						"pub fn deserialize(input: &mut Input"
						{if has_args => " , args: Args<'_>"}
						") -> Result<Ty> {" INDENT NL
							{forin field in fields.iter() =>
								"let " {?field.name} " = " {=meta.deserialize(field.ty.target)} "(input"
								{if !input[field.ty.target].params.is_empty() => ", " {<generate_args_passing(&meta, &field.ty)}}
								")?;" NL
							}

							"Ok(Ty {" INDENT NL
								{forin field in fields.iter().filter(|f| f.load_from.is_none()) =>
									{?field.name} "," NL
								}
							DEDENT "})" NL
						DEDENT "}" NL

						"pub fn size_of(_0: &Ty) -> usize {" INDENT NL
							"0"
							{forin i in fields.iter().enumerate() =>
								" + "
								{=meta.size_of(i.1.ty.target)} "("
								{ifelse i.1.load_from.is_none() =>
									{"&_0." {?i.1.name}}
									{{<generate_read_reference(&meta, false, i.1.load_from.as_ref().unwrap(), None)}}
								}
								")"
							}
							NL
						DEDENT "}" NL
					);
				}
				TypeKind::DynamicArray { size_ref, t } => {
					pi!(pi =>
						"pub type Ty = Vec<" {=meta.ty(t.target)} ">;" NL

						"pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
							"for item in _0 {" INDENT NL
								{=meta.serialize(t.target)} "(&item, output)?;" NL
							DEDENT "}" NL
							"Ok(())" NL
						DEDENT "}" NL

						"pub fn deserialize(input: &mut Input"
						{if has_args => " , args: Args<'_>"}
						") -> Result<Ty> {" INDENT NL
							"let mut out = Vec::with_capacity(*" {<generate_read_reference(&meta, false, size_ref, Some("usize"))} ");" NL
							"for _ in 0..(*" {<generate_read_reference(&meta, false, size_ref, Some("usize"))} ") {" INDENT NL
								"out.push(" {=meta.deserialize(t.target)} "(input)?);" NL
							DEDENT "}" NL
							"Ok(out)" NL
						DEDENT "}" NL

						"pub fn size_of(_0: &Ty) -> usize {" INDENT NL
							"_0" NL INDENT
								".iter()" NL
								".map(|v| " {=meta.size_of(t.target)} "(&v))" NL
								".sum()" NL DEDENT
						DEDENT "}" NL

						"pub fn virtual_size(_0: &Ty) -> super::pds_usize::Ty {" INDENT NL
							"_0.len() as super::pds_usize::Ty" NL
						DEDENT "}" NL
					);
				}
				TypeKind::Union {
					tag: _,
					matches,
					default,
				} => {
					pi!(pi =>
						"pub enum Ty {" INDENT NL
							{forin m in matches =>
								{?m.1} "(" {=meta.ty(m.2.target)} ")," NL
							}
							{if default.is_some() =>
								{?default.as_ref().unwrap().0} "(" {=meta.ty(default.as_ref().unwrap().1.target)} ")," NL
							}
						DEDENT "}" NL
						"pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
							"todo!()" NL
						DEDENT "}" NL
						"pub fn deserialize(input: &mut Input"
						{if has_args => " , args: Args<'_>"}
						") -> Result<Ty> {" INDENT NL
							"todo!()" NL
						DEDENT "}" NL

						"pub fn size_of(_0: &Ty) -> usize {" INDENT NL
							"match &_0 {" INDENT NL
								{forin m in matches =>
									"Self::" {?m.1} "(_0) => " {=meta.size_of(m.2.target)} "(_0)," NL
								}
								{if default.is_some() =>
									"Self::" {?default.as_ref().unwrap().0} "(_0) => " {=meta.size_of(default.as_ref().unwrap().1.target)} "(_0)," NL
								}
							DEDENT "}" NL
						DEDENT "}" NL
						"pub fn virtual_tag(_0: &Ty) -> super::pds_usize::Ty {" INDENT NL
							"match &_0 {" INDENT NL
								{forin m in matches =>
									"Self::" {?m.1} "(_0) => " {?m.0} "," NL
								}
								{if default.is_some() =>
									"Self::" {?default.as_ref().unwrap().0} "(_0) => " {=meta.size_of(default.as_ref().unwrap().1.target)} "(_0)," NL
								}
							DEDENT "}" NL
						DEDENT "}" NL
					);
				}
				TypeKind::FixedSizeArray { size, t } => {
					pi!(pi =>
						"type " {=name} " = ["{=input[t.target].name.readable_name()}"; "{=size}"];"

					);
				}
				TypeKind::Native(_) | TypeKind::Void => unreachable!(),
			}
			pi!(pi => DEDENT "}" NL);
		} else {
			let TypeKind::Native(native) = &ty.kind else {
				unreachable!();
			};
			match native {
				mir::NativeTypeKind::Opaque {
					specializer: Some(specializer),
					specialization,
					virtual_fields: _,
				} => {
					let specializer = spec
						.specializer
						.get(specializer.as_str().into())
						.map_err(|e| anyhow!("{e}"))?
						.ok_or_else(|| anyhow!("missing specializer for {specializer:?}"))?;
					let func = specializer
						.as_func()
						.ok_or_else(|| anyhow!("specializer should be function"))?;

					let mut args: GcHashMap<IStr, Any> = GcHashMap::new();
					args.insert("pds".into(), Any(Val::Obj(pds.clone())));
					for (name, ty) in specialization {
						args.insert(
							name.as_str().into(),
							Any(TypeId::into_untyped(ty.target)
								.expect("into conversion always succeeds")),
						);
					}
					let out = func
						.evaluate(
							ContextBuilder::dangerous_empty_state().build(),
							CallLocation::native(),
							&args,
							true,
						)
						.map_err(|e| anyhow!("{e}"))?;

					#[derive(Typed)]
					struct SpecializerOutput {
						ty: Any,
						read: Any,
						write: Any,
						size: Any,
					}

					let out = SpecializerOutput::from_untyped(out).map_err(|e| anyhow!("{e}"))?;
					let tyf =
						crate::format::convert_format(&out.ty.0).map_err(|e| anyhow!("{e}"))?;
					let readf =
						crate::format::convert_format(&out.read.0).map_err(|e| anyhow!("{e}"))?;
					let writef =
						crate::format::convert_format(&out.write.0).map_err(|e| anyhow!("{e}"))?;
					let sizef =
						crate::format::convert_format(&out.size.0).map_err(|e| anyhow!("{e}"))?;

					pi!(pi => "pub mod pds_" {=ty.name.readable_name()} " {" INDENT NL);
					pi!(pi => "use std::convert::TryInto;" NL);
					pi!(pi => "use super::{Input, Output, Result};" NL);
					pi!(pi => "pub type Ty = " {<tyf} ";" NL);
					pi!(pi => "pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL {<writef} DEDENT NL "}" NL);
					pi!(pi => "pub fn deserialize(input: &mut Input) -> Result<Ty> {" INDENT NL {<readf} DEDENT NL "}" NL);
					pi!(pi => "pub fn size_of(_0: &Ty) -> usize {" INDENT NL {<sizef} DEDENT NL "}" NL);
					pi!(pi => DEDENT "}" NL)
				}
				_ => {
					pi!(pi =>
						"use crate::pds::pds_" {=ty.name.readable_name()}  ";" NL
					);
				}
			}
		}
	}
	output.insert("mod.rs".into(), pi);
	Ok(())
}
