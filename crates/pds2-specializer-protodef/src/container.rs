use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::decode::Input;
use pds2_core::specializer::encode::{const_eq, Encoder, Encoding, Scope, Target, Val};
use pds2_core::specializer::expr::{describe, Elaborator, ValueModel};
use crate::natives::sized_count;
use crate::union::{self, TagView, UnionView};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx, NativeLayout,
	SlotVisitor, ViewPolicy,
};
use pds2_core::ast::{FieldRefSegment, Ident};
use pds2_core::generator::common::local;
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
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		view(native)
			.into_iter()
			.flatten()
			.map(|f| cx.min_size(f.ty.target))
			.fold(0, usize::saturating_add)
	}
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
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let fields = view(t.native).ok_or_else(|| anyhow!("malformed container"))?;
		let mut read: Vec<(Ident, Expr)> = Vec::new();
		for f in &fields {
			let scope = Scope {
				fields: &read,
				..t.scope()
			};
			let value = cx
				.decode_use(f.ty, &scope, input)
				.with_context(|| format!("field {:?}", f.name))?;
			read.push((f.name.clone(), value));
		}
		let scope = Scope {
			fields: &read,
			..t.scope()
		};
		for f in &fields {
			let Some(from) = f.load_from else {
				continue;
			};
			let expected = cx
				.typed(cx.eval(from, &scope)?, f.ty.target)
				.with_context(|| format!("virtual field {:?}", f.name))?;
			let (_, got) = read
				.iter()
				.find(|(n, _)| n == f.name)
				.expect("every field was read");
			ensure!(
				const_eq(got, &expected),
				"virtual field {:?} reads {}, but its value source gives {}",
				f.name,
				describe(got),
				describe(&expected)
			);
		}
		let stored = read
			.iter()
			.filter(|(n, _)| fields.iter().any(|f| f.name == n && f.load_from.is_none()))
			.cloned()
			.collect();
		Ok(Construction::Record(stored))
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
	fn members(&self, native: &mir::NativeType) -> Members {
		Members {
			fields: view(native)
				.into_iter()
				.flatten()
				.filter(|f| f.load_from.is_none())
				.map(|f| f.name.clone())
				.collect(),
			..Default::default()
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
	readable_in_serialize(siblings, val).then_some(val)
}

fn readable_in_serialize(siblings: &[ContainerField], val: &Expr) -> bool {
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
	readable
}

fn count_check_arg<'a>(
	types: &'a TypeMap,
	siblings: &[ContainerField],
	field: &'a ContainerField,
) -> Option<&'a Expr> {
	if field.load_from.is_some() {
		return None;
	}
	sized_count(types, field.ty).filter(|val| readable_in_serialize(siblings, val))
}

impl NativeCodegen<Rust> for Container {
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let fields = view(ctx.native).ok_or_else(|| anyhow!("malformed container"))?;
		let parts = fields.iter().filter(|f| f.load_from.is_none()).map(|f| {
			let name = ctx.field(f.name.as_str());
			let v = ctx.slot_new(
				f.name,
				quote!($(ctx.names.into_view_fn(f.ty.target))(&mut _0.$(&name))),
			);
			quote!($name: $v)
		});
		Ok(Some(ctx.into_view_fn(quote! {
			$(ctx.into_path()) {
				$(for p in parts join(,) => $p)
			}
		})))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let fields = view(ctx.native).ok_or_else(|| anyhow!("malformed container"))?;
		let min = Encoding::new(ctx.elab).min_size(ctx.id);
		let expect = (min > 0).then(|| quote!(input.expect($(min.to_string()))?;));
		let names = ctx.names;
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let lt = &ctx.lt();
		let this = &ctx.this();
		let stored: Vec<&ContainerField> = fields.iter().filter(|f| f.load_from.is_none()).collect();
		let fname = |f: &ContainerField| ctx.field(f.name.as_str());
		let view_fields = stored.iter().map(|f| {
			let v = ctx.slot_new(f.name, quote!($(names.view_fn(f.ty.target))(&_0.$(fname(f)))));
			quote!($(fname(f)): $v)
		});
		let assert_tag_call = |field: &ContainerField| -> rust::Tokens {
			let Some(val) = assert_tag_arg(ctx.types, &fields, field) else {
				return quote!();
			};
			quote! {
				$(names.item(field.ty.target, "debug_assert_tag"))(
					&_0.$(ctx.field(field.name.as_str())),
					$((ctx.read)(val, false)),
				);
			}
		};
		let check_count_call = |field: &ContainerField| -> rust::Tokens {
			let Some(val) = count_check_arg(ctx.types, &fields, field) else {
				return quote!();
			};
			quote! {
				$(names.item(field.ty.target, "check_count"))(
					&_0.$(ctx.field(field.name.as_str())),
					$((ctx.read)(val, false)),
				)?;
			}
		};
		let value_of = |field: &ContainerField| -> rust::Tokens {
			match field.load_from {
				None => quote!(&_0.$(ctx.field(field.name.as_str()))),
				Some(from) => (ctx.read)(from, false),
			}
		};
		Ok(quote! {
			$((ctx.args_struct)())
			$(ctx.derive())
			pub struct $(ctx.nominal()) {
				$(for field in fields.iter() join($['\r']) =>
					$(if field.load_from.is_none() {
						pub $(fname(field)): $(ctx.slot_ty(field.name, field.ty.target)),
					} else {
						$("// ") $(field.name) $("- virtual field")
					})
				)
			}
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
				$(for field in fields.iter() join($['\r']) =>
					$(assert_tag_call(field))
					$(check_count_call(field))
					$(names.serialize(field.ty.target))(
						$(value_of(field)),
						output,
					)?;
				)
				Ok(())
			}
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
				$expect
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
						$(fname(field)): $(ctx.slot_new(field.name, quote!($(local(field.name)))))
					)
				})
			}
			pub fn size_of$lt(_0: &$this) -> usize {
				0
				$(for field in fields.iter() join($['\r']) =>
					+ $(names.size_of(field.ty.target))(
						$(value_of(field))
					)
				)
			}
			$(if ctx.view =>
				pub fn view<'i>(_0: &'i $(ctx.owned_this())) -> Ty<'i> {
					Ty {
						$(for v in view_fields join(,) => $v)
					}
				}
			)
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
			parts.push(quote!($(cx.names.field(id, n.as_str())): $value));
		}
		Ok(quote!($(cx.names.ty(id)) { $(for p in parts join(, ) => $p) }))
	}
}

impl NativeCodegen<JavaScript> for Container {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let fields = view(ctx.native).ok_or_else(|| anyhow!("malformed container"))?;
		let names = ctx.names;
		let pinput = &ctx.runtime("Input");
		let poutput = &ctx.runtime("Output");
		let perror = &ctx.runtime("PdsError");
		let pequal = &ctx.runtime("equal");
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
		let check_count_call = |field: &ContainerField| -> js::Tokens {
			let Some(val) = count_check_arg(ctx.types, &fields, field) else {
				return quote!();
			};
			quote! {
				$(names.item(field.ty.target, "check_count"))(_0.$(field.name.as_str()), $((ctx.read)(val, false)));
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
					$(check_count_call(f))
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
