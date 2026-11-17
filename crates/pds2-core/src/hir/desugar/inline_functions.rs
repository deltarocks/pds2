use std::collections::{HashMap, HashSet};

use anyhow::{bail, ensure, Context, Result};

use crate::{
	ast::{FieldRef, FieldRefSegment, Ident, Path},
	hir::{
		Expr, Function, Namespace, NamespaceItem, Reference, ReferenceKind, Traverse, TraverseSkip,
		Type, TypeKind, Visitor,
	},
	specializer::Registry,
};

type Params = Vec<(Ident, Option<TypeKind>)>;

struct Scope {
	ns: Vec<Ident>,
	generics: HashSet<Ident>,
	hygienic: bool,
}

struct Inliner<'r> {
	functions: HashMap<Vec<Ident>, Function>,
	names: HashSet<Vec<Ident>>,
	expanded: HashMap<Vec<Ident>, (Params, Expr)>,
	stack: Vec<Vec<Ident>>,
	reg: &'r Registry,
}

fn display(path: &[Ident]) -> String {
	path.iter().map(Ident::as_str).collect::<Vec<_>>().join("::")
}

fn cast_path(ty: &TypeKind) -> Option<&Path> {
	match ty {
		TypeKind::Reference(Reference {
			ty: ReferenceKind::Path(path),
			args,
			types,
			body,
		}) if args.is_empty() && types.is_empty() && body.is_empty() => Some(path),
		_ => None,
	}
}

fn param(r: &FieldRef) -> Option<&Ident> {
	match r.path.0.as_slice() {
		[FieldRefSegment::Arg(p)] if r.up == 0 => Some(p),
		_ => None,
	}
}

fn substitute(body: &Expr, bound: &[(Ident, Expr)]) -> Expr {
	let rec = |e: &Expr| substitute(e, bound);
	match body {
		Expr::Field(r) => {
			let p = param(r).expect("function bodies only reference parameters");
			bound
				.iter()
				.find(|(n, _)| n == p)
				.map(|(_, v)| v.clone())
				.expect("every parameter is bound")
		}
		Expr::Const(c) => Expr::Const(c.clone()),
		Expr::Binary(op, a, b) => Expr::Binary(*op, Box::new(rec(a)), Box::new(rec(b))),
		Expr::Cast(e, t) => Expr::Cast(Box::new(rec(e)), t.clone()),
		Expr::Typed(e, t) => Expr::Typed(Box::new(rec(e)), t.clone()),
		Expr::Record(fields) => Expr::Record(fields.iter().map(|(n, e)| (n.clone(), rec(e))).collect()),
		Expr::List(items) => Expr::List(items.iter().map(rec).collect()),
		Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(rec).collect()),
	}
}

