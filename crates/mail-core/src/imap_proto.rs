//! IMAP wire-format helpers: a tokenizer for response data, string quoting
//! and modified UTF-7 mailbox names (RFC 3501 section 5.1.3).

use crate::b64;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Atom(String),
    Str(Vec<u8>),
    List(Vec<Tok>),
    Nil,
}

impl Tok {
    pub fn as_string(&self) -> Option<String> {
        match self {
            Tok::Atom(s) => Some(s.clone()),
            Tok::Str(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Tok::Atom(s) => Some(s.as_bytes().to_vec()),
            Tok::Str(b) => Some(b.clone()),
            _ => None,
        }
    }
}

fn proto(msg: &str) -> Error {
    Error::Protocol(msg.to_string())
}

/// Tokenizes an untagged response (without the leading "* ").
/// Literals (`{n}\r\n` + n bytes) must be inlined in `input`.
pub fn parse_tokens(input: &[u8]) -> Result<Vec<Tok>> {
    Parser { s: input, i: 0, depth: 0 }.seq(false)
}

/// Real responses nest a few levels (BODYSTRUCTURE is the deepest); a hostile
/// server must not be able to exhaust the stack.
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn seq(&mut self, in_list: bool) -> Result<Vec<Tok>> {
        let mut out = Vec::new();
        loop {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\r' | b'\n') {
                self.i += 1;
            }
            if self.i >= self.s.len() {
                return if in_list { Err(proto("unterminated list")) } else { Ok(out) };
            }
            match self.s[self.i] {
                b'(' => {
                    self.i += 1;
                    self.depth += 1;
                    if self.depth > MAX_DEPTH {
                        return Err(proto("response nested too deeply"));
                    }
                    let l = self.seq(true)?;
                    self.depth -= 1;
                    out.push(Tok::List(l));
                }
                b')' => {
                    if !in_list {
                        return Err(proto("unbalanced parenthesis"));
                    }
                    self.i += 1;
                    return Ok(out);
                }
                b'"' => out.push(Tok::Str(self.quoted()?)),
                b'{' => out.push(Tok::Str(self.literal()?)),
                _ => {
                    let a = self.atom();
                    out.push(if a.eq_ignore_ascii_case("NIL") { Tok::Nil } else { Tok::Atom(a) });
                }
            }
        }
    }

    fn quoted(&mut self) -> Result<Vec<u8>> {
        self.i += 1;
        let mut v = Vec::new();
        while self.i < self.s.len() {
            let c = self.s[self.i];
            self.i += 1;
            match c {
                b'\\' => {
                    if self.i < self.s.len() {
                        v.push(self.s[self.i]);
                        self.i += 1;
                    }
                }
                b'"' => return Ok(v),
                _ => v.push(c),
            }
        }
        Err(proto("unterminated quoted string"))
    }

    fn literal(&mut self) -> Result<Vec<u8>> {
        let close = self.s[self.i..]
            .iter()
            .position(|&b| b == b'}')
            .ok_or_else(|| proto("unterminated literal"))?
            + self.i;
        let n: usize = std::str::from_utf8(&self.s[self.i + 1..close])
            .ok()
            .map(|t| t.trim_end_matches('+'))
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| proto("bad literal length"))?;
        let mut j = close + 1;
        if self.s.get(j) == Some(&b'\r') {
            j += 1;
        }
        if self.s.get(j) == Some(&b'\n') {
            j += 1;
        }
        if j + n > self.s.len() {
            return Err(proto("literal shorter than announced"));
        }
        self.i = j + n;
        Ok(self.s[j..j + n].to_vec())
    }

    /// Atoms may contain bracketed sections with spaces, e.g.
    /// `BODY[HEADER.FIELDS (FROM TO)]`.
    fn atom(&mut self) -> String {
        let start = self.i;
        let mut depth = 0u32;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'[' => depth += 1,
                b']' => depth = depth.saturating_sub(1),
                b' ' | b'(' | b')' if depth == 0 => break,
                b'\r' | b'\n' => break,
                _ => {}
            }
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }
}

/// Quotes a string for use as an IMAP argument.
pub fn quote(s: &str) -> Result<String> {
    crate::net::check_line(s)?;
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('"');
    Ok(o)
}

pub fn utf7_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
        out.push('&');
        out.push_str(&b64::encode_with(bytes, b64::IMAP, false));
        out.push('-');
        pending.clear();
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

pub fn utf7_decode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(dash) = after.find('-') else {
            out.push_str(&rest[amp..]);
            return out;
        };
        let chunk = &after[..dash];
        if chunk.is_empty() {
            out.push('&');
        } else {
            match b64::decode_with(chunk, b64::IMAP) {
                Some(bytes) if bytes.len() % 2 == 0 => {
                    let units: Vec<u16> =
                        bytes.chunks(2).map(|p| u16::from_be_bytes([p[0], p[1]])).collect();
                    out.push_str(&String::from_utf16_lossy(&units));
                }
                _ => {
                    out.push('&');
                    out.push_str(chunk);
                    out.push('-');
                }
            }
        }
        rest = &after[dash + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_fetch_with_literal_and_sections() {
        let input = b"1 FETCH (UID 7 FLAGS (\\Seen $Junk) BODY[HEADER.FIELDS (FROM TO)] {5}\r\nhello RFC822.SIZE 99)";
        let t = parse_tokens(input).unwrap();
        assert_eq!(t[0], Tok::Atom("1".into()));
        let Tok::List(l) = &t[2] else { panic!() };
        assert_eq!(l[0], Tok::Atom("UID".into()));
        assert_eq!(l[3], Tok::List(vec![Tok::Atom("\\Seen".into()), Tok::Atom("$Junk".into())]));
        assert_eq!(l[4], Tok::Atom("BODY[HEADER.FIELDS (FROM TO)]".into()));
        assert_eq!(l[5], Tok::Str(b"hello".to_vec()));
        assert_eq!(l[7], Tok::Atom("99".into()));
    }

    #[test]
    fn tokenizes_list_response() {
        let t = parse_tokens(br#"LIST (\HasNoChildren \Sent) "/" "Sent \"Items\"""#).unwrap();
        assert_eq!(t[2], Tok::Str(b"/".to_vec()));
        assert_eq!(t[3], Tok::Str(b"Sent \"Items\"".to_vec()));
        let t = parse_tokens(b"LIST () NIL INBOX").unwrap();
        assert_eq!(t[2], Tok::Nil);
    }

    #[test]
    fn rejects_truncated_literal() {
        assert!(parse_tokens(b"1 FETCH (BODY[] {50}\r\nshort)").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote(r#"a"b\c"#).unwrap(), r#""a\"b\\c""#);
        assert!(quote("x\r\nA1 DELETE INBOX").is_err());
    }

    #[test]
    fn utf7_roundtrip() {
        for (plain, enc) in [
            ("Entwürfe", "Entw&APw-rfe"),
            ("日本語", "&ZeVnLIqe-"),
            ("A&B", "A&-B"),
            ("Inbox", "Inbox"),
            ("Caixa de Saída", "Caixa de Sa&AO0-da"),
        ] {
            assert_eq!(utf7_encode(plain), enc, "encode {plain}");
            assert_eq!(utf7_decode(enc), plain, "decode {enc}");
        }
    }
}
