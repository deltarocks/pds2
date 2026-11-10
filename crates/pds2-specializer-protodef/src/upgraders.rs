use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, ensure, Context as _, Result};
use tracing::info;

use pds2_core::ast::{self, Ident, Statement};

use crate::container::Container;
use crate::upgrade::{
	keys_fit, plan, ContainerScope, NativeSig, NativeSigVariant, TagKind, Upgrade, UpgradeCtx,
};

macro_rules! trivial_upgrade {
	($struct:ident => $name:literal) => {
		pub struct $struct;
		impl Upgrade for $struct {
			fn name(&self) -> String {
				$name.to_owned()
			}
		}
	};
	($struct:ident => $name:literal, scalar: $scalar:literal, encoding: $encoding:literal) => {
		pub struct $struct;
		impl Upgrade for $struct {
			fn name(&self) -> String {
				$name.to_owned()
			}
			fn sig(&self) -> NativeSig {
				NativeSig {
					scalar: Some($scalar.to_owned()),
					encoding: Some($encoding),
					..Default::default()
				}
			}
		}
	};
}

pub struct Void;

impl Upgrade for Void {
	fn name(&self) -> String {
		"void".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			is_void: true,
			..Default::default()
		}
	}
}

trivial_upgrade!(U8P => "u8", scalar: "u8", encoding: "be");

trivial_upgrade!(U16P => "u16", scalar: "u16", encoding: "be");

trivial_upgrade!(U32P => "u32", scalar: "u32", encoding: "be");

trivial_upgrade!(U64P => "u64", scalar: "u64", encoding: "be");

trivial_upgrade!(I8P => "i8", scalar: "i8", encoding: "be");

trivial_upgrade!(I16P => "i16", scalar: "i16", encoding: "be");

trivial_upgrade!(I32P => "i32", scalar: "i32", encoding: "be");

trivial_upgrade!(I64P => "i64", scalar: "i64", encoding: "be");

trivial_upgrade!(F32P => "f32", scalar: "f32", encoding: "be");

trivial_upgrade!(F64P => "f64", scalar: "f64", encoding: "be");

trivial_upgrade!(Varint => "varint", scalar: "i32", encoding: "varint");

trivial_upgrade!(Varlong => "varlong", scalar: "i64", encoding: "varint");

pub struct BoolP;

impl Upgrade for BoolP {
	fn name(&self) -> String {
		"bool".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			scalar: Some("bool".to_owned()),
			encoding: Some("be"),
			..Default::default()
		}
	}
}

pub struct RestBuffer;

impl Upgrade for RestBuffer {
	fn name(&self) -> String {
		"restBuffer".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			scalar: Some("bytes".to_owned()),
			encoding: Some("raw"),
			..Default::default()
		}
	}
}

