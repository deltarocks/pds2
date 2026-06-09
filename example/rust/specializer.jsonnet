local
	str(s) = {Str: s},
	cast(pds, load, Ty) = [str("("), str(load), str(" as "), str(pds.tyTy(Ty)), str(")")],
	foreach(pds, load, cb) = [str("for _value in "), str(load), str(".iter()"), str(" {"), INDENT, NL, cb("_value"), DEDENT, NL, str("}")],
	NL = 'NL',
	INDENT = 'INDENT',
	DEDENT = 'DEDENT'
;

{
	specializer: {
		mymap(pds, Size, Key, Value): {
			ty: [str("std::collections::HashMap"), str("<"), str(pds.tyTy(Key)), str(", "), str(pds.tyTy(Value)), str(">")],
			read: [str("Ok(todo!())")],
			write: [
				str(pds.tyWrite(Size)), str("(&"), cast(pds, "_0.len()", Size), str(", output)?;"), NL,
				foreach(pds, '_0', function(v) [
					str(pds.tyWrite(Key)), str("(&"), str(v), str(".0"), str(", output"), str(")?;"), NL,
					str(pds.tyWrite(Value)), str("(&"), str(v), str(".1"), str(", output"), str(")?;"),
				]), NL,
				str("Ok(())"),
			],
			size: [
				str("let mut size = "), str(pds.tySize(Size)), str("(&"), cast(pds, "_0.len()", Size), str(");"),
				foreach(pds, '_0', function(v) [
					str("size += "), str(pds.tySize(Key)), str("(&"), str(v), str(".0"), str(");"), NL,
					str("size += "), str(pds.tySize(Value)), str("(&"), str(v), str(".1"), str(");"),
				]), NL,
				str("size")
			],
		},
		parray(pds, Size, Value): {
			ty: [str("Vec"), str("<"), str(pds.tyTy(Value)), str(">")],
			read: [str("todo!()")],
			write: [str("todo!()")],
			size: [str("todo!()")],
		},
	},
}
