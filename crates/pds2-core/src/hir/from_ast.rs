use std::collections::BTreeMap;

use crate::{
	ast::{BaseFieldRef, Block, FieldRef, FieldRefSegment, Ident, Item, ItemArgs, ParseValue, Statement, Value},
	hir::{
		BodyDecl, BodyField, BodyItemDecl, Expr, Function, NamespaceItem,
		NativeType, Reference, ReferenceKind, Value as HirValue,
	},
	specializer::Registry,
};

use super::{IdGenerator, Namespace, Type, TypeKind};
use anyhow::{anyhow, bail, ensure, Context, Result};

fn parse_specializer_body(block: &Block) -> Result<Vec<BodyField>> {
	let mut out = Vec::new();
	for stmt in &block.statements {
		let (_attrs, item, mut items) = stmt.clone().pop_item()?;
		let kind = item.name.expect_simple_name()?;
		let name: Option<Ident> = item.args.arg(0, "body field name")?;
		let mut args = BTreeMap::new();
		for n in item.args.named_names().cloned().collect::<Vec<_>>() {
			ensure!(
				!n.is_type_arg(),
				"body item {kind:?} takes its type as `=> type`, not as argument {n:?}"
			);
			let value: Value = item.args.named(n.clone())?;
			args.insert(
				n.clone(),
				arg_value(value).with_context(|| format!("body item {kind:?} argument {n:?}"))?,
			);
		}
		item.args.verify_unused()?;
		ensure!(
			item.block.is_empty(),
			"body item {kind:?} can't have a block"
		);
		let ty = if items.is_empty() {
			None
		} else {
			let type_item = items
				.remove(0)
				.into_item()
				.ok_or_else(|| anyhow!("body field type should be item"))?;
			Some(TypeKind::from_ast(type_item, items)?)
		};
		out.push(BodyField {
			kind,
			name,
			args,
			ty,
		});
	}
	Ok(out)
}

fn const_value(value: Value) -> Result<HirValue> {
	Ok(match value {
		Value::Int(i) => HirValue::Number(i),
		Value::Float(f) => HirValue::Float(f),
		Value::Bytes(b) => HirValue::Bytes(b),
		Value::String { string, .. } => HirValue::String(string),
		Value::Item(item) => match item.simple_name().as_ref().map(Ident::as_str) {
			Some("true") => HirValue::Bool(true),
			Some("false") => HirValue::Bool(false),
			_ => bail!("expected a constant, got {item:?}"),
		},
		other => bail!("expected a constant, got {other:?}"),
	})
}

fn record(fields: Vec<(Ident, Value)>) -> Result<Expr> {
	Ok(Expr::Record(
		fields
			.into_iter()
			.map(|(n, v)| {
				let e = arg_value(v).with_context(|| format!("field {n:?}"))?;
				Ok((n, e))
			})
			.collect::<Result<_>>()?,
	))
}

pub fn arg_value(value: Value) -> Result<Expr> {
	Ok(match value {
		Value::FieldRef(r) => Expr::Field(r),
		Value::Binary(op, a, b) => {
			Expr::Binary(op, Box::new(arg_value(*a)?), Box::new(arg_value(*b)?))
		}
		Value::List(items) => Expr::List(items.into_iter().map(arg_value).collect::<Result<_>>()?),
		Value::Record(fields) => record(fields)?,
		Value::Item(item) if item.block.is_empty() && !item.args.is_empty() => {
			let operand = match item.args.positional_count() {
				0 => {
					let mut fields = Vec::new();
					for n in item.args.named_names().cloned().collect::<Vec<_>>() {
						let v: Value = item.args.named(n.clone())?;
						fields.push((n, v));
					}
					record(fields)?
				}
				1 => {
					let operand: Value = item.args.arg(0, "cast operand")?;
					arg_value(operand)?
				}
				n => {
					let mut args = Vec::new();
					for i in 0..n {
						let v: Value = item.args.arg(i, "call argument")?;
						args.push(arg_value(v)?);
					}
					item.args.verify_unused()?;
					return Ok(Expr::Call(item.name, args));
				}
			};
			item.args.verify_unused()?;
			let ty = TypeKind::from_ast(
				Item {
					name: item.name,
					args: ItemArgs::default(),
					block: Block::default(),
				},
				vec![],
			)?;
			Expr::Cast(Box::new(operand), Box::new(ty))
		}
		Value::Item(item)
			if item.block.is_empty()
				&& !item.name.absolute
				&& item.name.path.is_empty()
				&& !matches!(item.name.item.as_str(), "true" | "false") =>
		{
			Expr::Const(HirValue::UnionDescriminator(item.name.item))
		}
		v => Expr::Const(const_value(v).context(
			"argument should be a field reference, constant, arithmetic, record, list or `type(value)` cast",
		)?),
	})
}

