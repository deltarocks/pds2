use super::*;
use peg::{error::ParseError, parser, str::LineCol};

parser! {
pub grammar pds_parser() for str {
rule traced<T>(e: rule<T>) -> T =
	&(input:$([_]*) {
		#[cfg(feature = "trace")]
		println!("[PEG_INPUT_START]\n{}\n[PEG_TRACE_START]", input);
	})
	e:e()? {?
		#[cfg(feature = "trace")]
		println!("[PEG_TRACE_STOP]");
		e.ok_or("")
	}
pub rule root() -> Block
	= traced(<root0()>);
rule root0() -> Block
	= attrs:(_ a:insideBlockAttr() {a})* _ stmts:statement()* {
		Block {
			attributes: attrs.iter().cloned().collect(),
			statements: stmts,
			root: true
		}
	}

rule statement() -> Statement
	= attrs:(_ s:statementAttr() _ {s})* _ items:valueChain() _ ";" _ {
		  let mut a = attrs;
		  let out = a.drain(..).collect();
		  Statement {
			  attributes: out,
			  items,
		  }
	  }
rule statementAttr() -> (Ident, Vec<Value>)
	= "@" name:baseIdent() val:valueChain() { (name, val) }
rule insideBlockAttr() -> (Ident, Vec<Value>)
	= "@!" name:baseIdent() val:valueChain() { (name, val) }

rule valueChain() -> Vec<Value>
	= value() ++ (_ "=>" _)

rule value() -> Value
	= s:string_block() { Value::String { is_block: true, string: s } }
	/ s:string_singleline() { Value::String { is_block: false, string: s } }
	/ "b\"" b:byteChar()* "\"" _ { Value::Bytes(b.concat()) }
	/ "0x" i:$(['0'..='9' | 'a'..='f' | 'A'..='F']+) { Value::Int(i128::from_str_radix(i, 16).unwrap()) }
	/ f:float() { Value::Float(Float(f)) }
	/ i:$("-"? ['0'..='9']+) { Value::Int(i.parse().unwrap()) }
	/ "[" _ items:(e:expr() _ {e}) ** ("," _) ("," _)? "]" _ { Value::List(items) }
	/ "(" _ fields:recordField() ** ("," _) ("," _)? ")" _ {?
		let mut seen = BTreeSet::new();
		if fields.iter().all(|(k, _)| seen.insert(k.clone())) {
			Ok(Value::Record(fields))
		} else {
			Err("record fields to be unique")
		}
	}
	/ i:item() { Value::Item(i) }
	/ r:reference() { Value::FieldRef(r) }

rule recordField() -> (Ident, Value)
	= k:baseIdent() ":" !":" _ v:expr() _ { (k, v) }
rule float() -> f64
	= n:$("-"? ['0'..='9']+ (("." ['0'..='9']+ exponent()?) / exponent())) {? n.parse().or(Err("float")) }
rule exponent()
	= ['e' | 'E'] ['+' | '-']? ['0'..='9']+
rule byteChar() -> Vec<u8>
	= "\\x" h:$(['0'..='9' | 'a'..='f' | 'A'..='F']*<2>) { vec![u8::from_str_radix(h, 16).unwrap()] }
	/ "\\n" { vec![b'\n'] }
	/ "\\r" { vec![b'\r'] }
	/ "\\t" { vec![b'\t'] }
	/ "\\0" { vec![0] }
	/ "\\\\" { vec![b'\\'] }
	/ "\\\"" { vec![b'"'] }
	/ !['"' | '\\' | '\n'] c:$[_] { c.as_bytes().to_vec() }

rule item() -> Item
	= ident:item_path() args:itemArgs()? block:itemBlock()? {
		  Item {
			  name: ident,
			  args: args.unwrap_or_default(),
			  block: block.unwrap_or_else(|| Block { attributes: Default::default(), statements: vec![], root: false }),
		  }
	  }
rule itemArgs() -> ItemArgs
	= "(" _ args:(itemArg() ** (_ "," _)) ("," _)? ")" _ {?
		let mut out = ItemArgs::default();
		for (k, v) in args {
			if let Some(k) = k {
				if out.named.insert(k, v).is_some() {
					return Err("named arguments to be unique");
				}
			} else {
				out.unnamed.push(v)
			}
		}
		Ok(out)
	}
rule itemArg() -> (Option<Ident>, Value)
	= tag:(t:baseIdent() ":" !":" {t})? _ val:expr() _ { (tag, val) }
pub rule expr_root() -> Value
	= _ e:expr() _ { e }
rule expr() -> Value = precedence!{
	x:(@) _ "+" _ y:@ { Value::Binary(BinOp::Add, Box::new(x), Box::new(y)) }
	x:(@) _ "-" _ y:@ { Value::Binary(BinOp::Sub, Box::new(x), Box::new(y)) }
	--
	x:(@) _ "*" _ y:@ { Value::Binary(BinOp::Mul, Box::new(x), Box::new(y)) }
	x:(@) _ "/" _ y:@ { Value::Binary(BinOp::Div, Box::new(x), Box::new(y)) }
	--
	"(" _ e:expr() _ ")" { e }
	v:value() { v }
}
rule itemBlock() -> Block
	= "{" attrs:(_ a:insideBlockAttr() _ {a})* s:statement()* _ "}" _ {
		Block {
			attributes: attrs.iter().cloned().collect(),
			statements: s,
			root: false,
		}
	}

rule string_block() -> String
	= "\"\"\"\n" s:$((!"\"\"\"\n" [_])+) "\"\"\"\n" { s.trim().to_string() }
rule string_singleline() -> String
	= quiet! { "\"" chars:stringChar()* "\"" _ { chars.into_iter().collect() } }
	/ expected!("string")
rule stringChar() -> char
	= "\\\\" { '\\' }
	/ "\\\"" { '"' }
	/ "\\n" { '\n' }
	/ "\\r" { '\r' }
	/ "\\t" { '\t' }
	/ "\\0" { '\0' }
	/ "\\u{" h:$(['0'..='9' | 'a'..='f' | 'A'..='F']*<1,6>) "}" {?
		u32::from_str_radix(h, 16).ok().and_then(char::from_u32).ok_or("unicode scalar value")
	}
	/ !['"' | '\n' | '\\'] c:$[_] { c.chars().next().unwrap() }
pub rule reference_segment() -> FieldRefSegment
	= "@" id:baseIdent() { FieldRefSegment::Virtual(id) }
	/ "$" id:baseIdent() { FieldRefSegment::Arg(id) }
	/ ignore:"."? id: baseIdent() { FieldRefSegment::Down{to:id, ignore_delegation: ignore.is_some()} }
pub rule reference() -> FieldRef
	= ups:(p:"../"+ {p.len()} / "./" {0}) path: reference_segment() ++ ("/") {
		FieldRef {
			up: ups,
			path: BaseFieldRef(path),
		}
	}

pub rule item_path() -> Path
	= quiet! { absolute:("root::")? path:baseIdent()++("::") {
		let mut path = path.clone();
		let item = path.pop().unwrap();
		Path {
		absolute: absolute.is_some(),
		path,
		item,
	} } }
	/ expected!("namespaced identifier")
rule baseIdent() -> Ident
	= quiet! { chars:$(['0'..='9' | 'a'..='z' | 'A'..='Z' | '_']+) _ { Ident(chars.into())} }
	/ expected!("identifier")

rule _ = quiet! { (whitespace() / eol())* }

rule whitespace()
	= [' ' | '\t']
rule eolChar()
	= ['\n' | '\r']
rule eol()
	= "\n"
	/ "\r\n"
}
}

pub fn parse(input: &str) -> Result<Block, ParseError<LineCol>> {
	pds_parser::root(input)
}
pub fn parse_reference(input: &str) -> Result<FieldRef, ParseError<LineCol>> {
	pds_parser::reference(input)
}
pub fn parse_expr(input: &str) -> Result<Value, ParseError<LineCol>> {
	pds_parser::expr_root(input)
}
