use genco::lang::rust::import;
use genco::prelude::*;
use genco::quote;
use rust::{Import, Tokens};
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::FieldOrConst;
use crate::mir::{self, Field, Type, TypeKind, TypeMap};

use super::GeneratorOutput;
use crate::specializer::{NativeCodegenCtx, Registry, RustNames};
use anyhow::{Context as _, Result};

#[derive(Clone)]
struct RustMeta {
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
	fn ty(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "Ty")
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

impl RustNames for RustMeta {
	fn ty(&self, id: TypeId) -> Import {
		RustMeta::ty(self, id)
	}
	fn serialize(&self, id: TypeId) -> Import {
		RustMeta::serialize(self, id)
	}
	fn deserialize(&self, id: TypeId) -> Import {
		RustMeta::deserialize(self, id)
	}
	fn size_of(&self, id: TypeId) -> Import {
		RustMeta::size_of(self, id)
	}
}

fn generate_read_or_const_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &FieldOrConst,
	target: Option<&Tokens>,
) -> Tokens {
	let input = match input {
		FieldOrConst::Field(field) => field,
		FieldOrConst::Const(mir::Value::Signed(n)) => {
			return quote!(&($(n.to_string())).into())
		}
		FieldOrConst::Const(_) => return quote!(Into::<$target>::into(todo!("const"))),
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
	for (i, field) in input.0.iter().enumerate() {
		indexing.push(match &field.segment {
			FieldRefSegment::Arg(name) => {
				assert_eq!(i, 0);
				quote! {
					let _0 = &args;
					let _0: &$(types.ty(field.to.get())) = &_0.$name;
				}
			}
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					quote!(let _0: &$(types.ty(field.to.get())) = &$to;)
				} else {
					quote!(let _0: &$(types.ty(field.to.get())) = &_0.$to;)
				}
			}
			FieldRefSegment::Virtual(name) => {
				quote!(let _0: &$(types.ty(field.to.get())) = &$(types.virt(field.from.get(), name.clone()))(&_0);)
			}
		})
	}
	if let Some(target) = target {
		indexing.push(quote!(Into::<$target>::into(*_0)));
	} else {
		indexing.push(quote!(*_0))
	}
	quote!(&{$(for i in indexing => $i)})
}

fn generate_args_passing(types: &RustMeta, input: &Type) -> Tokens {
	let mut args = vec![];
	for (name, field) in input.args.iter() {
		args.push(quote!($name: $(generate_read_or_const_reference(types, true, field, None))))
	}
	quote! {
		$(types.args(input.target)) {
			$(for arg in args join(,) => $arg)
		}
	}
}