fn argument_type(name: &Ident, mut items: Vec<Value>) -> Result<Option<TypeKind>> {
	if items.is_empty() {
		return Ok(None);
	}
	let type_item = items
		.remove(0)
		.into_item()
		.ok_or_else(|| anyhow!("argument {name:?} type should be item"))?;
	Ok(Some(TypeKind::from_ast(type_item, items)?))
}

impl BodyDecl {
	fn from_ast(block: Block) -> Result<Self> {
		let mut out = BodyDecl::default();
		for stmt in block.statements {
			let (_attrs, item, items) = stmt.pop_item()?;
			ensure!(
				items.is_empty() && item.name.expect_simple_name()?.as_str() == "item",
				"expected `item(kind) {{ ... }}` in body declaration"
			);
			let kind: Ident = item.args.arg(0, "body item kind")?;
			item.args.verify_unused()?;
			let decl =
				BodyItemDecl::from_ast(item.block).with_context(|| format!("item({kind:?})"))?;
			ensure!(
				out.item(&kind).is_none(),
				"body item {kind:?} declared twice"
			);
			out.0.push((kind, decl));
		}
		Ok(out)
	}
}

impl BodyItemDecl {
	fn from_ast(block: Block) -> Result<Self> {
		let mut out = BodyItemDecl::default();
		for stmt in block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			match item.name.expect_simple_name()?.as_str() {
				"anonymous" => {
					ensure!(items.is_empty(), "anonymous has no items");
					item.args.verify_unused()?;
					out.anonymous = true;
				}
				"argument" => {
					let name: Ident = item.args.arg(0, "argument name")?;
					item.args.verify_unused()?;
					ensure!(
						!name.is_type_arg(),
						"body item types are declared with `type(Value)`, got argument {name:?}"
					);
					let default = match items.len() {
						0 => None,
						1 => Some(const_value(items.remove(0)).context("argument default")?),
						_ => bail!("argument {name:?} takes one default value"),
					};
					ensure!(
						out.args.iter().all(|(n, _)| n != &name),
						"argument {name:?} declared twice"
					);
					out.args.push((name, default));
				}
				"type" => {
					let name: Ident = item.args.arg(0, "type name")?;
					item.args.verify_unused()?;
					ensure!(
						name.as_str() == "Value",
						"body items only take the chained `Value` type, got {name:?}"
					);
					let default = if items.is_empty() {
						None
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("default type should be item"))?;
						Some(TypeKind::from_ast(type_item, items)?)
					};
					ensure!(out.value.is_none(), "type(Value) declared twice");
					out.value = Some(default);
				}
				n => bail!("unknown body item declaration statement: {n:?}"),
			}
		}
		Ok(out)
	}
}

pub fn parse_ast(ast: Block, reg: &Registry) -> Result<Namespace> {
	let (ast, _) = crate::definition::lower(ast, reg).context("lowering custom definitions")?;
	let mut ns = Namespace::from_ast(ast).context("parsing AST to IR")?;
	super::desugar::inline_functions(&mut ns, reg).context("inlining functions")?;
	super::desugar::resolve_calls(&mut ns, reg).context("resolving calls")?;
	let id = IdGenerator::new();
	super::desugar::give_all_types_ids(id.clone(), &mut ns, reg)?;

	super::desugar::rewrite_paths_to_absolute(&mut ns);
	super::desugar::resolve_paths_to_ids(&mut ns).context("resolving paths")?;
	super::desugar::infer_generics(&mut ns, reg).context("inferring generics")?;

	super::desugar::apply_specializers(&mut ns, reg).context("applying specializers")?;
	super::desugar::check_levels(&ns).context("checking reference levels")?;

	// Make monomorphization only see scope-free references in bindings
	super::desugar::extract_nested_types(id.clone(), &mut ns, reg)?;
	super::desugar::monomorphize(id.clone(), &mut ns, reg)?;
	// After monomorphization we can see new inline kinds (cloned template bodies)
	super::desugar::extract_nested_types(id, &mut ns, reg)?;
	super::desugar::replace_aliases(&mut ns, reg)?;
	Ok(ns)
}

