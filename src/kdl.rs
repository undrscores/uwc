//! Parser for the subset of KDL (https://kdl.dev) that uwm and uwc configs use. A port of uwm's
//! `kdl.h`, so both compositors accept exactly the same files (the `kdl` crate rejects some of
//! them: `{ name "x" }` with no `;` before the brace, for one).
//!
//! ```text
//! node-name arg1 "arg 2" key=value key2=1.5 { child-node; child-node }
//! ```
//!
//! Supported: nodes separated by newlines or `;`, children blocks, string / number / bool / null
//! values, `key=value` properties, quoted or bare identifiers, `//` and nested block comments, `/-`
//! to comment out a node or an entry, `\` line continuation. Not supported: type annotations, raw
//! strings, multi-line strings. A bare word as a value is read as a string.

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

impl Val {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub line: usize,
    pub args: Vec<Val>,
    pub props: Vec<(String, Val)>,
    pub kids: Vec<Node>,
}

impl Node {
    pub fn arg_str(&self, i: usize) -> Option<&str> {
        self.args.get(i).and_then(Val::as_str)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    p: usize,
    line: usize,
}

type R<T> = Result<T, String>;

pub fn parse(src: &str) -> R<Vec<Node>> {
    let mut p = Parser { s: src.as_bytes(), p: 0, line: 1 };
    p.nodes(false).map_err(|m| format!("line {}: {m}", p.line))
}

impl Parser<'_> {
    fn at(&self, off: usize) -> u8 {
        self.s.get(self.p + off).copied().unwrap_or(0)
    }

    /// Skip spaces, tabs, comments and line continuations; stops before a newline.
    fn skipws(&mut self) -> R<()> {
        loop {
            let c = self.at(0);
            if c == b' ' || c == b'\t' || c == b'\r' {
                self.p += 1;
            } else if c == b'/' && self.at(1) == b'/' {
                while self.at(0) != 0 && self.at(0) != b'\n' {
                    self.p += 1;
                }
            } else if c == b'/' && self.at(1) == b'*' {
                let mut depth = 1;
                self.p += 2;
                while depth > 0 && self.at(0) != 0 {
                    if self.at(0) == b'/' && self.at(1) == b'*' {
                        depth += 1;
                        self.p += 2;
                    } else if self.at(0) == b'*' && self.at(1) == b'/' {
                        depth -= 1;
                        self.p += 2;
                    } else {
                        if self.at(0) == b'\n' {
                            self.line += 1;
                        }
                        self.p += 1;
                    }
                }
                if depth > 0 {
                    return Err("unterminated /* comment".into());
                }
            } else if c == b'\\' {
                let mut q = 1;
                while matches!(self.at(q), b' ' | b'\t' | b'\r') {
                    q += 1;
                }
                if self.at(q) != b'\n' {
                    return Err("stray backslash".into());
                }
                self.p += q + 1;
                self.line += 1;
            } else {
                return Ok(());
            }
        }
    }

    fn is_delim(&self) -> bool {
        let c = self.at(0);
        self.p >= self.s.len() || b" \t\r\n;{}()=\"\\".contains(&c) || (c == b'/' && matches!(self.at(1), b'/' | b'*'))
    }

