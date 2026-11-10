use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::encode::{Encoder, Encoding, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use crate::union::{self, TagView, UnionView};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor,
};
use pds2_core::ast::{FieldRefSegment, Ident};
use pds2_core::generator::common::local;
use pds2_core::generator::rust::rname;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, TypeKind, TypeMap};

pub struct Container;

pub struct ContainerField<'a> {
	pub name: &'a Ident,
	pub ty: &'a mir::Type,
	pub load_from: Option<&'a Expr>,
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
				"virtual_field" => Some(item.arg("value")?),
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
						!matches!(f.args.get(&Ident::new("value")), Some(hir::Expr::Const(_)) | None),
						"virtual_field {name:?} needs a `value` computed from the container"
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

impl ValueModel for Container {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let Expr::Record(given) = value else {
			bail!("{} is written as a record `(field: value, ...)`", cx.name(id));
		};
		let fields = view(native).ok_or_else(|| anyhow!("malformed container"))?;
		let stored: Vec<&ContainerField> = fields.iter().filter(|f| f.load_from.is_none()).collect();
		for (n, _) in given {
			ensure!(
				stored.iter().any(|f| f.name == n),
				"{} has no stored field {n:?}",
				cx.name(id)
			);
		}
		let mut out = Vec::new();
		for f in stored {
			let e = match given.iter().find(|(n, _)| n == f.name) {
				Some((_, e)) => cx
					.coerce(e.clone(), f.ty.target)
					.with_context(|| format!("field {:?}", f.name))?,
				None if cx.types.is_void(f.ty.target) => cx.build(f.ty.target, &Expr::Record(vec![]))?,
				None => bail!("{} needs field {:?}", cx.name(id), f.name),
			};
			out.push((f.name.clone(), e));
		}
		Ok(Construction::Record(out))
	}
}

impl Encoder for Container {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let fields = view(v.native).ok_or_else(|| anyhow!("malformed container"))?;
		let scope = v.scope();
		for f in &fields {
			self.field(cx, v, f.name)
				.and_then(|value| cx.encode_use(f.ty, &value, &scope, out))
				.with_context(|| format!("field {:?}", f.name))?;
		}
		Ok(())
	}
	fn field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		let fields = view(v.native).ok_or_else(|| anyhow!("malformed container"))?;
		let f = fields
			.iter()
			.find(|f| f.name == name)
			.ok_or_else(|| anyhow!("{} has no field {name:?}", cx.name(v.id)))?;
		match f.load_from {
			Some(from) => cx.typed(cx.eval(from, &v.scope())?, f.ty.target),
			None => record_of(v.c)?
				.iter()
				.find(|(n, _)| n == name)
				.map(|(_, e)| e.clone())
				.ok_or_else(|| anyhow!("{} value lacks field {name:?}", cx.name(v.id))),
		}
	}
}

fn record_of(c: &Construction) -> Result<&[(Ident, Expr)]> {
	match c {
		Construction::Record(fields) => Ok(fields),
		c => bail!("container is constructed from a record, got {c:?}"),
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
) -> Option<&'a Expr> {
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
	let mut readable = true;
	val.walk(&mut |e| {
		let Expr::Field(f) = e else {
			return;
		};
		match f.0.first().map(|i| &i.segment) {
			Some(FieldRefSegment::Arg(_)) => readable = false,
			Some(FieldRefSegment::Down { to, .. })
				if siblings
					.iter()
					.any(|s| s.name == to && s.load_from.is_some()) =>
			{
				readable = false
			}
			_ => {}
		}
	});
	readable.then_some(val)
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
					$((ctx.read)(val, false)),
				);
			}
		};
		let value_of = |field: &ContainerField| -> rust::Tokens {
			match field.load_from {
				None => quote!(&_0.$(rname(field.name))),
				Some(from) => (ctx.read)(from, false),
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
						&$(local(field.name)) == $((ctx.read)(field.load_from.unwrap(), true)),
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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let mut parts = Vec::new();
		for (n, e) in record_of(c)? {
			let value = cx.names.slot_new(id, n, (cx.lower)(e)?);
			parts.push(quote!($(rname(n)): $value));
		}
		Ok(quote!($(cx.names.ty(id)) { $(for p in parts join(, ) => $p) }))
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
				$(names.item(field.ty.target, "assert_tag"))(_0.$(field.name.as_str()), $((ctx.read)(val, false)));
			}
		};
		let value_of = |field: &ContainerField| -> js::Tokens {
			match field.load_from {
				None => quote!(_0.$(field.name.as_str())),
				Some(from) => (ctx.read)(from, false),
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
					if (!$pequal($(local(f.name)), $((ctx.read)(f.load_from.unwrap(), true)))) {
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
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<js::Tokens> {
		let mut parts = Vec::new();
		for (n, e) in record_of(c)? {
			parts.push(quote!($(n.as_str()): $((cx.lower)(e)?)));
		}
		Ok(quote!({ $(for p in parts join(, ) => $p) }))
	}
}
