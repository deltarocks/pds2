use pds2_core::ast::{self, BaseFieldRef, Block, FieldRefSegment, Ident, Item, ItemArgs, Statement};
use anyhow::{anyhow, bail, ensure, Context as _, Result};
use inflector::Inflector;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub mod plan;

enum ProtodefPathItem {
	Down(Ident),
	Arg(Ident),
}

pub struct ProtodefPath {
	up: usize,
	items: Vec<ProtodefPathItem>,
}

peg::parser!(
grammar protodef() for str {
	rule ident() -> Ident = s:$(['a'..='z' | 'A'..='Z' | '_']+) { s.into() }
	rule path_item() -> ProtodefPathItem = arg:"$"? ident:ident() { if arg.is_some() { ProtodefPathItem::Arg(ident) } else { ProtodefPathItem::Down(ident) } }
	pub rule path() -> ProtodefPath = v:("../")* items:path_item()++"/" {ProtodefPath {up: v.len(), items}}
}
);

pub fn parse_reference(r: &str, extra_ups: usize) -> Result<ast::FieldRef> {
	let path = protodef::path(r)?;
	for i in path.items.iter().skip(1) {
		ensure!(
			!matches!(i, ProtodefPathItem::Arg(_)),
			"argument only can be first"
		)
	}
	Ok(ast::FieldRef {
		up: path.up + extra_ups,
		path: BaseFieldRef(
			path.items
				.into_iter()
				.map(|i| match i {
					ProtodefPathItem::Down(v) => FieldRefSegment::Down {
						to: v,
						ignore_delegation: false,
					},
					ProtodefPathItem::Arg(v) => FieldRefSegment::Arg(v),
				})
				.collect::<Vec<_>>(),
		),
	})
}

#[derive(Default, Clone)]
pub struct NativeSigVariant {
	/// value (lowercase) parameters, declared as `argument(name)`
	pub args: BTreeSet<String>,
	pub virtual_fields: BTreeMap<String, &'static str>,
	pub arg_types: BTreeMap<String, &'static str>,
	/// type (uppercase) parameters, declared as `type(Name)`
	pub generics: BTreeSet<String>,
	/// whether the native receives a chained `=> T` element type (the `Value` generic)
	pub value_generic: bool,
	pub accepts_body: Option<&'static str>,
}
/// Discovered/declared signature of a native type, used to emit `def_native` blocks.
#[derive(Default, Clone)]
pub struct NativeSig {
	/// `@specializer` name, if the native is specializer-backed
	pub specializer: Option<String>,
	/// the native carries no data (`is_void;`)
	pub is_void: bool,
	pub scalar: Option<String>,
	pub encoding: Option<&'static str>,
	pub view: Option<&'static str>,
	pub default: NativeSigVariant,
	pub variants: BTreeMap<String, NativeSigVariant>,
}

/// One level of the lexical scope stack tracked during upgrade, used to convert
/// protodef references (bare names resolved by searching enclosing containers) into
/// pds references (explicit `../` counts).
#[derive(Default, Clone)]
pub struct ContainerScope {
	pub fields: BTreeSet<String>,
	pub switch_valued: BTreeSet<String>,
	pub inlined: BTreeMap<String, String>,
	pub types: BTreeMap<String, Value>,
}

enum ScopeFrame {
	/// a container with its field names
	Container(ContainerScope),
	/// a native wrapper (option, array element, generic binding): consumes one
	/// `../` level in pds scoping but holds no fields
	Wrapper,
}

type ParamKey = (String, String);

pub struct UpgradeCtx<'r> {
	registry: &'r UpgradeRegistry,
	natives: BTreeMap<String, NativeSig>,
	scopes: Vec<ScopeFrame>,
	type_scopes: Vec<Map<String, Value>>,
	counted: bool,
	param_uses: BTreeMap<ParamKey, Vec<Option<String>>>,
	param_types: BTreeMap<ParamKey, String>,
}

pub enum TagKind {
	Mapper(Map<String, Value>),
	Scalar,
	Bool,
	OptionScalar,
}