pub fn generator(
	input: &mir::TypeMap,
	output: &mut GeneratorOutput,
	reg: &Registry,
) -> Result<()> {
	let meta = RustMeta::new(input);

	let mut out_module = vec![];

	// pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports, unused_parens, clippy::needless_borrow, clippy::just_underscores_and_digits, clippy::identity_op)]" NL);
	// pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
	for (id, ty) in &input.0 {
		let pinput = &import("crate::pds", "Input");
		let poutput = &import("crate::pds", "Output");
		let presult = &import("crate::pds", "Result");
		let try_into = &import("std::convert", "TryInto");
		let stdvec = &import("std::vec", "Vec");
		let name = ty.name.readable_name();
		let def = if !ty.kind.is_native() {
			let has_args = !ty.params.is_empty();
			let mod_contents = match &ty.kind {
				TypeKind::Container { fields, .. } => {
					quote! {
						$(if has_args => pub struct Args<'a> {
							$(for (name, par) in ty.params.iter() join(,) => pub $name: &'a $(meta.ty(*par)))
						})
						pub struct Ty {
							$(for field in fields.iter() join($['\r']) =>
								$(if field.load_from.is_none() {
									pub $(&field.name): $(meta.ty(field.ty.target)),
								} else {
									$("// ") $(&field.name) $("- virtual field")
								})
							)
						}
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(for field in fields.iter() join($['\r']) =>
								$(meta.serialize(field.ty.target))(
									$(if field.load_from.is_none() {
										&_0.$(&field.name)
									} else {
										$(generate_read_reference(&meta, false, field.load_from.as_ref().unwrap(), None))
									}),
									output,
								)?;
							)
							Ok(())
						}
						pub fn deserialize(input: &mut $pinput, $(if has_args => args: Args<'_>>)) -> $presult<Ty> {
							$(for field in fields.iter() join($['\r']) =>
								let $(&field.name) = $(meta.deserialize(field.ty.target))(
									input,
									$(if !input[field.ty.target].params.is_empty() => $(generate_args_passing(&meta, &field.ty)),)
								);
							)
							$(for field in fields.iter().filter(|f| f.load_from.is_some()) join($['\r']) =>
								debug_assert!(
									&$(&field.name) == $(generate_read_reference(&meta, true, field.load_from.as_ref().unwrap(), None)),
									$(quoted(format!("virtual field {} does not match its value source", field.name.as_str())))
								);
							)
							Ok(Ty {
								$(for field in fields.iter().filter(|f| f.load_from.is_none()) join(,) =>
									$(&field.name)
								)
							})
						}
						pub fn size_of(_0: &Ty) -> usize {
							0
							$(for field in fields.iter() join($['\r']) =>
								+ $(meta.size_of(field.ty.target))(
									$(if field.load_from.is_none() {
										&_0.$(&field.name)
									} else {
										{$(generate_read_reference(&meta, false, dbg!(field.load_from.as_ref()).unwrap(), None))}
									})
								)
							)
						}
					}
				}
				TypeKind::Union {
					tag: _,
					tag_type,
					matches,
					default,
				} => {
					quote! {
						pub enum Ty {
							$(for (_, variant, value) in matches =>
								$variant($(meta.ty(value.target))),
							)
							$(if default.is_some() =>
								$(&default.as_ref().unwrap().0)($(meta.ty(default.as_ref().unwrap().1.target)))
							)
						}
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							todo!()
						}
						pub fn deserialize(input: &mut $pinput, $(if has_args => args: Args<'_>)) -> $presult<Ty> {
							todo!()
						}
						pub fn size_of(_0: &Ty) -> usize {
							match &_0 {
								$(for (_, variant, value) in matches =>
									Self::$variant(_0) => $(meta.size_of(value.target))(_0),
								)
								$(if default.is_some() =>
									Self::$(&default.as_ref().unwrap().0)(_0) => $(meta.size_of(default.as_ref().unwrap().1.target))(_0),
								)
							}
						}
						pub fn virtual_tag(_0: &Ty) -> super::pds_usize::Ty {
							match &_0 {
								$(for (tag, variant, _) in matches =>
									Self::$variant(_0) => tag,
								)
								$(if default.is_some() =>
									Self::$(&default.as_ref().unwrap().0)(_0) => todo!(),
								)
							}
						}
					}
				}
				TypeKind::Alias(target) => {
					quote! {
						pub type Ty = $(meta.ty(target.target));
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(meta.serialize(target.target))(_0, output)
						}
						pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
							$(meta.deserialize(target.target))(
								input,
								$(if !input[target.target].params.is_empty() => $(generate_args_passing(&meta, target)),)
							)
						}
						pub fn size_of(_0: &Ty) -> usize {
							$(meta.size_of(target.target))(_0)
						}
					}
				}
				TypeKind::Native(_) | TypeKind::Void => {
					quote! {$("// TODO: Native")}
				}
				// pi!(pi => "/// " {?ty.name} NL);
				// pi!(pi => "pub mod " {=meta.name(*id)} " {" INDENT NL);
				// pi!(pi => "use std::convert::TryInto;" NL);
				// pi!(pi => "use super::{Input, Output, Result};" NL);
			};
			quote! {
				pub mod $(meta.name(*id)) {
					$mod_contents
				}
			}
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
					let Some(codegen) = reg.codegen::<Tokens>(specializer.as_str()) else {
						anyhow::bail!("no rust codegen for specializer {specializer:?}");
					};
					let contents = codegen
						.generate(&NativeCodegenCtx {
							variant,
							native,
							params: &ty.params,
							rust: &meta,
							resolve: &resolve,
						})
						.with_context(|| format!("codegen for {:?}", ty.name))?;
					quote! {
						pub mod $(meta.name(*id)) {
							$contents
						}
					}
				}
				_ => {
					let import = import(
						"crate::pds_native",
						format!("pds_{}", ty.name.readable_name()),
					);
					quote!(pub use $import::*;)
				}
			}
		};
		output.insert(
			format!("{}.rs", meta.name_str(*id)).into(),
			def.to_file_string().expect("formatting should succeed"),
		);
		out_module.push(quote! {
			mod $(meta.name(*id));
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
