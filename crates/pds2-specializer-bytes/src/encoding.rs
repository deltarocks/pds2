use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Deref;

use anyhow::{Result, anyhow, bail, ensure};
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction, Expr};
use pds2_core::specializer::Registry;
use pds2_core::specializer::eval::{Args, Scope, Target, Val};
use pds2_core::specializer::expr::{Elaborator, describe};

use crate::Input;

pub trait Encoder {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()>;
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction>;
	fn min_size(&self, _cx: &Encoding, _id: TypeId, _native: &mir::NativeType) -> usize {
		0
	}
}

pub fn register_encoder(reg: &mut Registry, name: &str, encoder: impl Encoder + 'static) {
	reg.register_encoder::<dyn Encoder>(name, Box::new(encoder));
}

pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
	while v >= 0x80 {
		out.push((v as u8) | 0x80);
		v >>= 7;
	}
	out.push(v as u8);
}

pub struct Encoding<'a> {
	pub elab: &'a Elaborator<'a>,
	skipped: RefCell<BTreeMap<String, usize>>,
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
			skipped: RefCell::default(),
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

	pub fn min_size_of(&self, name: &str) -> usize {
		self.resolve_name(name).map_or(0, |id| self.min_size(id))
	}

	pub fn skip(&self, what: String) {
		*self.skipped.borrow_mut().entry(what).or_default() += 1;
	}

	pub fn skipped(&self) -> BTreeMap<String, usize> {
		self.skipped.take()
	}

	pub fn encode_number(&self, ty: TypeId, n: i128, out: &mut Vec<u8>) -> Result<()> {
		self.encode(&self.number(ty, n)?, &Args::new(), out)
	}

	fn encoder(&self, id: TypeId) -> Result<(&'a mir::NativeType, &'a dyn Encoder)> {
		let (native, name) = self.elab.encoder_name(id)?;
		let reg = self.elab.reg;
		let encoder = reg.encoder::<dyn Encoder>(name.as_str()).ok_or_else(|| {
			if reg.has_encoder(name.as_str()) {
				anyhow!(
					"{} uses encoder {name:?}, which doesn't write bytes",
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

	pub fn encode(&self, e: &Expr, args: &Args, out: &mut Vec<u8>) -> Result<()> {
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

	pub fn encode_root(&self, e: &Expr) -> Result<Vec<u8>> {
		let mut out = Vec::new();
		self.encode(e, &Args::new(), &mut out)?;
		Ok(out)
	}

	pub fn encode_use(
		&self,
		u: &mir::Type,
		value: &Expr,
		scope: &Scope,
		out: &mut Vec<u8>,
	) -> Result<()> {
		let value = self.typed(value.clone(), u.target)?;
		let args = self.use_args(u, scope)?;
		self.encode(&value, &args, out)
	}

	pub fn decode(&self, id: TypeId, args: &Args, input: &mut Input) -> Result<Expr> {
		let id = self.elab.resolve(id);
		if self.types().is_void(id) {
			return Ok(Expr::Construct(id, Construction::Record(vec![])));
		}
		let (native, encoder) = self.encoder(id)?;
		let c = encoder.decode(self, &Target { id, native, args }, input)?;
		Ok(Expr::Construct(id, c))
	}

	pub fn decode_use(&self, u: &mir::Type, scope: &Scope, input: &mut Input) -> Result<Expr> {
		let args = self.use_args(u, scope)?;
		self.decode(u.target, &args, input)
	}

	pub fn decode_root(&self, id: TypeId, bytes: &[u8]) -> Result<Expr> {
		let mut input = Input::new(bytes);
		let root = mir::Type {
			target: id,
			args: vec![],
		};
		let value = self.decode_use(&root, &Scope::of(&Args::new()), &mut input)?;
		ensure!(
			input.remaining() == 0,
			"{} bytes are left after {}",
			input.remaining(),
			self.name(id)
		);
		Ok(value)
	}
}
