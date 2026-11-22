use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Deref;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use pds2_core::ast::Ident;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction, Expr, Value};
use pds2_core::specializer::eval::{Args, Scope, Target, Val, const_eq};
use pds2_core::specializer::expr::{Elaborator, describe};
use pds2_core::specializer::reflect::{count_arg, list_of, native_variant, use_of};
use pds2_core::specializer::{Function, Registry, VALUE};
use pds2_specializer_protodef::container;

pub use pds_bits_runtime::{Input, Output};

pub trait Encoder {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()>;
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction>;
	fn min_size(&self, _cx: &Encoding, _id: TypeId, _native: &mir::NativeType) -> usize {
		0
	}
}

pub fn register_encoder(reg: &mut Registry, name: &str, encoder: impl Encoder + 'static) {
	reg.register_encoder::<dyn Encoder>(name, Box::new(encoder));
}

pub fn read(input: &Input, n: u32) -> Result<u64> {
	let at = input.position();
	input
		.read(n)
		.map_err(|_| anyhow!("bit {at}: {n} bits needed, {} left", input.remaining()))
}

pub fn skip(input: &Input, n: usize) -> Result<()> {
	let at = input.position();
	input
		.skip(n)
		.map_err(|_| anyhow!("bit {at}: {n} bits needed, {} left", input.remaining()))
}

pub fn choose(input: &Input, patterns: &[&[u8]]) -> Result<usize> {
	let at = input.position();
	let shown = || {
		patterns
			.iter()
			.map(|p| String::from_utf8_lossy(p).into_owned())
			.collect::<Vec<_>>()
			.join(" | ")
	};
	pds_bits_runtime::csn1::choose(input, patterns).map_err(|e| match e {
		pds_bits_runtime::Error::NotEnoughData => {
			anyhow!("bit {at}: the input ends inside {{ {} }}", shown())
		}
		_ => anyhow!("bit {at}: none of {{ {} }} matches", shown()),
	})
}

pub struct Encoding<'a> {
	pub elab: &'a Elaborator<'a>,
	sizes: RefCell<BTreeMap<TypeId, Option<usize>>>,
}

impl<'a> Deref for Encoding<'a> {
	type Target = Elaborator<'a>;
	fn deref(&self) -> &Elaborator<'a> {
		self.elab
	}
}

impl<'a> Encoding<'a> {
	pub fn new(elab: &'a Elaborator<'a>) -> Self {
		Encoding {
			elab,
			sizes: RefCell::default(),
		}
	}

	pub fn min_size(&self, id: TypeId) -> usize {
		let id = self.elab.resolve(id);
		if self.types().is_void(id) {
			return 0;
		}
		if let Some(known) = self.sizes.borrow().get(&id) {
			return known.unwrap_or(0);
		}
		let Ok((native, encoder)) = self.encoder(id) else {
			return 0;
		};
		self.sizes.borrow_mut().insert(id, None);
		let n = encoder.min_size(self, id, native);
		self.sizes.borrow_mut().insert(id, Some(n));
		n
	}

	fn encoder(&self, id: TypeId) -> Result<(&'a mir::NativeType, &'a dyn Encoder)> {
		let (native, name) = self.elab.encoder_name(id)?;
		let reg = self.elab.reg;
		let encoder = reg.encoder::<dyn Encoder>(name.as_str()).ok_or_else(|| {
			if reg.has_encoder(name.as_str()) {
				anyhow!(
					"{} uses encoder {name:?}, which doesn't write bits",
					self.name(id)
				)
			} else {
				anyhow!(
					"no encoder {name:?} is registered, {} uses it",
					self.name(id)
				)
			}
		})?;
		Ok((native, encoder))
	}

	pub fn encode(&self, e: &Expr, args: &Args, out: &mut Output) -> Result<()> {
		let Expr::Construct(id, c) = e else {
			bail!("{} is not a constant value", describe(e));
		};
		if self.types().is_void(*id) {
			return Ok(());
		}
		let (native, encoder) = self.encoder(*id)?;
		encoder.encode(
			self,
			&Val {
				id: *id,
				native,
				c,
				args,
			},
			out,
		)
	}

	pub fn encode_use(
		&self,
		u: &mir::Type,
		value: &Expr,
		scope: &Scope,
		out: &mut Output,
	) -> Result<()> {
		let value = self.typed(value.clone(), u.target)?;
		let args = self.use_args(u, scope)?;
		self.encode(&value, &args, out)
	}

	pub fn encode_root(&self, e: &Expr) -> Result<Vec<u8>> {
		let mut out = Output::new();
		self.encode(e, &Args::new(), &mut out)?;
		Ok(out.into_inner())
	}

