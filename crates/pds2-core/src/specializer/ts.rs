use genco::lang::{JavaScript, js};
use genco::prelude::*;

use anyhow::Result;

use super::{LowerCx, Names, NativeCodegenCtx, Scalar, VALUE, specialization_use};
use crate::hir::TypeId;
use crate::mir;

pub type Tokens = js::Tokens;
pub type Ctx<'a> = NativeCodegenCtx<'a, JavaScript>;

pub fn pds(names: &dyn Names<JavaScript>, name: &str) -> Tokens {
	names.runtime(name)
}

pub fn typed_array(names: &dyn Names<JavaScript>, value: TypeId) -> Option<&'static str> {
	Some(match names.scalar(value)? {
		Scalar::I8 => "Int8Array",
		Scalar::U8 => "Uint8Array",
		Scalar::I16 => "Int16Array",
		Scalar::U16 => "Uint16Array",
		Scalar::I32 => "Int32Array",
		Scalar::U32 => "Uint32Array",
		Scalar::I64 => "BigInt64Array",
		Scalar::U64 => "BigUint64Array",
		Scalar::F32 => "Float32Array",
		Scalar::F64 => "Float64Array",
		Scalar::Bool | Scalar::String | Scalar::Bytes => return None,
	})
}

pub fn array_ty(names: &dyn Names<JavaScript>, value: TypeId) -> Tokens {
	match typed_array(names, value) {
		Some(a) => quote!($a),
		None => quote!(Array<$(names.ty(value))>),
	}
}

pub fn construct_array(
	cx: &LowerCx<JavaScript>,
	native: &mir::NativeType,
	c: &mir::Construction,
) -> Result<Tokens> {
	let items = super::list_items(cx, c)?;
	let list = quote!([$(for i in items join(, ) => $i)]);
	Ok(
		match value_of(native).and_then(|v| typed_array(cx.names, v)) {
			Some(a) => quote!(new $a($list)),
			None => list,
		},
	)
}

pub fn wide(names: &dyn Names<JavaScript>, ty: TypeId) -> bool {
	matches!(names.scalar(ty), Some(Scalar::I64 | Scalar::U64))
}

pub fn int_as(names: &dyn Names<JavaScript>, ty: TypeId, expr: Tokens) -> Tokens {
	if wide(names, ty) {
		quote!(BigInt($expr))
	} else {
		expr
	}
}

pub fn value_of(native: &mir::NativeType) -> Option<TypeId> {
	specialization_use(native, VALUE).map(|u| u.target)
}

pub fn collect(ctx: &Ctx, value: TypeId, count: Tokens, read: Tokens) -> Tokens {
	let perror = &pds(ctx.names, "PdsError");
	match typed_array(ctx.names, value) {
		Some(a) => quote! {
			if ($(&count) > input.remaining()) {
				throw new $perror("NotEnoughData", "array length exceeds the input");
			}
			const out = new $a($(&count));
			for (let i = 0; i < $(&count); i++) {
				out[i] = $read;
			}
			return out;
		},
		None => quote! {
			const out: Ty = [];
			for (let i = 0; i < $(&count); i++) {
				out.push($read);
			}
			return out;
		},
	}
}

pub fn read_count(ctx: &Ctx, expr: Tokens) -> Tokens {
	let perror = &pds(ctx.names, "PdsError");
	quote! {
		const count = Number($expr);
		if (!Number.isSafeInteger(count) || count < 0) {
			throw new $perror("InvalidValue", "invalid length");
		}
	}
}

pub fn from_list(ctx: &Ctx, value: TypeId) -> Tokens {
	match typed_array(ctx.names, value) {
		Some(a) => quote!($a.from(out)),
		None => quote!(out),
	}
}