impl UpgradeCtx<'_> {
	fn named_type(&self, name: &str) -> Option<&Value> {
		self.type_scopes
			.iter()
			.rev()
			.find_map(|types| types.get(name))
	}
	pub fn classify_tag(&self, t: &Value) -> Option<TagKind> {
		self.classify_tag_at(t, 0)
	}
	fn classify_tag_at(&self, t: &Value, depth: usize) -> Option<TagKind> {
		if depth > 16 {
			return None;
		}
		match t {
			Value::String(name) => match self.named_type(name)? {
				Value::String(s) if s == "native" => {
					if name == "bool" {
						return Some(TagKind::Bool);
					}
					let sig = self.registry.get(name.as_str())?.sig();
					sig.scalar.is_some().then_some(TagKind::Scalar)
				}
				def => self.classify_tag_at(&def.clone(), depth + 1),
			},
			Value::Array(_) => match plan::head(t)? {
				"mapper" => plan::arg(t)?.as_object().cloned().map(TagKind::Mapper),
				"option" => match self.classify_tag_at(plan::arg(t)?, depth + 1)? {
					TagKind::Scalar => Some(TagKind::OptionScalar),
					_ => None,
				},
				_ => None,
			},
			_ => None,
		}
	}
	pub fn upgrade_type(&mut self, value: Value) -> Result<Vec<ast::Value>> {
		upgrade_type(value, self)
	}
	pub fn upgrade_type_generic(&mut self, name: &str, args: &[Value]) -> Result<Vec<ast::Value>> {
		upgrade_type_generic(name, args, self)
	}
	pub fn upgrade_value(&mut self, value: &Value) -> Result<ast::Value> {
		upgrade_value(value)
	}
	/// run `f` one native-wrapper level deeper (a chained `=> T` value, a generic
	/// binding): pds scoping counts the wrapper as one `../` level
	pub fn wrapped<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
		self.scopes.push(ScopeFrame::Wrapper);
		let r = f(self);
		self.scopes.pop();
		r
	}
	/// run `f` inside a container scope with the given field names
	pub fn in_container<T>(
		&mut self,
		scope: ContainerScope,
		f: impl FnOnce(&mut Self) -> Result<T>,
	) -> Result<T> {
		self.scopes.push(ScopeFrame::Container(scope));
		let r = f(self);
		self.scopes.pop();
		r
	}
	fn switch_valued(&self, r: &str) -> bool {
		let dots = r.matches("../").count();
		let head = plan::ref_head(r);
		self.scopes
			.iter()
			.rev()
			.filter_map(|f| match f {
				ScopeFrame::Container(c) => Some((&c.fields, &c.switch_valued)),
				ScopeFrame::Wrapper => None,
			})
			.skip(dots)
			.find(|(fields, _)| fields.contains(head))
			.is_some_and(|(_, switches)| switches.contains(head))
	}
	/// Convert a protodef reference to a pds one. Protodef resolves bare names by
	/// searching enclosing containers lexically (`../` skips one container); pds
	/// wants an explicit up-count where every container *and* native wrapper is a
	/// level, and level 1 is the reference holder itself.
	pub fn reference(&self, r: &str) -> Result<ast::FieldRef> {
		let mut parsed = parse_reference(r, 0)?;
		let dots = parsed.up;
		let head = match parsed.path.0.first() {
			Some(FieldRefSegment::Down { to, .. }) => to.as_str().to_owned(),
			Some(FieldRefSegment::Arg(_)) => {
				ensure!(dots == 0, "type parameter reference {r:?} can't go up");
				parsed.up = self.scopes.len();
				return Ok(parsed);
			}
			_ => bail!("protodef reference should start with a field name: {r:?}"),
		};
		let mut up = 1;
		let mut skipped = 0;
		let mut found = false;
		for frame in self.scopes.iter().rev() {
			match frame {
				ScopeFrame::Wrapper => up += 1,
				ScopeFrame::Container(c) => {
					if skipped < dots {
						skipped += 1;
						up += 1;
					} else if c.fields.contains(&head) {
						found = true;
						break;
					} else if let Some(holder) = c.inlined.get(&head) {
						parsed.path.0.insert(
							0,
							FieldRefSegment::Down {
								to: Ident::new(holder),
								ignore_delegation: false,
							},
						);
						found = true;
						break;
					} else {
						up += 1;
					}
				}
			}
		}
		ensure!(
			found,
			"reference {r:?} does not resolve in any enclosing container"
		);
		parsed.up = up;
		Ok(parsed)
	}
	/// The protodef type name of a referenced field, when the reference names a
	/// sibling (or an outer) field directly and its type is a plain name
	pub fn reference_type(&self, r: &str) -> Option<String> {
		let parsed = parse_reference(r, 0).ok()?;
		let [FieldRefSegment::Down { to, .. }] = parsed.path.0.as_slice() else {
			return None;
		};
		let mut skipped = 0;
		for frame in self.scopes.iter().rev() {
			let ScopeFrame::Container(c) = frame else {
				continue;
			};
			if skipped < parsed.up {
				skipped += 1;
				continue;
			}
			if let Some(t) = c.types.get(to.as_str()) {
				return t.as_str().map(str::to_owned);
			}
			if c.fields.contains(to.as_str()) {
				return None;
			}
		}
		None
	}
	fn scalar_of_type(&self, name: &str, depth: usize) -> Option<String> {
		if depth > 16 {
			return None;
		}
		match self.named_type(name)? {
			Value::String(s) if s == "native" => self.registry.get(name)?.sig().scalar,
			Value::String(other) => self.scalar_of_type(&other.clone(), depth + 1),
			_ => None,
		}
	}
	fn choose_param_types(&mut self) -> Result<()> {
		for ((def, param), uses) in std::mem::take(&mut self.param_uses) {
			let Some(uses) = uses.into_iter().collect::<Option<Vec<String>>>() else {
				continue;
			};
			let mut counts: BTreeMap<String, usize> = BTreeMap::new();
			for u in uses {
				*counts.entry(u).or_default() += 1;
			}
			let scalar = |t: &str| self.scalar_of_type(t, 0);
			let covers = |a: &str, b: &str| {
				a == b
					|| matches!((scalar(a), scalar(b)), (Some(x), Some(y)) if scalar_covers(&x, &y))
			};
			let chosen = counts
				.iter()
				.filter(|(c, _)| counts.keys().all(|o| covers(c, o)))
				.max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
				.map(|(c, _)| c.clone());
			let Some(chosen) = chosen else {
				bail!(
					"argument {param:?} of {def:?} is given {:?}, none of which holds all the others",
					counts.keys().collect::<Vec<_>>()
				);
			};
			self.param_types.insert((def, param), chosen);
		}
		Ok(())
	}
	pub fn upgrade_named_field(&mut self, obj: &Map<String, Value>) -> Result<Statement> {
		let name = obj
			.get("name")
			.and_then(Value::as_str)
			.ok_or_else(|| anyhow!("expected name in {obj:?}"))?;
		let ty = obj.get("type").ok_or_else(|| anyhow!("missing type"))?;
		let ty = self.wrapped(|ctx| ctx.upgrade_type(ty.clone()))?;
		Ok(Statement::simple(
			Item::simple_builder("field")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(name)))
				.then(ty),
		))
	}
}

