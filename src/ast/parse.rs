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
    = _ stmts:statement()* { Block { statements: stmts, root: true } }

rule statement() -> Statement
    = attrs:(_ s:statementAttr() _ {s})* _ items:valueChain() _ ";" _ {
          let mut a = attrs;
          let out = a.drain(..).collect();
          Statement {
              attributes: out,
              items: items,
          }
      }
rule statementAttr() -> (Ident, Vec<Value>)
    = "@" name:baseIdent() val:valueChain() { (name, val) }

rule valueChain() -> Vec<Value>
    = value() ++ (_ "=>" _)

rule value() -> Value
    = s:string_block() { Value::String { is_block: true, string: s } }
    / s:string_singleline() { Value::String { is_block: false, string: s } }
    / i:$(['0'..='9']+) { Value::Int(i.parse().unwrap()) }
    / i:item() { Value::Item(i) }
    / r:reference() { Value::Reference(r) }

rule item() -> Item
    = ident:item_path() args:itemArgs()? block:itemBlock()? {
          Item {
              name: ident,
              args: args.unwrap_or_else(|| ItemArgs::default()),
              block: block.unwrap_or_else(|| Block { statements: vec![], root: false }),
          }
      }
rule itemArgs() -> ItemArgs
    = "(" args:(itemArg() ** (_ "," _)) ")" _ {
        let mut out = ItemArgs::default();
        for (k, v) in args {
            if let Some(k) = k {
                out.named.insert(k, v);
            } else {
                out.unnamed.push(v)
            }
        }
        out
    }
rule itemArg() -> (Option<Ident>, Value)
    = tag:(t:baseIdent() ":" {t})? _ val:value() _ { (tag, val) }
rule itemBlock() -> Block
    = "{" s:statement()* "}" _ { Block { statements: s, root: false } }

rule string_block() -> String
    = "\"\"\"\n" s:$((!"\"\"\"\n" [_])+) "\"\"\"\n" { s.trim().to_string() }
rule string_singleline() -> String
    = quiet! { "\"" chars:stringChar()* "\"" _ { chars.into_iter().collect() } }
    / expected!("string")
rule stringChar() -> char
    = !['"' | '\n'] c:$[_] { c.chars().next().unwrap() }
pub rule reference_segment() -> ReferenceSegment
    = "@" id:baseIdent() { ReferenceSegment::Virtual(id) }
    / "$" id:baseIdent() { ReferenceSegment::Arg(id) }
    / ignore:"."? id: baseIdent() { ReferenceSegment::Down{to:id, ignore_delegation: ignore.is_some()} }
pub rule reference() -> Reference
    = ups:(p:"../"+ {p.len()} / "./" {0}) path: reference_segment() ++ ("/") {
        Reference {
            up: ups,
            path: BaseReference(path),
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
    pds_parser::root(&input)
}
pub fn parse_reference(input: &str) -> Result<Reference, ParseError<LineCol>> {
    pds_parser::reference(input)
}
