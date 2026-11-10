use anyhow::{anyhow, ensure, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::union::{self, TagView, UnionView};
use super::{AstHir, BoxCost, Embed, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor};
use crate::ast::{FieldRefSegment, Ident};
use crate::generator::common::local;
use crate::generator::rust::rname;
use crate::hir::{self, FieldRefOrConst, HirSpecializerCtx, TypeId};
use crate::mir::{self, FieldOrConst, TypeKind, TypeMap};

pub struct Container;

pub struct ContainerField<'a> {
	pub name: &'a Ident,
	pub ty: &'a mir::Type,
	pub load_from: Option<&'a mir::Field>,
}

pub fn view(native: &mir::NativeType) -> Option<Vec<ContainerField<'_>>> {
	let (spec, _) = native.specializer.as_ref()?;
	if spec.as_str() != "container" {
		return None;
	}
	native
		.body
		.iter()
		.zip(native.fields.iter())
		.map(|(item, (name, ty))| {
			let load_from = match item.kind.as_str() {
				"field" => None,
				"virtual_field" => Some(item.field("value")?),
				_ => return None,
			};
			Some(ContainerField {
				name,
				ty,
				load_from,
			})
		})
		.collect()
}

pub fn is_container(types: &TypeMap, id: TypeId) -> bool {
	matches!(&types[id].kind, TypeKind::Native(n) if view(n).is_some())
}

impl AstHir for Container {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ctx.body
			.iter()
			.map(|f| {
				let name = f.expect_name()?;
				if f.kind.as_str() == "virtual_field" {
					ensure!(
						matches!(f.args.get(&Ident::new("value")), Some(FieldRefOrConst::FieldRef(_))),
						"virtual_field {name:?} needs a reference `value`"
					);
				}
				let ty =
					f.ty.clone()
						.ok_or_else(|| anyhow!("field {name:?} has no type"))?;
				Ok((name.clone(), ty))
			})
			.collect()
	}
}

impl NativeLayout for Container {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		for f in view(native).into_iter().flatten() {
			if f.load_from.is_none() {
				v.slot(f.name, f.ty.target, Embed::Boxable(BoxCost::High));
			}
		}
	}
}

// For a container field that is an external-tag union, a debug-build check that
// the union variant agrees with the tag field it is keyed on.
fn assert_tag_arg<'a>(
	types: &'a TypeMap,
	siblings: &[ContainerField],
	field: &'a ContainerField,
) -> Option<&'a FieldOrConst> {
	if field.load_from.is_some() {
		return None;
	}
	let Some(UnionView {
		tag: TagView::External(tagref),
		arms,
		default,
		..
	}) = union::view_of(types, field.ty.target)
	else {
		return None;
	};
	if arms.is_empty() && default.is_none() {
		return None;
	}
	let Some(FieldRefSegment::Arg(tag_param)) = tagref.0.first().map(|i| &i.segment) else {
		return None;
	};
	let (_, val) = field.ty.args.iter().find(|(n, _)| n == tag_param)?;
	// The tag must be readable from `_0` alone. A tag that is itself a
	// closure-converted param (threaded only at deserialize time, e.g. a
	// bitflags field passed down through an array) is not available in
	// serialize, so skip the check there.
	if let FieldOrConst::Field(f) = val {
		match f.0.first().map(|i| &i.segment) {
			Some(FieldRefSegment::Arg(_)) => return None,
			Some(FieldRefSegment::Down { to, .. })
				if siblings
					.iter()
					.any(|s| s.name == to && s.load_from.is_some()) =>
			{
				return None
			}
			_ => {}
		}
	}
	Some(val)
}