fn simple_stmt(items: Vec<ast::Value>) -> Statement {
	Statement::simple(items)
}

pub fn upgrade_value(value: &Value) -> Result<ast::Value> {
	match value {
		Value::Number(n) => Ok(ast::Value::Int(
			n.as_i64().ok_or_else(|| anyhow!("bad integer"))? as i128,
		)),
		Value::Bool(b) => Ok(ast::Value::new_simple_item(if *b {
			"true"
		} else {
			"false"
		})),
		_ => bail!("unknown value type: {value:?}"),
	}
}

/// Convert a json value used in a positional argument slot (count, endVal, ...) into
/// a field reference or constant.
fn upgrade_scalar_arg(value: &Value, ctx: &UpgradeCtx) -> Result<ast::Value> {
	match value {
		Value::Number(_) | Value::Bool(_) => upgrade_value(value),
		Value::String(s) => Ok(ast::Value::FieldRef(ctx.reference(s)?)),
		// Keep anything more exotic as a string literal rather than bailing.
		other => Ok(ast::Value::new_string(other.to_string())),
	}
}

/// Convert a type that is expected to reduce to a single value (e.g. a length-prefix
/// type passed as a generic argument).
fn upgrade_type_single(value: &Value, ctx: &mut UpgradeCtx) -> Result<ast::Value> {
	let mut v = upgrade_type(value.clone(), ctx).context("single")?;
	ensure!(v.len() == 1, "expected a single-value type, got {v:?}");
	Ok(v.remove(0))
}

/// Sanitize an arbitrary protodef key (`minecraft:foo`, `0`, `true`) into a valid ident.
fn sanitize_ident(key: &str) -> String {
	let mut out: String = key
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' {
				c
			} else {
				'_'
			}
		})
		.collect();
	if out.is_empty() {
		out.push('_');
	}
	if out
		.chars()
		.next()
		.map(|c| c.is_ascii_digit())
		.unwrap_or(false)
	{
		out.insert(0, 'v');
	}
	out
}

// https://github.com/ProtoDef-io/node-protodef/blob/master/src/datatypes/compiler-conditional.js#L14
fn key_int(key: &str) -> Option<i128> {
	if let Ok(n) = key.parse::<i128>() {
		return Some(n);
	}
	let (neg, rest) = match key.strip_prefix('-') {
		Some(r) => (true, r),
		None => (false, key),
	};
	let n = i128::from_str_radix(rest.strip_prefix("0x")?, 16).ok()?;
	Some(if neg { -n } else { n })
}

/// `match:` value for a variant -- a number when the key is numeric, a bare
/// `true`/`false` item when the key is a boolean literal (so HIR maps it to
/// `Value::Bool` rather than a string discriminator), otherwise a discriminator string.
fn match_value(key: &str) -> ast::Value {
	if let Some(n) = key_int(key) {
		ast::Value::Int(n)
	} else if key == "true" || key == "false" {
		ast::Value::new_simple_item(key)
	} else {
		ast::Value::new_string(key.to_string())
	}
}

fn switch_fields(obj: &Map<String, Value>) -> Result<Map<String, Value>> {
	let fields = obj
		.get("fields")
		.and_then(Value::as_object)
		.ok_or_else(|| anyhow!("switch missing fields"))?;
	if let Some(key) = fields.keys().find(|k| k.starts_with('/')) {
		bail!("switch key {key:?} compares against a root context variable, which pds has no equivalent for");
	}
	let numeric = fields.keys().any(|k| key_int(k).is_some());
	Ok(fields
		.iter()
		.filter(|(k, _)| {
			let dead = numeric && matches!(k.as_str(), "true" | "false");
			if dead {
				tracing::warn!("switch key {k:?} never equals a number, dropping its arm");
			}
			!dead
		})
		.map(|(k, v)| (k.clone(), v.clone()))
		.collect())
}

fn bool_exhaustive(fields: &Map<String, Value>) -> bool {
	fields.contains_key("true") && fields.contains_key("false")
}

fn variant_stmt(name: &str, key: ast::Value, payload: Option<Vec<ast::Value>>) -> Statement {
	let variant = Item::simple_builder("variant").argsb(
		ItemArgs::builder()
			.arg(ast::Value::new_simple_item(sanitize_ident(name).as_str()))
			.named("match", key),
	);
	simple_stmt(match payload {
		Some(ty) => variant.then(ty),
		None => variant.build_values(),
	})
}

// https://github.com/ProtoDef-io/node-protodef/blob/master/src/datatypes/compiler-conditional.js#L17
fn default_stmt(
	obj: &Map<String, Value>,
	exhaustive: bool,
	ctx: &mut UpgradeCtx,
) -> Result<Option<Statement>> {
	Ok(match obj.get("default") {
		Some(default) => {
			let ty = ctx
				.wrapped(|ctx| ctx.upgrade_type(default.clone()))
				.context("union default value")?;
			Some(simple_stmt(Item::simple_builder("default").then(ty)))
		}
		None if exhaustive => None,
		None => Some(simple_stmt(Item::simple_builder("default").build_values())),
	})
}