fn mark_params(e: &mut Expr, params: &[(Ident, Option<TypeKind>)]) -> Result<()> {
	match e {
		Expr::Field(r) => bail!("a function sees only its parameters, got {r:?}"),
		Expr::Const(HirValue::UnionDescriminator(n)) if params.iter().any(|(p, _)| p == n) => {
			*e = Expr::Field(FieldRef {
				up: 0,
				path: BaseFieldRef(vec![FieldRefSegment::Arg(n.clone())]),
			});
		}
		Expr::Const(_) => {}
		Expr::Binary(_, a, b) => {
			mark_params(a, params)?;
			mark_params(b, params)?;
		}
		Expr::Cast(e, _) | Expr::Typed(e, _) => mark_params(e, params)?,
		Expr::Record(fields) => {
			for (_, e) in fields {
				mark_params(e, params)?;
			}
		}
		Expr::List(items) | Expr::Call(_, items) => {
			for e in items {
				mark_params(e, params)?;
			}
		}
	}
	Ok(())
}

impl Function {
	fn from_ast(value: Item, mut items: Vec<Value>) -> Result<(Ident, Function)> {
		let mut args = value.args.ordered();
		let name = match args.next() {
			Some((None, v)) => Ident::parse(v.clone()).context("function name")?,
			_ => bail!("expected `fn(name, params...) => expression`"),
		};
		let function = (|| -> Result<Function> {
			ensure!(value.block.is_empty(), "fn takes no block");
			ensure!(
				items.len() == 1,
				"fn takes one body expression: `fn(name, params...) => expression`"
			);
			let mut params: Vec<(Ident, Option<TypeKind>)> = Vec::new();
			for (k, v) in args {
				let (param, ty) = match k {
					None => (Ident::parse(v.clone()).context("parameter name")?, None),
					Some(k) => {
						let ty = v
							.clone()
							.into_item()
							.ok_or_else(|| anyhow!("parameter {k:?} type should be a type"))?;
						(k.clone(), Some(TypeKind::from_ast(ty, vec![])?))
					}
				};
				ensure!(
					!matches!(param.as_str(), "true" | "false"),
					"{param:?} can't name a parameter"
				);
				ensure!(
					params.iter().all(|(p, _)| p != &param),
					"parameter {param:?} declared twice"
				);
				params.push((param, ty));
			}
			ensure!(
				!params.is_empty(),
				"fn takes at least one parameter, write a constant as `def(name) => const(value: ...)`"
			);
			let mut body = arg_value(items.remove(0)).context("body")?;
			mark_params(&mut body, &params).context("body")?;
			Ok(Function { params, body })
		})()
		.with_context(|| format!("fn <{name:?}>"))?;
		Ok((name, function))
	}
}

impl Type {
	pub fn from_ast(value: Item, items: Vec<Value>, block: Block) -> Result<Type> {
		let mut params = Vec::new();
		let mut param_types = BTreeMap::new();
		let mut types = BTreeMap::new();
		let mut export = false;
		let mut derives = Vec::new();
		for stmt in block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			match item.name.expect_simple_name()?.as_str() {
				"argument" => {
					let name: Ident = item.args.arg(0, "argument name")?;
					item.args.verify_unused()?;
					if let Some(ty) = argument_type(&name, items)? {
						param_types.insert(name.clone(), ty);
					}
					params.push(name);
				}
				"export" => {
					ensure!(items.is_empty(), "export has no items");
					item.args.verify_unused()?;
					ensure!(!export, "export set twice");
					export = true;
				}
				"derive" => {
					ensure!(items.is_empty(), "derive has no items");
					ensure!(item.args.positional_count() > 0, "derive needs at least one trait");
					for i in 0..item.args.positional_count() {
						let name: Ident = item.args.arg(i, "derived trait")?;
						ensure!(!derives.contains(&name), "derive {name:?} set twice");
						derives.push(name);
					}
					item.args.verify_unused()?;
				}
				"type" => {
					let name: Ident = item.args.arg(0, "generic name")?;
					item.args.verify_unused()?;
					let default = if items.is_empty() {
						None
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("missing virtual field type"))?;
						Some(TypeKind::from_ast(type_item, items)?)
					};

					ensure!(
						types.insert(name.clone(), default).is_none(),
						"generic set twice: {name:?}"
					);
				}
				v => bail!("unknown def block statement type: {}", v),
			};
		}

		Ok(Type {
			_id: Default::default(),
			types,
			params,
			param_types,
			kind: TypeKind::from_ast(value, items)?,
			export,
			derives,
		})
	}
}

