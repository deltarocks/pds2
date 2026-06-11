use std::collections::BTreeMap;
use std::rc::Rc;

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Field, Type, TypeKind, TypeMap};
use crate::pi;

use super::GeneratorOutput;
use anyhow::{anyhow, Result};
use dprint_core::formatting::PrintItems;
use genco::lang::js::Tokens;
use genco::quote;

#[derive(Clone)]
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
			FieldRefSegment::Arg(name) => {
				assert_eq!(i, 0);
				pi!(pi =>
					"let _0 = &args;" NL
					"let _0: &" {=types.ty(field.to.get())} " = &_0." {?name} ";" NL
				);
			}
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &" {?to} ";" NL)
				} else {
					pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &_0." {?to} ";" NL)
				}
			}
			FieldRefSegment::Virtual(name) => {
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

	let mut out = Tokens::new();
	for (id, ty) in &input.0 {
		let name = &ty.name.readable_name();
		let name_serialize = &format!("{name}Serialize");
		let name_deserialize = &format!("{name}Deserialize");
		let name_size_of = &format!("{name}SizeOf");

		let body = if !ty.kind.is_native() {
			let has_args = !ty.params.is_empty();
			if has_args {
				/*pi!(pi => "pub struct Args<'a> {" INDENT NL);
				for (name, par) in ty.params.iter() {
					pi!(pi => "pub " {?name} ": &'a " {=meta.ty(*par)} "," NL);
				}
				pi!(pi => DEDENT "}" NL);*/
			}
			match &ty.kind {
				TypeKind::Container { fields, .. } => {
					quote!{
						export type $name = {

						};
						export const $name_serialize = (_0: $name, output: Output) => {

						};
					}
				}
				TypeKind::DynamicArray { size_ref, t } => {
					quote!()
				}
				TypeKind::Union {
					tag: _,
					matches,
					default,
				} => {
					quote!()
				}
				TypeKind::FixedSizeArray { size, t } => {
					quote!()
				}
				TypeKind::Native(_) | TypeKind::Void => unreachable!(),
			}
		} else {
			let TypeKind::Native(native) = &ty.kind else {
				unreachable!();
			};
			match native {
				mir::NativeType {
					specializer: Some((specializer, _variant)),
					specialization,
					..
				} => {
					let tyf: Tokens = quote! {Type};
					let readf: Tokens = quote! {throw new Error("not implemented");};
					let writef: Tokens = quote! {throw new Error("not implemented");};
					let sizef: Tokens = quote! {throw new Error("not implemented");};
					quote! {
						export type $name = $tyf;
						export const $name_serialize = (_0: $name, output: Output) => {
							$writef
						};
						export const $name_deserialize = (input: Input): $name => {
							$readf
						};
						export const $name_size_of = (input: Input): number => {
							$sizef
						};
					}
				}
				_ => {
					quote! {
						import {$name, $name_serialize, $name_size_of, $name_deserialize} from "pds-native";
					}
				}
			}
		};
		out.extend(body);
	}
	output.insert("mod.rs".into(), out.to_string().unwrap());
	Ok(())
}