impl Upgrade for Container {
	fn name(&self) -> String {
		"container".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("container".to_owned()),
			default: NativeSigVariant {
				accepts_body: Some(
					"item(field) { type(Value); }; item(virtual_field) { argument(value); type(Value); };",
				),
				..Default::default()
			},
			..Default::default()
		}
	}
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "container should have argument");
		let data = data[0]
			.as_array()
			.ok_or_else(|| anyhow!("expected container data to be array"))?;
		let fields = plan::inline_fields(data);
		let scope = plan::scope_names(&fields);
		let names: BTreeSet<String> = fields.iter().map(|(n, _)| n.clone()).collect();
		let inlined = plan::inlined_names(&fields);
		let mut types: BTreeMap<String, serde_json::Value> = fields
			.iter()
			.filter_map(|(n, f)| Some((n.clone(), f.get("type")?.clone())))
			.collect();
		types.extend(plan::inlined_types(&fields));

		// protodef `mapper` (discriminant) + `switch(compareTo: mapper)` is a pds
		// inline-tag union; merge them when the mapper feeds exactly one switch and
		// nothing else refers to it.
		type JsonMap = serde_json::Map<String, serde_json::Value>;
		enum Plan {
			Keep,
			Absorbed,
			Inline(TagKind, JsonMap, serde_json::Value, String),
			External(JsonMap),
			Virtual(String),
		}
		let mut plans: Vec<Plan> = fields.iter().map(|_| Plan::Keep).collect();
		for (i, (tag, tag_field)) in fields.iter().enumerate() {
			if tag_field.get("name").is_none() || !matches!(plans[i], Plan::Keep) {
				continue;
			}
			let refs = plan::count_refs(&fields, tag);
			if refs == 0 {
				continue;
			}
			let switch_of = |j: usize| -> Option<&JsonMap> {
				let ty = fields[j].1.get("type")?;
				(plan::head(ty)? == "switch").then_some(())?;
				plan::arg(ty)?.as_object()
			};
			let keyed = |j: usize| {
				switch_of(j)
					.and_then(|s| s.get("compareTo"))
					.and_then(serde_json::Value::as_str)
					== Some(tag.as_str())
			};
			let run: Vec<usize> = (i + 1..fields.len()).take_while(|&j| keyed(j)).collect();
			if run.len() >= 2 {
				if refs != run.len() || run.iter().any(|&j| !matches!(plans[j], Plan::Keep)) {
					continue;
				}
				let switches: Vec<&serde_json::Value> = run.iter().map(|&j| &fields[j].1).collect();
				let Some(product) = plan::product(tag, &switches, &scope) else {
					continue;
				};
				let Some(tag_ty) = tag_field.get("type") else {
					continue;
				};
				let Some(kind) = ctx.classify_tag(tag_ty) else {
					continue;
				};
				if matches!(kind, TagKind::OptionScalar) || !keys_fit(&kind, &product) {
					continue;
				}
				plans[i] = Plan::Absorbed;
				for &j in &run[1..] {
					plans[j] = Plan::Absorbed;
				}
				plans[run[0]] = Plan::Inline(kind, product, tag_ty.clone(), tag.clone());
				continue;
			}
			if refs > 2 {
				continue;
			}
			let Some(j) = (i + 1..fields.len()).find(|&j| keyed(j)) else {
				continue;
			};
			let Some(switch) = switch_of(j).filter(|_| matches!(plans[j], Plan::Keep)) else {
				continue;
			};
			let switch = if refs == 1 {
				switch.clone()
			} else {
				match plan::fuse(switch, &scope, tag) {
					Some(fused) => fused,
					None => continue,
				}
			};
			let Some(tag_ty) = tag_field.get("type") else {
				continue;
			};
			let Some(kind) = ctx.classify_tag(tag_ty) else {
				continue;
			};
			if !keys_fit(&kind, &switch) {
				continue;
			}
			if j == i + 1 && !matches!(kind, TagKind::OptionScalar) {
				plans[i] = Plan::Absorbed;
				plans[j] = Plan::Inline(kind, switch, tag_ty.clone(), fields[j].0.clone());
			} else {
				plans[i] = Plan::Virtual(fields[j].0.clone());
				plans[j] = Plan::External(switch);
			}
		}

		let emit = |ctx: &mut UpgradeCtx, i: usize| -> Result<Vec<ast::Value>> {
			let (name, field) = &fields[i];
			match &plans[i] {
				Plan::Inline(TagKind::Mapper(mapper), switch, _, _) => {
					info!("container field {name} looks like legacy protodef mapper+switch, it will be replaced with union");
					crate::upgrade::merge_mapper_switch(mapper, switch, ctx)
				}
				Plan::Inline(_, switch, tag, _) => {
					crate::upgrade::merge_scalar_switch(tag, switch, ctx)
				}
				Plan::External(switch) => crate::upgrade::upgrade_switch(switch, ctx),
				Plan::Keep | Plan::Virtual(_) | Plan::Absorbed => {
					let ty = field.get("type").ok_or_else(|| anyhow!("missing type"))?;
					ctx.upgrade_type(ty.clone())
				}
			}
		};

		let name_of = |i: usize| match &plans[i] {
			Plan::Inline(_, _, _, name) => name.clone(),
			_ => fields[i].0.clone(),
		};
		// effective = fields surviving the mapper merge
		let effective: Vec<usize> = (0..fields.len())
			.filter(|&i| !matches!(plans[i], Plan::Absorbed))
			.collect();
		let switch_valued: BTreeSet<String> = effective
			.iter()
			.filter(|&&i| match &plans[i] {
				Plan::Inline(..) | Plan::External(_) => true,
				Plan::Keep => fields[i].1.get("type").and_then(plan::head) == Some("switch"),
				_ => false,
			})
			.map(|&i| name_of(i))
			.collect();
		// A container left with a single field is a transparent wrapper: emit that
		// field's type directly (e.g. a packet *is* its body union, not a struct
		// wrapping it). Unframed so the inner type's references account for the missing
		// container level.
		if let [only] = effective.as_slice() {
			let name = name_of(*only);
			return emit(ctx, *only).with_context(|| format!("transparent container <{name}>"));
		}

		let mut statements = Vec::new();
		for i in effective {
			let name = name_of(i);
			let name = name.as_str();
			let scope = ContainerScope {
				fields: names.clone(),
				switch_valued: switch_valued.clone(),
				inlined: inlined.clone(),
				types: types.clone(),
			};
			let args = ast::ItemArgs::builder().arg(ast::Value::new_simple_item(name));
			if let Some(count) = count_of(&fields[i].1)? {
				let (counted, ty) = count;
				let size = ast::Value::FieldRef(ast::FieldRef {
					up: 0,
					path: ast::BaseFieldRef(vec![
						ast::FieldRefSegment::Down {
							to: Ident::new(counted),
							ignore_delegation: false,
						},
						ast::FieldRefSegment::Virtual(Ident::new("size")),
					]),
				});
				let mut cast = ast::Item::simple_builder(ty)
					.argsb(ast::ItemArgs::builder().arg(size))
					.build_values();
				statements.push(Statement::simple(
					ast::Item::simple_builder("virtual_field")
						.argsb(args.named("value", cast.remove(0)))
						.then(vec![ast::Value::new_simple_item(ty)]),
				));
				continue;
			}
			let ty = ctx
				.in_container(scope, |ctx| emit(ctx, i))
				.with_context(|| format!("container field <{name}> value"))?;
			let item = match &plans[i] {
				Plan::Virtual(union) => {
					ast::Item::simple_builder("virtual_field").argsb(args.named(
						"value",
						ast::Value::FieldRef(ast::FieldRef {
							up: 0,
							path: ast::BaseFieldRef(vec![
								ast::FieldRefSegment::Down {
									to: Ident::new(union),
									ignore_delegation: false,
								},
								ast::FieldRefSegment::Virtual(Ident::new("tag")),
							]),
						}),
					))
				}
				_ => ast::Item::simple_builder("field").argsb(args),
			};
			statements.push(Statement::simple(item.then(ty)));
		}
		Ok(ast::Item::simple_builder("container")
			.block(ast::Block {
				attributes: Default::default(),
				statements,
				root: false,
			})
			.build_values())
	}
}