fn upgrade_arm(ty: &Value, ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
	ctx.wrapped(|ctx| ctx.upgrade_type(ty.clone()))
		.context("union variant value")
}

/// protodef `switch`/`mapper` -> pds `union`.
pub fn upgrade_switch(obj: &Map<String, Value>, ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
	let compare_to = obj
		.get("compareTo")
		.and_then(Value::as_str)
		.ok_or_else(|| anyhow!("switch missing compareTo (compareToValue is not supported)"))?;
	ensure!(
		!ctx.switch_valued(compare_to),
		"switch compares {compare_to:?}, the value of another switch; protodef compares that \
		 payload, a pds union tag would compare its discriminant"
	);
	let fields = switch_fields(obj)?;

	let mut statements = Vec::new();
	for (key, ty) in &fields {
		statements.push(variant_stmt(
			key,
			match_value(key),
			Some(upgrade_arm(ty, ctx)?),
		));
	}
	statements.extend(default_stmt(obj, bool_exhaustive(&fields), ctx)?);

	Ok(Item::simple_builder("union")
		.argsb(ItemArgs::builder().named("tag", ast::Value::FieldRef(ctx.reference(compare_to)?)))
		.block(Block {
			attributes: Default::default(),
			statements,
			root: false,
		})
		.build_values())
}

fn upgrade_mapper(obj: &Map<String, Value>, ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
	let ty = obj
		.get("type")
		.ok_or_else(|| anyhow!("mapper missing type"))?;
	let mappings = obj
		.get("mappings")
		.and_then(Value::as_object)
		.ok_or_else(|| anyhow!("mapper missing mappings"))?;

	let mut statements = Vec::new();
	for (key, name) in mappings {
		let name = name
			.as_str()
			.ok_or_else(|| anyhow!("mapper mapping value should be string"))?;
		// `variant` with no chained type is a `void` arm — the tag is read via `Tag`.
		statements.push(simple_stmt(
			Item::simple_builder("variant")
				.argsb(
					ItemArgs::builder()
						.arg(ast::Value::new_simple_item(sanitize_ident(name).as_str()))
						.named("match", match_value(key)),
				)
				.build_values(),
		));
	}
	statements.push(simple_stmt(Item::simple_builder("default").build_values()));

	Ok(Item::simple_builder("union")
		.argsb(ItemArgs::builder().named("Tag",upgrade_type_single(ty, ctx)?))
		.block(Block {
			attributes: Default::default(),
			statements,
			root: false,
		})
		.build_values())
}

/// Merge a protodef `mapper` (discriminant) with the `switch` keyed on it into a
/// single self-contained pds inline-tag `union`: the mapper supplies the wire tag
/// type and each variant's `match` value, the switch supplies each variant's payload.
/// Wire-compatible with the separate mapper-field + external-switch encoding, but the
/// discriminant field disappears (the union reads its tag inline).
pub fn merge_mapper_switch(
	mapper: &Map<String, Value>,
	switch: &Map<String, Value>,
	ctx: &mut UpgradeCtx,
) -> Result<Vec<ast::Value>> {
	let tag_ty = mapper
		.get("type")
		.ok_or_else(|| anyhow!("mapper missing type"))?;
	let mappings = mapper
		.get("mappings")
		.and_then(Value::as_object)
		.ok_or_else(|| anyhow!("mapper missing mappings"))?;
	let fields = switch_fields(switch)?;
	let tag_ty = upgrade_type_single(tag_ty, ctx)?;

	let mut statements = Vec::new();
	let mut names = BTreeSet::new();
	let mut values = BTreeSet::new();
	for (val, name) in mappings {
		let name = name
			.as_str()
			.ok_or_else(|| anyhow!("mapper mapping value should be string"))?;
		let value = key_int(val).ok_or_else(|| anyhow!("mapper key {val:?} is not a number"))?;
		values.insert(value);
		let mut variant = sanitize_ident(name);
		while !names.insert(variant.clone()) {
			variant.push('_');
		}
		// payload from the switch, or a void arm when the switch maps this tag to nothing
		let payload = match fields.get(name).or_else(|| switch.get("default")) {
			Some(p) => Some(upgrade_arm(p, ctx)?),
			None => None,
		};
		statements.push(variant_stmt(&variant, ast::Value::Int(value), payload));
	}
	for (key, payload) in &fields {
		let Some(value) = key_int(key) else {
			continue;
		};
		if values.insert(value) {
			let mut variant = sanitize_ident(key);
			while !names.insert(variant.clone()) {
				variant.push('_');
			}
			statements.push(variant_stmt(
				&variant,
				ast::Value::Int(value),
				Some(upgrade_arm(payload, ctx)?),
			));
		}
	}
	statements.extend(default_stmt(switch, false, ctx)?);

	Ok(Item::simple_builder("union")
		.argsb(ItemArgs::builder().named("Tag",tag_ty))
		.block(Block {
			attributes: Default::default(),
			statements,
			root: false,
		})
		.build_values())
}

pub fn merge_scalar_switch(
	tag: &Value,
	switch: &Map<String, Value>,
	ctx: &mut UpgradeCtx,
) -> Result<Vec<ast::Value>> {
	let fields = switch_fields(switch)?;
	let tag_ty = upgrade_type_single(tag, ctx)?;
	let mut statements = Vec::new();
	for (key, ty) in &fields {
		statements.push(variant_stmt(
			key,
			match_value(key),
			Some(upgrade_arm(ty, ctx)?),
		));
	}
	statements.extend(default_stmt(switch, bool_exhaustive(&fields), ctx)?);
	Ok(Item::simple_builder("union")
		.argsb(ItemArgs::builder().named("Tag",tag_ty))
		.block(Block {
			attributes: Default::default(),
			statements,
			root: false,
		})
		.build_values())
}