impl TypeKind {
	fn from_ast(value: Item, mut items: Vec<Value>) -> Result<Self> {
		// ensure!(items.is_empty(), "unexpected values");
		let body = parse_specializer_body(&value.block)?;

		let mut args = Vec::new();
		let mut types = BTreeMap::new();
		// We don't have reference resolved at this phase, so verify_unused check will only reject unused unnamed arguments
		for name in value.args.named_names() {
			if name.is_type_arg() {
				let ty: Item = value.args.named(name.clone())?;
				let kind = TypeKind::from_ast(ty, vec![])?;
				if types.insert(name.clone(), kind).is_some() {
					bail!("generic set twice: {name:?}")
				}
			} else {
				let value: Value = value
					.args
					.named(name.clone())
					.context("parsing value argument")?;
				args.push((name.clone(), arg_value(value)?));
			}
		}
		value.args.verify_unused()?;

		if !items.is_empty() {
			let item = items
				.remove(0)
				.into_item()
				.ok_or_else(|| anyhow!("plain type value should be item"))?;
			let kind = TypeKind::from_ast(item, items)?;
			if types.insert(Ident::new("Value"), kind).is_some() {
				bail!("generic set twice: Value");
			}
		}

		Ok(TypeKind::Reference(Reference {
			ty: ReferenceKind::Path(value.name),
			args,
			types,
			body,
		}))
	}
}