/// protodef `["count", { type, countFor }]`: the count of a sibling list
fn count_of(field: &serde_json::Value) -> Result<Option<(&str, &str)>> {
	let Some(ty) = field.get("type") else {
		return Ok(None);
	};
	if plan::head(ty) != Some("count") {
		return Ok(None);
	}
	let arg = plan::arg(ty).ok_or_else(|| anyhow!("count needs an argument"))?;
	let int = arg
		.get("type")
		.and_then(serde_json::Value::as_str)
		.ok_or_else(|| anyhow!("count needs a type name"))?;
	let counted = arg
		.get("countFor")
		.and_then(serde_json::Value::as_str)
		.ok_or_else(|| anyhow!("count needs countFor"))?;
	ensure!(
		!counted.contains('/'),
		"count of {counted:?} should name a sibling field"
	);
	Ok(Some((counted, int)))
}

pub struct Switch;

impl Upgrade for Switch {
	fn name(&self) -> String {
		"switch".to_owned()
	}
	// 	fn sig(&self) -> NativeSig {
	// // NativeSig { args: , generics: (), value_generic: (), specializer: (), accepts_body: () }
	// 	}
}

pub struct Mapper;

impl Upgrade for Mapper {
	fn name(&self) -> String {
		"mapper".to_owned()
	}
}