pub fn keys_fit(kind: &TagKind, switch: &Map<String, Value>) -> bool {
	let Some(fields) = switch.get("fields").and_then(Value::as_object) else {
		return false;
	};
	fields.keys().all(|k| match kind {
		TagKind::Scalar | TagKind::OptionScalar => key_int(k).is_some(),
		TagKind::Bool => k == "true" || k == "false",
		TagKind::Mapper(m) => {
			key_int(k).is_some()
				|| m.get("mappings")
					.and_then(Value::as_object)
					.is_some_and(|ms| ms.values().any(|v| v.as_str() == Some(k)))
		}
	})
}

pub fn upgrade_type_generic(
	f: &str,
	args: &[Value],
	ctx: &mut UpgradeCtx,
) -> Result<Vec<ast::Value>> {
	if args.len() != 1 {
		bail!("unknown type: {f:?} => {args:?}");
	}
	let a = &args[0];

	Ok(if a.is_string() {
		let inner = ctx
			.wrapped(|ctx| ctx.upgrade_type(a.clone()))
			.context("unknown string type def")?;
		Item::simple_builder(f).then(inner)
	} else if a.is_array() {
		let inner = ctx
			.wrapped(|ctx| ctx.upgrade_type(a.clone()))
			.with_context(|| format!("unknown array type def <{f}>"))?;
		Item::simple_builder(f).then(inner)
	} else if a.is_object() {
		let obj = a.as_object().unwrap();
		let mut builder = Item::simple_builder(f);
		let mut args = ItemArgs::builder();
		let mut value = None;

		for (k, v) in obj {
			if k == "type" {
				value = Some(v.clone())
			} else if let Some(val) = k.strip_suffix("Val") {
				args = args.named(val, upgrade_value(v)?);
				// ctx.record_arg(f, val);
			} else if let Some(val) = k.strip_suffix("Type") {
				let val = Inflector::to_pascal_case(val);
				args = args.named(val.as_str(), upgrade_type_single(v, ctx)?);
				// ctx.record_arg(f, &val);
			} else if let Some(val) = k.strip_suffix("To") {
				args = args.named(
					val,
					ast::Value::FieldRef(
						ctx.reference(v.as_str().ok_or_else(|| anyhow!("bad reference value"))?)?,
					),
				);
				// ctx.record_arg(f, val);
			} else {
				// A plain value argument (count, endVal, ...).
				args = args.named(k.as_str(), upgrade_scalar_arg(v, ctx)?);
				// ctx.record_arg(f, k);
			}
		}
		let args = args.build();
		if !args.is_empty() {
			builder = builder.args(args);
		}

		let mut o = builder.build_values();
		if let Some(value) = value {
			o.extend(ctx.wrapped(|ctx| ctx.upgrade_type(value))?);
		}
		o
	} else {
		bail!("unknown type: {f:?} => {a:?}");
	})
}

pub fn upgrade_type(value: Value, ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
	match value {
		Value::Array(a) => {
			ensure!(a.len() >= 2, "array type should be 2 or more in length");
			let name = a[0]
				.as_str()
				.ok_or_else(|| anyhow!("first type array value should be string: {a:?}"))?;
			Ok(match name {
				"switch" => upgrade_switch(
					a[1].as_object()
						.ok_or_else(|| anyhow!("switch needs object"))?,
					ctx,
				)?,
				"mapper" => upgrade_mapper(
					a[1].as_object()
						.ok_or_else(|| anyhow!("mapper needs object"))?,
					ctx,
				)?,
				name if ctx.registry.get(name).is_none()
					&& ctx.named_type(name).is_some_and(|t| t != "native")
					&& a.len() == 2
					&& a[1].is_object() =>
				{
					let mut args = ItemArgs::builder();
					for (k, v) in a[1].as_object().expect("checked") {
						let value = match v {
							Value::String(r) => {
								let reference = ast::Value::FieldRef(ctx.reference(r)?);
								let got = ctx.reference_type(r);
								let key = (name.to_owned(), k.clone());
								ctx.param_uses.entry(key.clone()).or_default().push(got.clone());
								match (ctx.param_types.get(&key), got) {
									(Some(want), Some(got)) if *want != got => {
										let mut cast = Item::simple_builder(want.as_str())
											.argsb(ItemArgs::builder().arg(reference))
											.build_values();
										cast.remove(0)
									}
									_ => reference,
								}
							}
							v => upgrade_value(v)?,
						};
						args = args.named(k.as_str(), value);
					}
					Item::simple_builder(name).args(args.build()).build_values()
				}
				name if ctx.registry.get(name).is_some() => {
					let upgrade = ctx.registry.get(name).expect("existence checked");
					upgrade
						.upgrade(&a[1..], ctx)
						.with_context(|| format!("during upgrade of {name}"))?
				}
				v => upgrade_type_generic(v, &a[1..], ctx)?,
			})
		}
		Value::String(s) => Ok(Item::simple_builder(&s as &str).build_values()),
		v => bail!("can't understand type: {:?}", v),
	}
}