    /// A quoted string; at the opening quote.
    fn quoted(&mut self) -> R<String> {
        let mut out = Vec::new();
        self.p += 1;
        while self.p < self.s.len() && self.at(0) != b'"' {
            let mut c = self.at(0);
            self.p += 1;
            if c == b'\n' {
                self.line += 1;
            }
            if c == b'\\' {
                c = self.at(0);
                self.p += 1;
                c = match c {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'0' => 0,
                    b'\\' | b'"' | b'/' => c,
                    _ => return Err("unknown escape in string".into()),
                };
            }
            out.push(c);
        }
        if self.at(0) != b'"' {
            return Err("unterminated string".into());
        }
        self.p += 1;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// A bare identifier or word.
    fn bare(&mut self) -> R<String> {
        let start = self.p;
        while !self.is_delim() {
            self.p += 1;
        }
        if self.p == start {
            return Err("unexpected character".into());
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.p]).into_owned())
    }

    fn value(&mut self) -> R<Val> {
        if self.at(0) == b'"' {
            return Ok(Val::Str(self.quoted()?));
        }
        let w = self.bare()?;
        Ok(match w.as_str() {
            "true" | "#true" => Val::Bool(true),
            "false" | "#false" => Val::Bool(false),
            "null" | "#null" => Val::Null,
            _ => {
                let b = w.as_bytes();
                let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
                let numeric = digit(0) || (matches!(b[0], b'-' | b'+' | b'.') && (digit(1) || b.get(1) == Some(&b'.')));
                match numeric.then(|| w.parse::<f64>().ok()).flatten() {
                    Some(n) => Val::Num(n),
                    None => Val::Str(w),
                }
            }
        })
    }

    /// One node, at its name.
    fn node(&mut self) -> R<Node> {
        let mut node = Node { line: self.line, ..Default::default() };
        node.name = if self.at(0) == b'"' { self.quoted()? } else { self.bare()? };
        loop {
            self.skipws()?;
            if matches!(self.at(0), 0 | b'\n' | b';' | b'}') {
                break;
            }
            let mut slash = false;
            if self.at(0) == b'/' && self.at(1) == b'-' {
                slash = true;
                self.p += 2;
                self.skipws()?;
            }
            if self.at(0) == b'{' {
                self.p += 1;
                let kids = self.nodes(true)?;
                if !slash {
                    node.kids.extend(kids);
                }
                continue;
            }
            if self.at(0) == b'(' {
                return Err("type annotations are not supported".into());
            }
            // A property (`key=value`) or an argument.
            let (save, line) = (self.p, self.line);
            let key = if self.at(0) == b'"' { self.quoted()? } else { self.bare()? };
            if self.at(0) == b'=' {
                self.p += 1;
                let v = self.value()?;
                if !slash {
                    node.props.push((key, v));
                }
                continue;
            }
            self.p = save;
            self.line = line;
            let v = self.value()?;
            if !slash {
                node.args.push(v);
            }
        }
        Ok(node)
    }

    /// Nodes up to the closing brace (in a block) or the end of the input.
    fn nodes(&mut self, in_block: bool) -> R<Vec<Node>> {
        let mut out = Vec::new();
        loop {
            loop {
                self.skipws()?;
                match self.at(0) {
                    b'\n' => {
                        self.line += 1;
                        self.p += 1;
                    }
                    b';' => self.p += 1,
                    _ => break,
                }
            }
            if self.p >= self.s.len() {
                return if in_block { Err("missing closing }".into()) } else { Ok(out) };
            }
            if self.at(0) == b'}' {
                if !in_block {
                    return Err("unexpected }".into());
                }
                self.p += 1;
                return Ok(out);
            }
            let mut discard = false;
            if self.at(0) == b'/' && self.at(1) == b'-' {
                discard = true;
                self.p += 2;
                self.skipws()?;
            }
            let n = self.node()?;
            if !discard {
                out.push(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_args_props_children() {
        let n = parse("a 1 \"two\" true k=3.5 { b; c \"x\" }\nd").unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].args, vec![Val::Num(1.0), Val::Str("two".into()), Val::Bool(true)]);
        assert_eq!(n[0].props, vec![("k".into(), Val::Num(3.5))]);
        assert_eq!(n[0].kids.len(), 2);
        assert_eq!(n[0].kids[1].arg_str(0), Some("x"));
        assert_eq!(n[1].name, "d");
    }

    #[test]
    fn last_child_needs_no_semicolon() {
        // The form uwm configs use and the `kdl` crate rejects.
        let n = parse("vars { accent \"#00aaff\" }").unwrap();
        assert_eq!(n[0].kids[0].name, "accent");
    }

    #[test]
    fn comments_and_slashdash() {
        let n = parse("// c\n/* a /* nested */ b */ a 1 /-2 3\n/-gone 1\nb /-{ x }").unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].args, vec![Val::Num(1.0), Val::Num(3.0)]);
        assert!(n[1].kids.is_empty());
    }

    #[test]
    fn key_combos_are_bare_names() {
        let n = parse("Mod+Shift+1 { send-to-tag 1; }").unwrap();
        assert_eq!(n[0].name, "Mod+Shift+1");
        assert_eq!(n[0].kids[0].args, vec![Val::Num(1.0)]);
    }

    #[test]
    fn errors_have_lines() {
        for (src, want) in [
            ("a {", "missing closing }"),
            ("a }", "unexpected }"),
            ("a \"x", "unterminated string"),
            ("a (u8)1", "type annotations"),
            ("\n\na \"\\q\"", "line 3"),
        ] {
            let e = parse(src).unwrap_err();
            assert!(e.contains(want), "{src:?} -> {e}");
        }
    }

    #[test]
    fn continuation_and_escapes() {
        let n = parse("a 1 \\\n  2 \"t\\tq\\\"\"").unwrap();
        assert_eq!(n[0].args, vec![Val::Num(1.0), Val::Num(2.0), Val::Str("t\tq\"".into())]);
    }
}
