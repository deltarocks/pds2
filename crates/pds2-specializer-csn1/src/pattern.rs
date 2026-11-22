use anyhow::{Result, anyhow, bail, ensure};
use genco::lang::rust;
use genco::prelude::*;
use pds2_core::ast::Ident;
use pds2_core::hir;
use pds2_core::mir;

pub fn pattern(s: &str) -> Result<Vec<u8>> {
	ensure!(
		!s.is_empty() && s.bytes().all(|b| b"01LH".contains(&b)),
		"{s:?} is not a CSN.1 bit string, write it with 0, 1, L and H"
	);
	Ok(s.as_bytes().to_vec())
}

fn const_arg<'n>(native: &'n mir::NativeType, name: &str) -> Option<&'n hir::Value> {
	native
		.const_args
		.iter()
		.find(|(n, _)| n.as_str() == name)
		.map(|(_, v)| v)
}

pub fn pattern_arg(native: &mir::NativeType, name: &str) -> Result<Vec<u8>> {
	match const_arg(native, name) {
		Some(hir::Value::String(s)) => pattern(s),
		Some(v) => bail!("`{name}` should be a bit string like \"01\", got {v:?}"),
		None => bail!("`{name}` should be a constant bit string"),
	}
}

pub fn size_arg(native: &mir::NativeType, name: &str) -> Result<usize> {
	match const_arg(native, name) {
		Some(hir::Value::Number(n)) => {
			usize::try_from(*n).map_err(|_| anyhow!("`{name}` should not be negative, got {n}"))
		}
		Some(v) => bail!("`{name}` should be a number, got {v:?}"),
		None => bail!("`{name}` should be a constant number"),
	}
}

fn distinct(a: &[u8], b: &[u8]) -> bool {
	for (x, y) in a.iter().zip(b) {
		if x != y {
			return matches!(
				(x, y),
				(b'0', b'1') | (b'1', b'0') | (b'L', b'H') | (b'H', b'L')
			);
		}
	}
	false
}

pub fn prefix_free(arms: &[(&Ident, &[u8])]) -> Result<()> {
	for (i, (an, a)) in arms.iter().enumerate() {
		for (bn, b) in &arms[i + 1..] {
			ensure!(
				distinct(a, b),
				"{an:?} ({}) and {bn:?} ({}) can't be told apart: one starts the other, or 0/1 meets L/H",
				String::from_utf8_lossy(a),
				String::from_utf8_lossy(b)
			);
		}
	}
	Ok(())
}

pub fn literal(p: &[u8]) -> rust::Tokens {
	quote!($(format!("b\"{}\"", String::from_utf8_lossy(p))))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn arms_are_told_apart_by_their_first_differing_bit() {
		let (a, b, c) = (Ident::new("a"), Ident::new("b"), Ident::new("c"));
		assert!(prefix_free(&[(&a, b"0"), (&b, b"10"), (&c, b"11")]).is_ok());
		assert!(prefix_free(&[(&a, b"L"), (&b, b"H0"), (&c, b"H1")]).is_ok());
		assert!(prefix_free(&[(&a, b"0"), (&b, b"01")]).is_err());
		assert!(prefix_free(&[(&a, b"L"), (&b, b"1")]).is_err());
		assert!(prefix_free(&[(&a, b"1"), (&b, b"1")]).is_err());
		assert!(pattern("01x").is_err());
		assert!(pattern("").is_err());
	}
}
