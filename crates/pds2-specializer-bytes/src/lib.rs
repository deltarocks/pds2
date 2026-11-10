use anyhow::{anyhow, bail, ensure, Result};
use pds2_core::generator::common::scalar_of;
use pds2_core::mir::{Construction, Expr, Value};
use pds2_core::specializer::encode::{bits_of, float_of, int_of, width, write_varint, Encoder, Encoding, Val};
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
}

pub fn register(reg: &mut Registry) {
	for e in ScalarEncoding::ALL {
		reg.register_encoder(e.name(), e);
	}
}