impl Namespace {
	fn from_ast(value: Block) -> Result<Self> {
		let mut out = BTreeMap::new();
		let mut functions = BTreeMap::new();
		for statement in value.statements {
			let (attrs, item, mut items) = statement.pop_item()?;

			if item.name.simple_name().is_some_and(|n| n.as_str() == "fn") {
				let (name, function) = Function::from_ast(item, items)?;
				ensure!(
					functions.insert(name.clone(), function).is_none(),
					"function is already defined: {name:?}"
				);
				continue;
			}

			let (name, item) = match item.name.expect_simple_name()?.as_str() {
				"namespace" => {
					ensure!(items.is_empty(), "namespace has no items");
					let name = item.args.arg(0, "ns name")?;
					let ns = Namespace::from_ast(item.block)
						.with_context(|| format!("namespace {:?}", name))?;
					(name, NamespaceItem::Inner(ns))
				}
				"def_native" => {
					// TODO: Parse @type, arguments
					ensure!(items.is_empty(), "def_native has no items");
					// ensure!(item.args.is_empty(), "args not supported");

					let specializer = if let Some(specs) = attrs.get(&Ident::new("specializer")) {
						ensure!(specs.len() == 1, "only one specializer is allowed");
						let spec = specs.get(0).unwrap();
						let specializer = Ident::parse(spec.clone())?;
						Some(specializer)
					} else {
						None
					};
					let scalar = if let Some(scalars) = attrs.get(&Ident::new("scalar")) {
						ensure!(scalars.len() == 1, "only one scalar is allowed");
						let scalar = Ident::parse(scalars[0].clone())?;
						ensure!(
							crate::specializer::Scalar::parse(scalar.as_str()).is_some(),
							"unknown scalar {scalar:?}"
						);
						Some(scalar)
					} else {
						None
					};
					let mut native_attrs = BTreeMap::new();
					for (k, vs) in &attrs {
						if matches!(k.as_str(), "specializer" | "scalar") {
							continue;
						}
						let vs = vs
							.iter()
							.map(|v| Ident::parse(v.clone()))
							.collect::<Result<Vec<_>>>()
							.with_context(|| format!("attribute @{k:?}"))?;
						native_attrs.insert(k.clone(), vs);
					}

					enum NativeStmt {
						Param(Ident, Option<TypeKind>),
						VirtualField(Ident, TypeKind),
						Field(Ident, TypeKind),
						Type(Ident, Option<TypeKind>),
						Body(BodyDecl),
						IsVoid,

						Variant(Ident, Vec<NativeStmt>),
					}
					impl NativeStmt {
						fn from_ast(value: Statement) -> Result<Self> {
							let (_attrs, item, mut items) = value.pop_item()?;

							Ok(match item.name.expect_simple_name()?.as_str() {
								"variant" => {
									ensure!(items.is_empty(), "argument has no items");
									let id = item.args.arg(0, "variant name")?;
									item.args.verify_unused()?;

									let mut stmts = vec![];
									for ele in item.block.statements {
										let native = NativeStmt::from_ast(ele)?;
										ensure!(
											!matches!(native, NativeStmt::Variant(..)),
											"variants can't be nested"
										);
										stmts.push(native);
									}

									Self::Variant(id, stmts)
								}
								"argument" => {
									let name: Ident = item.args.arg(0, "argument name")?;
									let ty = argument_type(&name, items)?;
									Self::Param(name, ty)
								}
								"body" => {
									ensure!(items.is_empty(), "body has no items");
									item.args.verify_unused()?;
									Self::Body(BodyDecl::from_ast(item.block).context("body")?)
								}
								"is_void" => {
									ensure!(items.is_empty(), "is_void has no items");
									item.args.verify_unused()?;
									Self::IsVoid
								}
								"virtual_field" => {
									ensure!(
										!items.is_empty(),
										"virtual_field needs a type: `virtual_field(name) => type`"
									);
									let type_item = items
										.remove(0)
										.into_item()
										.ok_or_else(|| anyhow!("missing virtual field type"))?;
									// let block = item.block;
									Self::VirtualField(
										item.args.arg(0, "field name")?,
										TypeKind::from_ast(type_item, items)?,
									)
								}
								"field" => {
									ensure!(
										!items.is_empty(),
										"field needs a type: `field(name) => type`"
									);
									let type_item = items
										.remove(0)
										.into_item()
										.ok_or_else(|| anyhow!("missing field type"))?;
									// let block = item.block;
									Self::Field(
										item.args.arg(0, "field name")?,
										TypeKind::from_ast(type_item, items)?,
									)
								}
								"type" => {
									let default = if items.is_empty() {
										None
									} else {
										let type_item = items
											.remove(0)
											.into_item()
											.ok_or_else(|| anyhow!("missing virtual field type"))?;
										Some(TypeKind::from_ast(type_item, items)?)
									};
									Self::Type(item.args.arg(0, "type name")?, default)
								}
								n => {
									bail!("unknown native statement: {n:?}")
								}
							})
						}
					}

					/// Per-variant accumulator, before lowering to [`NativeType`].
					#[derive(Default)]
					struct NativeParts {
						params: Vec<Ident>,
						param_types: BTreeMap<Ident, TypeKind>,
						virtual_fields: Vec<(Ident, TypeKind)>,
						fields: Vec<(Ident, TypeKind)>,
						types: BTreeMap<Ident, Option<TypeKind>>,
						accepts_body: Option<BodyDecl>,
						is_void: bool,
					}
					impl NativeParts {
						fn extend(&mut self, from: &Self) -> Result<()> {
							self.params.extend(from.params.iter().cloned());
							self.param_types.extend(
								from.param_types.iter().map(|(k, v)| (k.clone(), v.clone())),
							);
							self.virtual_fields
								.extend(from.virtual_fields.iter().cloned());
							self.fields.extend(from.fields.iter().cloned());
							self.types
								.extend(from.types.iter().map(|(k, v)| (k.clone(), v.clone())));
							if let Some(decl) = &from.accepts_body {
								ensure!(
									self.accepts_body.is_none(),
									"body declared both on the native and its variant"
								);
								self.accepts_body = Some(decl.clone());
							}
							self.is_void |= from.is_void;
							Ok(())
						}
						/// A native's generics/params live on the surrounding [`Type`] like
						/// any generic def, so the generic-type machinery covers natives.
						fn into_type(
							self,
							specializer: Option<Ident>,
							scalar: Option<Ident>,
							attrs: BTreeMap<Ident, Vec<Ident>>,
							variant: Ident,
						) -> Type {
							let specialization = self
								.types
								.iter()
								.map(|(n, t)| {
									(
										n.clone(),
										t.clone().unwrap_or_else(|| {
											TypeKind::Reference(Reference {
												ty: ReferenceKind::Generic(n.clone()),
												args: vec![],
												types: BTreeMap::new(),
												body: vec![],
											})
										}),
									)
								})
								.collect();
							Type {
								_id: Default::default(),
								kind: TypeKind::Native(NativeType {
									specializer,
									variant,
									virtual_fields: self.virtual_fields,
									fields: self.fields,
									accepts_body: self.accepts_body,
									specialization,
									body: vec![],
									const_args: vec![],
									ref_args: vec![],
									arg_types: vec![],
									arg_virtuals: vec![],
									is_void: self.is_void,
									scalar,
									attrs,
								}),
								types: self.types,
								params: self.params,
								param_types: self.param_types,
								export: false,
								derives: Vec::new(),
							}
						}
					}

					fn native_type_statements(
						stmts: Vec<NativeStmt>,
					) -> Result<(NativeParts, BTreeMap<Ident, NativeParts>)> {
						let mut parts = NativeParts::default();
						let mut variants = BTreeMap::new();
						for statement in stmts {
							match statement {
								NativeStmt::Param(name, ty) => {
									if let Some(ty) = ty {
										parts.param_types.insert(name.clone(), ty);
									}
									parts.params.push(name);
								}
								NativeStmt::VirtualField(name, ty) => {
									parts.virtual_fields.push((name, ty))
								}
								NativeStmt::Field(name, ty) => parts.fields.push((name, ty)),
								NativeStmt::Type(ty, default) => ensure!(
									parts.types.insert(ty.clone(), default).is_none(),
									"generic set twice: {ty:?}"
								),
								NativeStmt::Body(decl) => {
									ensure!(parts.accepts_body.is_none(), "body declared twice");
									parts.accepts_body = Some(decl);
								}
								NativeStmt::IsVoid => parts.is_void = true,
								NativeStmt::Variant(name, v) => {
									let (var, vars) = native_type_statements(v)?;
									assert!(vars.is_empty(), "checked in stmt parsing");
									variants.insert(name.clone(), var);
								}
							}
						}
						Ok((parts, variants))
					}

					let mut native_stmt = Vec::new();
					for statement in item.block.statements {
						let stmt = NativeStmt::from_ast(statement)?;
						native_stmt.push(stmt);
					}
					let (default, variants) = native_type_statements(native_stmt)?;

					let has_types =
						!default.types.is_empty() || variants.values().any(|v| !v.types.is_empty());
					let accepts_body = default.accepts_body.is_some()
						|| variants.values().any(|v| v.accepts_body.is_some());

					let name: Ident = item.args.arg(0, "native name")?;
					ensure!(
						specializer.is_some() || (!has_types && !accepts_body),
						"specializer should be set when the native has generics or a body in <{name:?}>"
					);

					if variants.is_empty() {
						// No overloads: the native is a single type named directly.
						(
							name,
							NamespaceItem::Type(
								default.into_type(specializer, scalar, native_attrs, Ident::new("default")),
							),
						)
					} else {
						// Overloaded native: emit a namespace of per-variant types
						// (`name::variant`). Use-site references to the bare `name` select an
						// overload by shape during path resolution; `name::variant` selects
						// explicitly.
						ensure!(
							!variants.contains_key(&Ident::new("default")),
							"default variant name is reserved"
						);
						let mut items = BTreeMap::new();
						for (vname, mut parts) in variants {
							parts
								.extend(&default)
								.with_context(|| format!("variant {vname:?}"))?;
							items.insert(
								vname.clone(),
								NamespaceItem::Type(parts.into_type(
									specializer.clone(),
									scalar.clone(),
									native_attrs.clone(),
									vname,
								)),
							);
						}
						(
							name,
							NamespaceItem::Inner(Namespace {
								items,
								functions: BTreeMap::new(),
								overloads: true,
							}),
						)
					}
				}
				"def" => {
					ensure!(!items.is_empty(), "def has no items");
					let name: Ident = item.args.arg(0, "def name")?;
					let type_item = items
						.remove(0)
						.into_item()
						.ok_or_else(|| anyhow!("type def should be item"))?;
					let block = item.block;
					(
						name.clone(),
						NamespaceItem::Type(
							Type::from_ast(type_item, items, block)
								.with_context(|| format!("def <{:?}>", name))?,
						),
					)
				}
				n => bail!("unknown top level block statement: {:?}", n),
			};
			ensure!(
				out.insert(name.to_owned(), item).is_none(),
				"item is already defined: {:?}",
				name
			);
		}
		for name in functions.keys() {
			ensure!(
				!out.contains_key(name),
				"{name:?} is defined both as a function and an item"
			);
		}
		Ok(Self {
			items: out,
			functions,
			overloads: false,
		})
	}
}