fn native_variant(sig: &NativeSigVariant) -> Vec<Statement> {
	let mut statements = Vec::new();
	for arg in &sig.args {
		let item = Item::simple_builder("argument")
			.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(arg.as_str())));
		statements.push(simple_stmt(match sig.arg_types.get(arg) {
			Some(ty) => item.then(vec![ast::Value::new_simple_item(*ty)]),
			None => item.build_values(),
		}));
	}
	for (name, ty) in &sig.virtual_fields {
		statements.push(simple_stmt(
			Item::simple_builder("virtual_field")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(name.as_str())))
				.then(vec![ast::Value::new_simple_item(*ty)]),
		));
	}
	for generic in &sig.generics {
		statements.push(simple_stmt(
			Item::simple_builder("type")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(generic.as_str())))
				.build_values(),
		));
	}
	if sig.value_generic {
		statements.push(simple_stmt(
			Item::simple_builder("type")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item("Value")))
				.build_values(),
		));
	}
	if let Some(decl) = sig.accepts_body {
		let mut block = ast::parse(decl).expect("specializer body declaration should parse");
		block.root = false;
		statements.push(simple_stmt(
			Item::simple_builder("body").block(block).build_values(),
		));
	}
	statements
}

fn native_block(sig: &NativeSig) -> Block {
	let mut statements = native_variant(&sig.default);
	if sig.is_void {
		statements.push(simple_stmt(Item::simple_builder("is_void").build_values()));
	}
	for (name, stmts) in &sig.variants {
		let body = native_variant(&stmts);
		statements.push(Statement::simple(
			Item::simple_builder("variant")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(name.as_str())))
				.block(Block {
					root: false,
					attributes: Default::default(),
					statements: body,
				})
				.build_values(),
		))
	}
	Block {
		attributes: Default::default(),
		statements,
		root: false,
	}
}

fn uses_count(v: &Value) -> bool {
	match v {
		Value::Array(items) => {
			items.first().and_then(Value::as_str) == Some("count") || items.iter().any(uses_count)
		}
		Value::Object(map) => map.values().any(uses_count),
		_ => false,
	}
}

fn int_range(s: &str) -> Option<(bool, u32)> {
	let (sign, bits) = s.split_at(1);
	let bits = bits.parse().ok()?;
	match sign {
		"i" => Some((true, bits)),
		"u" => Some((false, bits)),
		_ => None,
	}
}

fn scalar_covers(a: &str, b: &str) -> bool {
	if a == b {
		return true;
	}
	match (int_range(a), int_range(b)) {
		(Some((false, wa)), Some((false, wb))) | (Some((true, wa)), Some((true, wb))) => wb <= wa,
		(Some((true, wa)), Some((false, wb))) => wb < wa,
		_ => false,
	}
}

fn type_params(v: &Value, out: &mut BTreeSet<String>) {
	match v {
		Value::String(s) => {
			if let Some(p) = s.strip_prefix('$') {
				out.insert(p.to_owned());
			}
		}
		Value::Array(items) => items.iter().for_each(|i| type_params(i, out)),
		Value::Object(map) => map.values().for_each(|i| type_params(i, out)),
		_ => {}
	}
}

fn upgrade_namespace(value: &Value, ctx: &mut UpgradeCtx) -> Result<ast::Block> {
	ensure!(value.is_object(), "namespace should be object");
	let types = value
		.get("types")
		.and_then(Value::as_object)
		.cloned()
		.unwrap_or_default();
	ctx.type_scopes.push(types);
	let block = upgrade_namespace_inner(value, ctx);
	ctx.type_scopes.pop();
	block
}

fn upgrade_namespace_inner(value: &Value, ctx: &mut UpgradeCtx) -> Result<ast::Block> {
	let mut statements = Vec::new();
	let mut declared = BTreeSet::new();
	if let Some(types) = value.get("types") {
		for (k, v) in types.as_object().unwrap() {
			if v == "native" {
				let k = match k.as_str() {
					"switch" | "mapper" => "union",
					k => k,
				};
				if !declared.insert(k) {
					continue;
				}
				let sig = ctx
					.registry
					.get(k)
					.ok_or_else(|| anyhow!("upgrader for native type {k} is not registered"))?;
				let mut sig = sig.sig();
				let has_varint = types.get("varint").is_some_and(|v| v == "native");
				if (ctx.counted || has_varint) && matches!(k, "array" | "buffer") {
					if let Some(sized) = sig.variants.get_mut("sized") {
						sized.virtual_fields.insert("size".to_owned(), "varint");
					}
				}
				let mut builder = Item::simple_builder("def_native")
					.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k)));
				let block = native_block(&sig);
				if !block.statements.is_empty() {
					builder = builder.block(block);
				}
				let mut attributes = ast::Attributes::default();
				if let Some(spec) = &sig.specializer {
					attributes.insert(
						Ident::new("specializer"),
						vec![ast::Value::new_simple_item(spec.as_str())],
					);
				}
				if let Some(scalar) = &sig.scalar {
					attributes.insert(
						Ident::new("scalar"),
						vec![ast::Value::new_simple_item(scalar.as_str())],
					);
				}
				if let Some(encoding) = sig.encoding {
					attributes.insert(
						Ident::new("encoding"),
						vec![ast::Value::new_simple_item(encoding)],
					);
				}
				if let Some(view) = sig.view {
					attributes.insert(Ident::new("view"), vec![ast::Value::new_simple_item(view)]);
				}
				statements.push(Statement {
					attributes,
					items: builder.build_values(),
				});
			} else {
				let mut params = BTreeSet::new();
				type_params(v, &mut params);
				let mut def = Item::simple_builder("def")
					.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())));
				if !params.is_empty() {
					def = def.block(Block {
						attributes: Default::default(),
						statements: params
							.iter()
							.map(|p| {
								let arg = Item::simple_builder("argument")
									.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(p.as_str())));
								simple_stmt(match ctx.param_types.get(&(k.clone(), p.clone())) {
									Some(ty) => arg.then(vec![ast::Value::new_simple_item(ty.as_str())]),
									None => arg.build_values(),
								})
							})
							.collect(),
						root: false,
					});
				}
				statements.push(simple_stmt(def.then(upgrade_type(v.clone(), ctx)?)));
			}
		}
	}
	for (k, v) in value.as_object().unwrap().into_iter() {
		if k == "types" {
			continue;
		}
		statements.push(simple_stmt(
			Item::simple_builder("namespace")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
				.block(upgrade_namespace(v, ctx)?)
				.build_values(),
		));
	}
	Ok(ast::Block {
		attributes: Default::default(),
		statements,
		root: false,
	})
}

