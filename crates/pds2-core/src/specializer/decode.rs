use anyhow::{anyhow, bail, ensure, Result};

use super::encode::{int_of, Args, Encoding, Scope, Target};
use super::expr::describe;
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr};

pub struct Input<'b> {
	bytes: &'b [u8],
	pos: usize,
	base: usize,
}

impl<'b> Input<'b> {
	pub fn new(bytes: &'b [u8]) -> Self {
		Self::at(bytes, 0)
	}

	pub fn at(bytes: &'b [u8], base: usize) -> Self {
		Input {
			bytes,
			pos: 0,
			base,
		}
	}

	pub fn offset(&self) -> usize {
		self.pos
	}

	pub fn position(&self) -> usize {
		self.base + self.pos
	}

	pub fn rewind(&mut self, at: usize) {
		self.pos = at;
	}

	pub fn remaining(&self) -> usize {
		self.bytes.len() - self.pos
	}

	pub fn peek(&self) -> &'b [u8] {
		&self.bytes[self.pos..]
	}

	pub fn take(&mut self, n: usize) -> Result<&'b [u8]> {
		ensure!(
			n <= self.remaining(),
			"byte {}: {n} bytes needed, {} left",
			self.position(),
			self.remaining()
		);
		let out = &self.bytes[self.pos..self.pos + n];
		self.pos += n;
		Ok(out)
	}

	pub fn byte(&mut self) -> Result<u8> {
		Ok(self.take(1)?[0])
	}

	pub fn rest(&mut self) -> &'b [u8] {
		let out = self.peek();
		self.pos = self.bytes.len();
		out
	}

	pub fn sub(&mut self, n: usize) -> Result<Input<'b>> {
		let base = self.position();
		Ok(Input::at(self.take(n)?, base))
	}

	pub fn varint(&mut self) -> Result<u64> {
		let start = self.position();
		let mut out = 0u64;
		for i in 0..10 {
			let b = self.byte()?;
			let bits = u64::from(b & 0x7f);
			ensure!(i < 9 || bits <= 1, "byte {start}: varint overflows 64 bits");
			out |= bits << (7 * i);
			if b & 0x80 == 0 {
				return Ok(out);
			}
		}
		bail!("byte {start}: varint is longer than 10 bytes")
	}
}

impl Encoding<'_> {
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

	pub fn count(&self, e: &Expr) -> Result<usize> {
		let n = int_of(e).ok_or_else(|| anyhow!("{} is not a number", describe(e)))?;
		usize::try_from(n).map_err(|_| anyhow!("count {n} is negative"))
	}
}