impl Inliner<'_> {
	fn function(&self, path: &Path, scope: &Scope) -> Option<Vec<Ident>> {
		if path.simple_name().is_some_and(|n| scope.generics.contains(&n)) {
			return None;
		}
		let mut path = path.clone();
		path.rewrite_to_absolute(&scope.ns, &self.names);
		let path = path.segments();
		self.functions.contains_key(&path).then_some(path)
	}

	fn registered(&self, path: &Path) -> bool {
		path.simple_name()
			.is_some_and(|n| self.reg.function(n.as_str()).is_some())
	}

	fn absolutize(&self, ty: &mut TypeKind, scope: &Scope) -> Result<()> {
		let TypeKind::Reference(r) = ty else {
			return Ok(());
		};
		if let ReferenceKind::Path(path) = &mut r.ty {
			let mut found = path.clone();
			found.rewrite_to_absolute(&scope.ns, &self.names);
			if self.names.contains(&found.segments()) {
				*path = found;
			} else if !self.registered(path) {
				bail!("{path:?} is not a type or function");
			}
		}
		for t in r.nested_types_mut() {
			self.absolutize(t, scope)?;
		}
		Ok(())
	}

	fn body(&mut self, path: &[Ident]) -> Result<(Params, Expr)> {
		if let Some(done) = self.expanded.get(path) {
			return Ok(done.clone());
		}
		ensure!(
			!self.stack.iter().any(|p| p == path),
			"fn {} calls itself through {}",
			display(path),
			self.stack
				.iter()
				.map(|p| display(p))
				.collect::<Vec<_>>()
				.join(" -> ")
		);
		self.stack.push(path.to_vec());
		let Function { mut params, body } = self.functions[path].clone();
		let scope = Scope {
			ns: path[..path.len() - 1].to_vec(),
			generics: HashSet::new(),
			hygienic: true,
		};
		let done = (|| -> Result<(Params, Expr)> {
			for (p, ty) in params.iter_mut() {
				if let Some(ty) = ty {
					self.absolutize(ty, &scope)
						.with_context(|| format!("parameter {p:?}"))?;
				}
			}
			let body = self.expand(body, &scope)?;
			Ok((params, body))
		})()
		.with_context(|| format!("fn {}", display(path)));
		self.stack.pop();
		let done = done?;
		self.expanded.insert(path.to_vec(), done.clone());
		Ok(done)
	}

	fn call(&mut self, path: Vec<Ident>, args: Vec<Expr>) -> Result<Expr> {
		let (params, body) = self.body(&path)?;
		ensure!(
			params.len() == args.len(),
			"{}(...) takes {} values, got {}",
			display(&path),
			params.len(),
			args.len()
		);
		let bound: Vec<(Ident, Expr)> = params
			.into_iter()
			.zip(args)
			.map(|((p, ty), arg)| match ty {
				Some(ty) => (p, Expr::Typed(Box::new(arg), Box::new(ty))),
				None => (p, arg),
			})
			.collect();
		Ok(substitute(&body, &bound))
	}

	fn expand_all(&mut self, items: Vec<Expr>, scope: &Scope) -> Result<Vec<Expr>> {
		items.into_iter().map(|e| self.expand(e, scope)).collect()
	}

	fn expand(&mut self, e: Expr, scope: &Scope) -> Result<Expr> {
		Ok(match e {
			Expr::Field(_) | Expr::Const(_) => e,
			Expr::Binary(op, a, b) => Expr::Binary(
				op,
				Box::new(self.expand(*a, scope)?),
				Box::new(self.expand(*b, scope)?),
			),
			Expr::Typed(e, t) => Expr::Typed(Box::new(self.expand(*e, scope)?), t),
			Expr::Record(fields) => Expr::Record(
				fields
					.into_iter()
					.map(|(n, e)| Ok((n, self.expand(e, scope)?)))
					.collect::<Result<_>>()?,
			),
			Expr::List(items) => Expr::List(self.expand_all(items, scope)?),
			Expr::Call(path, args) => {
				let args = self.expand_all(args, scope)?;
				match self.function(&path, scope) {
					Some(f) => self.call(f, args)?,
					None => {
						ensure!(
							!scope.hygienic || self.registered(&path),
							"`{path:?}(...)` with {} values is not a declared or registered function",
							args.len()
						);
						Expr::Call(path, args)
					}
				}
			}
			Expr::Cast(operand, mut ty) => {
				let operand = self.expand(*operand, scope)?;
				if let Some(f) = cast_path(&ty).and_then(|p| self.function(p, scope)) {
					return self.call(f, vec![operand]);
				}
				if scope.hygienic {
					self.absolutize(&mut ty, scope)?;
				}
				Expr::Cast(Box::new(operand), ty)
			}
		})
	}
}

struct CallSites<'i, 'r> {
	inliner: &'i mut Inliner<'r>,
	scope: Scope,
}

impl Visitor for CallSites<'_, '_> {
	fn visit_type(&mut self, t: &mut Type, _reg: &Registry) -> Result<()> {
		self.scope.generics = t.types.keys().cloned().collect();
		Ok(())
	}
	fn visit_expr(&mut self, e: &mut Expr, _reg: &Registry) -> Result<()> {
		let taken = std::mem::replace(e, Expr::List(vec![]));
		*e = self.inliner.expand(taken, &self.scope)?;
		Ok(())
	}
}

fn take_functions(
	ns: &mut Namespace,
	path: &mut Vec<Ident>,
	out: &mut HashMap<Vec<Ident>, Function>,
) {
	for (name, f) in std::mem::take(&mut ns.functions) {
		let mut p = path.clone();
		p.push(name);
		out.insert(p, f);
	}
	for (name, item) in ns.items.iter_mut() {
		if let NamespaceItem::Inner(inner) = item {
			path.push(name.clone());
			take_functions(inner, path, out);
			path.pop();
		}
	}
}

fn inline_ns(ns: &mut Namespace, path: &mut Vec<Ident>, inliner: &mut Inliner, reg: &Registry) -> Result<()> {
	for (name, item) in ns.items.iter_mut() {
		match item {
			NamespaceItem::Inner(inner) => {
				path.push(name.clone());
				inline_ns(inner, path, inliner, reg).with_context(|| format!("namespace({name:?})"))?;
				path.pop();
			}
			NamespaceItem::Type(t) => {
				let mut sites = CallSites {
					inliner,
					scope: Scope {
						ns: path.clone(),
						generics: HashSet::new(),
						hygienic: false,
					},
				};
				t.traverse(&mut sites, TraverseSkip::empty(), reg)
					.with_context(|| format!("def({name:?})"))?;
			}
		}
	}
	Ok(())
}

pub fn inline_functions(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut functions = HashMap::new();
	take_functions(ns, &mut Vec::new(), &mut functions);
	if functions.is_empty() {
		return Ok(());
	}
	let mut names = HashSet::new();
	ns.discover_paths(&mut Vec::new(), &mut names);
	names.extend(functions.keys().cloned());
	let mut inliner = Inliner {
		functions,
		names,
		expanded: HashMap::new(),
		stack: Vec::new(),
		reg,
	};
	let mut paths: Vec<_> = inliner.functions.keys().cloned().collect();
	paths.sort();
	for path in paths {
		inliner.body(&path)?;
	}
	inline_ns(ns, &mut Vec::new(), &mut inliner, reg)
}