pub fn upgrade(mut value: Value, registry: &UpgradeRegistry) -> Result<ast::Block> {
	if let Some(types) = value.get_mut("types").and_then(Value::as_object_mut) {
		if types.get("bitfield").is_some_and(|t| t == "native") {
			for int in ["u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64"] {
				types
					.entry(int)
					.or_insert_with(|| Value::String("native".to_owned()));
			}
		}
	}
	let mut ctx = UpgradeCtx {
		registry,
		natives: BTreeMap::new(),
		scopes: Vec::new(),
		type_scopes: Vec::new(),
		counted: uses_count(&value),
		param_uses: BTreeMap::new(),
		param_types: BTreeMap::new(),
	};

	upgrade_namespace(&value, &mut ctx)?;
	let root_types = value.get("types").and_then(Value::as_object).cloned();
	ctx.type_scopes.push(root_types.unwrap_or_default());
	ctx.choose_param_types()?;
	ctx.type_scopes.pop();
	let mut block = upgrade_namespace(&value, &mut ctx)?;
	block.root = true;
	Ok(block)
}

#[cfg(test)]
mod tests {
	use serde_json::{json, Value};

	use super::UpgradeRegistry;

	fn upgrade(types: Value) -> anyhow::Result<String> {
		let mut reg = UpgradeRegistry::default();
		crate::register_upgraders(&mut reg);
		let mut all = json!({
			"varint": "native", "u8": "native", "i8": "native", "bool": "native",
			"void": "native", "container": "native", "switch": "native",
			"mapper": "native", "option": "native", "array": "native"
		});
		all.as_object_mut()
			.unwrap()
			.extend(types.as_object().unwrap().clone());
		let block = super::upgrade(json!({ "types": all }), &reg)?;
		let out = pds2_core::ast::format(&block);
		pds2_core::ast::parse(&out).map_err(|e| anyhow::anyhow!("{e}"))?;
		Ok(squash(&out))
	}

	fn squash(s: &str) -> String {
		s.split_whitespace().collect::<Vec<_>>().join(" ")
	}

	fn assert_has(out: &str, expected: &str) {
		assert!(out.contains(&squash(expected)), "{expected}\nnot in\n{out}");
	}