	pub fn decode(&self, id: TypeId, args: &Args, input: &Input) -> Result<Expr> {
		let id = self.elab.resolve(id);
		if self.types().is_void(id) {
			return Ok(Expr::Construct(id, Construction::Record(vec![])));
		}
		let (native, encoder) = self.encoder(id)?;
		let c = encoder.decode(self, &Target { id, native, args }, input)?;
		Ok(Expr::Construct(id, c))
	}

	pub fn decode_use(&self, u: &mir::Type, scope: &Scope, input: &Input) -> Result<Expr> {
		let args = self.use_args(u, scope)?;
		self.decode(u.target, &args, input)
	}

	pub fn decode_root(&self, id: TypeId, bytes: &[u8]) -> Result<Expr> {
		let input = Input::new(bytes);
		let root = mir::Type {
			target: id,
			args: vec![],
		};
		let value = self.decode_use(&root, &Scope::of(&Args::new()), &input)?;
		let left = input.remaining();
		ensure!(
			left < 8 && read(&input, left as u32)? == 0,
			"{left} bits are left after {}",
			self.name(id)
		);
		Ok(value)
	}
}

pub struct SerializedBits;

impl Function for SerializedBits {
	fn call(&self, cx: &Elaborator, _ty: Option<TypeId>, args: &[Expr]) -> Result<Option<Expr>> {
		let [value] = args else {
			bail!("`serialized_bits(value)` takes one value");
		};
		let value = cx.elaborate(value.clone())?;
		let bytes = Encoding::new(cx)
			.encode_root(&value)
			.context("serialized_bits(...)")?;
		Ok(Some(Expr::Const(Value::Bytes(bytes))))
	}
}

pub struct DeserializedBits;

impl Function for DeserializedBits {
	fn call(&self, cx: &Elaborator, ty: Option<TypeId>, args: &[Expr]) -> Result<Option<Expr>> {
		let Some(ty) = ty else {
			return Ok(None);
		};
		let [bytes] = args else {
			bail!("`deserialized_bits(bytes)` takes one value");
		};
		let bytes = match bytes {
			Expr::Call(..) => cx.elaborate(bytes.clone())?,
			e => cx.operand(e.clone())?,
		};
		let bytes = match bytes {
			Expr::Const(Value::Bytes(b))
			| Expr::Construct(_, Construction::Literal(Value::Bytes(b))) => b,
			e => bail!("deserialized_bits(...) takes bytes, got {}", describe(&e)),
		};
		let value = Encoding::new(cx)
			.decode_root(ty, &bytes)
			.with_context(|| format!("deserialized_bits(...) as {}", cx.name(ty)))?;
		Ok(Some(value))
	}
}

pub struct Container;

impl Encoder for Container {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		container::view(native)
			.into_iter()
			.flatten()
			.map(|f| cx.min_size(f.ty.target))
			.fold(0, usize::saturating_add)
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let fields = container::view(v.native).ok_or_else(|| anyhow!("malformed container"))?;
		let scope = v.scope();
		for f in &fields {
			cx.field(v.id, v.c, v.args, f.name)
				.and_then(|value| cx.encode_use(f.ty, &value, &scope, out))
				.with_context(|| format!("field {:?}", f.name))?;
		}
		Ok(())
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let fields = container::view(t.native).ok_or_else(|| anyhow!("malformed container"))?;
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
}

pub struct Array;

impl Encoder for Array {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		match native_variant(native) {
			"inline" => use_of(native, "Count").map_or(0, |c| cx.min_size(c.target)),
			_ => 0,
		}
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Output) -> Result<()> {
		let items = list_of(v.c)?;
		let scope = v.scope();
		if native_variant(v.native) == "inline" {
			let count = use_of(v.native, "Count")?;
			let n = cx.number(count.target, items.len() as i128)?;
			cx.encode_use(count, &n, &scope, out).context("count")?;
		} else {
			count_arg(v, items.len())?;
		}
		let value = use_of(v.native, VALUE)?;
		for (i, item) in items.iter().enumerate() {
			cx.encode_use(value, item, &scope, out)
				.with_context(|| format!("item {i}"))?;
		}
		Ok(())
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &Input) -> Result<Construction> {
		let n = if native_variant(t.native) == "inline" {
			let count = use_of(t.native, "Count")?;
			let n = cx.decode_use(count, &t.scope(), input).context("count")?;
			cx.count(&n).context("count")?
		} else {
			let n = t
				.args
				.get(&Ident::new("count"))
				.ok_or_else(|| anyhow!("{} has no count argument", cx.name(t.id)))?;
			cx.count(n).context("count")?
		};
		let value = use_of(t.native, VALUE)?;
		let items = (0..n)
			.map(|i| {
				cx.decode_use(value, &t.scope(), input)
					.with_context(|| format!("item {i}"))
			})
			.collect::<Result<_>>()?;
		Ok(Construction::List(items))
	}
}