impl NativeCodegen<Rust> for Container {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let fields = view(ctx.native).ok_or_else(|| anyhow!("malformed container"))?;
		let names = ctx.names;
		let pinput = &rust::import("crate::pds", "Input");
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let assert_tag_call = |field: &ContainerField| -> rust::Tokens {
			let Some(val) = assert_tag_arg(ctx.types, &fields, field) else {
				return quote!();
			};
			quote! {
				$(names.item(field.ty.target, "debug_assert_tag"))(
					&_0.$(rname(field.name)),
					$((ctx.read_arg)(val, false)),
				);
			}
		};
		let value_of = |field: &ContainerField| -> rust::Tokens {
			match field.load_from {
				None => quote!(&_0.$(rname(field.name))),
				Some(from) => (ctx.read_ref)(from, false),
			}
		};
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub struct Ty {
				$(for field in fields.iter() join($['\r']) =>
					$(if field.load_from.is_none() {
						pub $(rname(field.name)): $(ctx.slot_ty(field.name, field.ty.target)),
					} else {
						$("// ") $(field.name) $("- virtual field")
					})
				)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(for field in fields.iter() join($['\r']) =>
					$(assert_tag_call(field))
					$(names.serialize(field.ty.target))(
						$(value_of(field)),
						output,
					)?;
				)
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				$(for field in fields.iter() join($['\r']) =>
					let $(local(field.name)) = $(names.deserialize(field.ty.target))(
						input,
						$((ctx.pass_args)(field.ty))
					)?;
				)
				$(for field in fields.iter().filter(|f| f.load_from.is_some()) join($['\r']) =>
					debug_assert!(
						&$(local(field.name)) == $((ctx.read_ref)(field.load_from.unwrap(), true)),
						$(quoted(format!("virtual field {} does not match its value source", field.name.as_str())))
					);
				)
				Ok(Ty {
					$(for field in fields.iter().filter(|f| f.load_from.is_none()) join(,) =>
						$(rname(field.name)): $(ctx.slot_new(field.name, quote!($(local(field.name)))))
					)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				0
				$(for field in fields.iter() join($['\r']) =>
					+ $(names.size_of(field.ty.target))(
						$(value_of(field))
					)
				)
			}
		})
	}
}

impl NativeCodegen<JavaScript> for Container {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let fields = view(ctx.native).ok_or_else(|| anyhow!("malformed container"))?;
		let names = ctx.names;
		let pinput = &js::import("../pds.ts", "Input");
		let poutput = &js::import("../pds.ts", "Output");
		let perror = &js::import("../pds.ts", "PdsError");
		let pequal = &js::import("../pds.ts", "equal");
		let has_args = !ctx.params.is_empty();
		let args_of = |t: &mir::Type| -> js::Tokens {
			let a = (ctx.pass_args)(t);
			if a.is_empty() {
				quote!()
			} else {
				quote!(, $a)
			}
		};
		let assert_tag_call = |field: &ContainerField| -> js::Tokens {
			let Some(val) = assert_tag_arg(ctx.types, &fields, field) else {
				return quote!();
			};
			quote! {
				$(names.item(field.ty.target, "assert_tag"))(_0.$(field.name.as_str()), $((ctx.read_arg)(val, false)));
			}
		};
		let value_of = |field: &ContainerField| -> js::Tokens {
			match field.load_from {
				None => quote!(_0.$(field.name.as_str())),
				Some(from) => (ctx.read_ref)(from, false),
			}
		};
		let stored: Vec<&ContainerField> = fields.iter().filter(|f| f.load_from.is_none()).collect();
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = {
				$(for f in &stored join($['\r']) => $(f.name.as_str()): $(names.ty(f.ty.target));)
			};
			export function serialize(_0: Ty, output: $poutput): void {
				$(for f in fields.iter() join($['\r']) =>
					$(assert_tag_call(f))
					$(names.serialize(f.ty.target))($(value_of(f)), output);
				)
			}
			export function deserialize(input: $pinput$(if has_args => , $((ctx.args_param)()))): Ty {
				$(for f in fields.iter() join($['\r']) =>
					const $(local(f.name)) = $(names.deserialize(f.ty.target))(input$(args_of(f.ty)));
				)
				$(for f in fields.iter().filter(|f| f.load_from.is_some()) join($['\r']) =>
					if (!$pequal($(local(f.name)), $((ctx.read_ref)(f.load_from.unwrap(), true)))) {
						throw new $perror("InvalidValue", $(quoted(format!("virtual field {} does not match its value source", f.name.as_str()))));
					}
				)
				return {
					$(for f in &stored join($['\r']) => $(f.name.as_str()): $(local(f.name)),)
				};
			}
			export function size_of(_0: Ty): number {
				return 0$(for f in fields.iter() => $[' ']+ $(names.size_of(f.ty.target))($(value_of(f))));
			}
		})
	}
}