	#[test]
	fn adjacent_scalar_tag_becomes_inline_union() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "id", "type": "varint" },
			{ "name": "data", "type": ["switch", { "compareTo": "id", "fields": { "0": "u8" } }] }
		]] }))
		.unwrap();
		assert_has(
			&out,
			"def(p) => union(Tag: varint) { variant(v0, match: 0) => u8; default; };",
		);
	}

	#[test]
	fn body_natives_declare_their_items() {
		let out = upgrade(json!({
			"bitfield": "native",
			"p": ["bitfield", [{ "name": "x", "size": 8, "signed": true }]]
		}))
		.unwrap();
		assert_has(
			&out,
			"@specializer bitfield def_native(bitfield) { body { item(field) { argument(size); argument(signed) => false; }; }; };",
		);
		assert_has(&out, "def(p) => bitfield { field(x, size: 8, signed: true); };");
	}

	#[test]
	fn type_parameters_become_def_arguments() {
		let out = upgrade(json!({
			"data": ["switch", { "compareTo": "$compareTo", "fields": { "1": "u8" }, "default": "void" }],
			"p": ["container", [
				{ "name": "kind", "type": "varint" },
				{ "name": "x", "type": "u8" },
				{ "name": "payload", "type": ["data", { "compareTo": "kind" }] }
			]],
			"q": ["container", [
				{ "name": "x", "type": "u8" },
				{ "name": "y", "type": "u8" },
				{ "name": "payload", "type": ["data", { "compareTo": "x" }] }
			]]
		}))
		.unwrap();
		assert_has(
			&out,
			"def(data) { argument(compareTo) => varint; } => union(tag: ./$compareTo) {",
		);
		assert_has(&out, "field(payload) => data(compareTo: ../kind);");
		assert_has(&out, "field(payload) => data(compareTo: varint(../x));");
	}

	#[test]
	fn count_becomes_a_virtual_size() {
		let out = upgrade(json!({
			"buffer": "native", "i16": "native",
			"p": ["container", [
				{ "name": "n", "type": ["count", { "type": "i16", "countFor": "items" }] },
				{ "name": "items", "type": ["array", { "count": "n", "type": "u8" }] }
			]]
		}))
		.unwrap();
		assert_has(&out, "virtual_field(n, value: i16(./items/@size)) => i16;");
		assert_has(&out, "variant(sized) { argument(count); virtual_field(size) => varint; };");
	}

	#[test]
	fn anonymous_bitfield_names_are_reachable() {
		let out = upgrade(json!({
			"bitfield": "native",
			"p": ["container", [
				{ "anon": true, "type": ["bitfield", [
					{ "name": "kind", "size": 3, "signed": false },
					{ "name": "key", "size": 5, "signed": false }
				]] },
				{ "name": "value", "type": ["switch", { "compareTo": "kind", "fields": { "0": "u8" } }] }
			]]
		}))
		.unwrap();
		assert_has(&out, "union(tag: ../ANON/kind)");
		assert_has(&out, "def_native(u32);");
	}

	#[test]
	fn boolean_keys_of_numeric_switches_are_dropped() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "n", "type": "varint" },
			{ "name": "x", "type": "u8" },
			{ "name": "v", "type": ["switch", { "compareTo": "n", "fields": { "0": "void", "false": "void" }, "default": "u8" }] }
		]] }))
		.unwrap();
		assert!(!out.contains("match: false"), "{out}");
	}

	#[test]
	fn exhaustive_bool_switch_has_no_default() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "has", "type": "bool" },
			{ "name": "v", "type": ["switch", { "compareTo": "has", "fields": { "true": "u8", "false": "void" } }] }
		]] }))
		.unwrap();
		assert_has(&out, "def(p) => union(Tag: bool) { variant(false, match: false) => void; variant(true, match: true) => u8; };");
	}

	#[test]
	fn switch_on_switch_value_is_rejected() {
		let err = upgrade(json!({ "p": ["container", [
			{ "name": "a", "type": "u8" },
			{ "name": "b", "type": ["switch", { "compareTo": "a", "fields": { "1": "varint" } }] },
			{ "name": "x", "type": "u8" },
			{ "name": "c", "type": ["switch", { "compareTo": "b", "fields": { "2": "u8" } }] }
		]] }))
		.unwrap_err();
		assert!(
			format!("{err:#}").contains("the value of another switch"),
			"{err:#}"
		);
	}

	#[test]
	fn sibling_switches_on_one_tag_become_one_union() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "m", "type": "varint" },
			{ "name": "x", "type": ["switch", { "compareTo": "m", "fields": { "2": "u8" } }] },
			{ "name": "y", "type": ["switch", { "compareTo": "m", "fields": { "0": "varint", "2": "u8" } }] }
		]] }))
		.unwrap();
		assert_has(&out, "def(p) => union(Tag: varint) { variant(v0, match: 0) => varint; variant(v2, match: 2) => container { field(x) => u8; field(y) => u8; }; default; };");
	}

	#[test]
	fn switch_in_default_on_same_tag_is_fused() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "t", "type": "varint" },
			{ "name": "d", "type": ["switch", {
				"compareTo": "t",
				"fields": { "0": "void" },
				"default": ["container", [{ "name": "payload", "type": ["option", ["switch", { "compareTo": "t", "fields": { "1": "u8" } }]] }]]
			}] }
		]] }))
		.unwrap();
		assert_has(&out, "variant(v1, match: 1) => option => u8;");
		assert_has(&out, "default => option => void;");
	}

	#[test]
	fn anonymous_containers_are_flattened() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "a", "type": "varint" },
			{ "anon": true, "type": ["container", [
				{ "name": "b", "type": ["switch", { "compareTo": "a", "fields": { "1": "u8" } }] }
			]] }
		]] }))
		.unwrap();
		assert_has(
			&out,
			"def(p) => union(Tag: varint) { variant(v1, match: 1) => u8; default; };",
		);
	}

	#[test]
	fn hex_and_negative_keys_are_numbers() {
		let out = upgrade(json!({
			"e": ["mapper", { "type": "varint", "mappings": { "0x0a": "ten" } }],
			"p": ["container", [
				{ "name": "t", "type": "i8" },
				{ "name": "v", "type": ["switch", { "compareTo": "t", "fields": { "-1": "u8" } }] }
			]]
		}))
		.unwrap();
		assert_has(&out, "variant(ten, match: 10); default;");
		assert_has(&out, "variant(_1, match: -1) => u8;");
	}

	#[test]
	fn union_arms_are_a_scope_level() {
		let out = upgrade(json!({ "p": ["container", [
			{ "name": "len", "type": "varint" },
			{ "name": "k", "type": "u8" },
			{ "name": "v", "type": ["switch", { "compareTo": "k", "fields": { "1": ["array", { "count": "len", "type": "u8" }] } }] }
		]] }))
		.unwrap();
		assert_has(
			&out,
			"variant(v1, match: 1) => array(count: ../../len) => u8;",
		);
	}
}

pub trait Upgrade {
	fn name(&self) -> String;
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: None,
			..Default::default()
		}
	}
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ctx.upgrade_type_generic(&self.name(), data)
	}
}

#[derive(Default)]
pub struct UpgradeRegistry(std::collections::HashMap<String, Box<dyn Upgrade>>);

impl UpgradeRegistry {
	pub fn register(&mut self, upgrade: impl Upgrade + 'static) {
		let name = upgrade.name();
		assert!(
			self.0.insert(name.clone(), Box::new(upgrade)).is_none(),
			"upgrader {name:?} is registered twice"
		);
	}
	pub fn get(&self, name: &str) -> Option<&dyn Upgrade> {
		self.0.get(name).map(|b| b.as_ref())
	}
}
