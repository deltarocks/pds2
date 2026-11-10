use anyhow::{anyhow, bail, ensure, Result};
use pds2_core::generator::common::scalar_of;
use pds2_core::mir::{Construction, Expr, Value};
use pds2_core::specializer::decode::Input;
use pds2_core::specializer::encode::{
	bits_of, float_of, int_of, width, wrap, write_varint, Encoder, Encoding, Target, Val,
};
use pds2_core::specializer::{Registry, Scalar};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarEncoding {
	Varint,
	Varint64,
	Zigzag,
	Le,
	Be,
	Raw,
}

impl ScalarEncoding {
	pub const ALL: [ScalarEncoding; 6] = [
		ScalarEncoding::Varint,
		ScalarEncoding::Varint64,
		ScalarEncoding::Zigzag,
		ScalarEncoding::Le,
		ScalarEncoding::Be,
		ScalarEncoding::Raw,
	];

	pub fn name(&self) -> &'static str {
		match self {
			ScalarEncoding::Varint => "varint",
			ScalarEncoding::Varint64 => "varint64",
			ScalarEncoding::Zigzag => "zigzag",
			ScalarEncoding::Le => "le",
			ScalarEncoding::Be => "be",
			ScalarEncoding::Raw => "raw",
		}
	}
}

impl Encoder for ScalarEncoding {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let Construction::Literal(value) = v.c else {
			bail!("{} is not a literal", cx.name(v.id));
		};
		let s = scalar_of(cx.types(), v.id).ok_or_else(|| {
			anyhow!("{} needs `@scalar` for `@encoding {}`", cx.name(v.id), self.name())
		})?;
		let e = Expr::Construct(v.id, v.c.clone());
		let int = || int_of(&e).ok_or_else(|| anyhow!("{value:?} is not an integer"));
		match (self, s) {
			(ScalarEncoding::Raw, Scalar::String | Scalar::Bytes) => match value {
				Value::String(s) => out.extend_from_slice(s.as_bytes()),
				Value::Bytes(b) => out.extend_from_slice(b),
				v => bail!("{v:?} is not a string or bytes"),
			},
			(ScalarEncoding::Le | ScalarEncoding::Be, s) => {
				let bits = width(s).ok_or_else(|| anyhow!("{s:?} has no fixed width"))?;
				let raw = match s {
					Scalar::F32 => {
						(float_of(&e).ok_or_else(|| anyhow!("{value:?} is not a number"))? as f32)
							.to_bits() as u64
					}
					Scalar::F64 => float_of(&e)
						.ok_or_else(|| anyhow!("{value:?} is not a number"))?
						.to_bits(),
					_ => bits_of(int()?, bits),
				};
				let bytes = raw.to_le_bytes();
				let bytes = &bytes[..(bits / 8) as usize];
				if *self == ScalarEncoding::Le {
					out.extend_from_slice(bytes);
				} else {
					out.extend(bytes.iter().rev());
				}
			}
			(ScalarEncoding::Varint | ScalarEncoding::Varint64 | ScalarEncoding::Zigzag, s)
				if s.is_number() || s == Scalar::Bool =>
			{
				ensure!(
					!matches!(s, Scalar::F32 | Scalar::F64),
					"{} can't be a varint",
					cx.name(v.id)
				);
				let bits = width(s).expect("integer scalar");
				let n = int()?;
				let u = match self {
					ScalarEncoding::Varint => bits_of(n, bits),
					ScalarEncoding::Varint64 => bits_of(n, 64),
					_ => bits_of((n << 1) ^ (n >> (bits - 1)), bits),
				};
				write_varint(out, u);
			}
			(e, s) => bail!("`@encoding {}` can't encode {s:?} ({})", e.name(), cx.name(v.id)),
		}
		Ok(())
	}

	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let s = scalar_of(cx.types(), t.id).ok_or_else(|| {
			anyhow!("{} needs `@scalar` for `@encoding {}`", cx.name(t.id), self.name())
		})?;
		let int = |n: i128| {
			if s == Scalar::Bool {
				Value::Bool(n != 0)
			} else {
				Value::Signed(wrap(n, s))
			}
		};
		let value = match (self, s) {
			(ScalarEncoding::Raw, Scalar::String) => Value::String(
				String::from_utf8(input.rest().to_vec())
					.map_err(|e| anyhow!("{} is not UTF-8: {e}", cx.name(t.id)))?,
			),
			(ScalarEncoding::Raw, Scalar::Bytes) => Value::Bytes(input.rest().to_vec()),
			(ScalarEncoding::Le | ScalarEncoding::Be, s) => {
				let bits = width(s).ok_or_else(|| anyhow!("{s:?} has no fixed width"))?;
				let bytes = input.take((bits / 8) as usize)?;
				let mut buf = [0u8; 8];
				if *self == ScalarEncoding::Le {
					buf[..bytes.len()].copy_from_slice(bytes);
				} else {
					for (to, b) in buf.iter_mut().zip(bytes.iter().rev()) {
						*to = *b;
					}
				}
				let raw = u64::from_le_bytes(buf);
				match s {
					Scalar::F32 => Value::Float(f64::from(f32::from_bits(raw as u32))),
					Scalar::F64 => Value::Float(f64::from_bits(raw)),
					_ => int(i128::from(raw)),
				}
			}
			(ScalarEncoding::Varint | ScalarEncoding::Varint64 | ScalarEncoding::Zigzag, s)
				if s.is_number() || s == Scalar::Bool =>
			{
				ensure!(
					!matches!(s, Scalar::F32 | Scalar::F64),
					"{} can't be a varint",
					cx.name(t.id)
				);
				let bits = width(s).expect("integer scalar");
				let at = input.position();
				let u = input.varint()?;
				let fits = |u: u64| bits == 64 || u >> bits == 0;
				int(match self {
					ScalarEncoding::Varint64 => i128::from(u as i64),
					ScalarEncoding::Varint => {
						ensure!(fits(u), "byte {at}: varint {u} does not fit {bits} bits");
						i128::from(u)
					}
					_ => {
						ensure!(fits(u), "byte {at}: zigzag {u} does not fit {bits} bits");
						i128::from(u >> 1) ^ -i128::from(u & 1)
					}
				})
			}
			(e, s) => bail!("`@encoding {}` can't decode {s:?} ({})", e.name(), cx.name(t.id)),
		};
		Ok(Construction::Literal(value))
	}
}

pub fn register(reg: &mut Registry) {
	for e in ScalarEncoding::ALL {
		reg.register_encoder(e.name(), e);
	}
}
