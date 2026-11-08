//! Tiny, tolerant XML reader for WebDAV/CalDAV multistatus responses.
//! Namespace prefixes are dropped (element and attribute names are matched by
//! local name). No DTDs or external entities, by design.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// First direct child with this local name.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// First descendant (depth-first) with this local name.
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(f) = c.find(name) {
                return Some(f);
            }
        }
        None
    }

    /// All descendants with this local name.
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.find_all(name, out);
        }
    }

    pub fn all(&self, name: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        self.find_all(name, &mut out);
        out
    }

    /// Text of the first descendant with this name, trimmed.
    pub fn text_of(&self, name: &str) -> Option<String> {
        self.find(name).map(|n| n.text.trim().to_string())
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find(';') {
            Some(end) if end <= 10 => {
                let ent = &after[..end];
                let ch = match ent {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                    _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                    _ => None,
                };
                match ch {
                    Some(c) => {
                        out.push(c);
                        rest = &after[end + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = after;
                    }
                }
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `a="1" b='2'` -> [(a, 1), (b, 2)]; namespace declarations are skipped.
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let Some(end) = after[1..].find(q) else { break };
        let value = decode_entities(&after[1..1 + end]);
        if !key.starts_with("xmlns") {
            out.push((local(key), value));
        }
        rest = after[1 + end + 1..].trim_start();
    }
    out
}

/// Parses a document and returns its root element.
pub fn parse(input: &str) -> Result<Node> {
    let bad = |m: &str| Error::Protocol(format!("XML: {m}"));
    let mut stack: Vec<Node> = vec![Node { name: "#document".into(), ..Default::default() }];
    let mut rest = input;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            stack.last_mut().unwrap().text.push_str(&decode_entities(rest));
            break;
        };
        if lt > 0 {
            stack.last_mut().unwrap().text.push_str(&decode_entities(&rest[..lt]));
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or_else(|| bad("unterminated CDATA"))?;
            stack.last_mut().unwrap().text.push_str(&r[..end]);
            rest = &r[end + 3..];
        } else if let Some(r) = rest.strip_prefix("<!--") {
            let end = r.find("-->").ok_or_else(|| bad("unterminated comment"))?;
            rest = &r[end + 3..];
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = rest.find('>').ok_or_else(|| bad("unterminated declaration"))?;
            rest = &rest[end + 1..];
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or_else(|| bad("unterminated end tag"))?;
            let name = local(r[..end].trim());
            rest = &r[end + 1..];
            if stack.len() < 2 || stack.last().unwrap().name != name {
                return Err(bad(&format!("unexpected </{name}>")));
            }
            let node = stack.pop().unwrap();
            stack.last_mut().unwrap().children.push(node);
        } else {
            // Start tag; find its end while respecting quoted attribute values.
            let bytes = rest.as_bytes();
            let mut i = 1;
            let mut quote = 0u8;
            while i < bytes.len() {
                let c = bytes[i];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                i += 1;
            }
            if i >= bytes.len() {
                return Err(bad("unterminated start tag"));
            }
            let inner = &rest[1..i];
            let self_closing = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let tag = inner.split(|c: char| c.is_whitespace()).next().unwrap_or("");
            let name = local(tag);
            if name.is_empty() {
                return Err(bad("empty tag name"));
            }
            let attrs = parse_attrs(&inner[tag.len()..]);
            rest = &rest[i + 1..];
            let node = Node { name, attrs, ..Default::default() };
            if self_closing {
                stack.last_mut().unwrap().children.push(node);
            } else {
                stack.push(node);
            }
            if stack.len() > 256 {
                return Err(bad("nesting too deep"));
            }
        }
    }
    if stack.len() != 1 {
        return Err(bad("unclosed elements"));
    }
    stack.pop().unwrap().children.into_iter().next().ok_or_else(|| bad("empty document"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multistatus() {
        let doc = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/work/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Work &amp; stuff</d:displayname>
      <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/><cal:comp name='VTODO'/></cal:supported-calendar-component-set>
      <cal:calendar-data><![CDATA[BEGIN:VCALENDAR
END:VCALENDAR]]></cal:calendar-data>
    </d:prop></d:propstat>
  </d:response>
  <!-- comment -->
</d:multistatus>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "multistatus");
        let r = &root.all("response")[0];
        assert_eq!(r.text_of("href").unwrap(), "/cal/work/");
        assert_eq!(r.text_of("displayname").unwrap(), "Work & stuff");
        assert!(r.find("resourcetype").unwrap().child("calendar").is_some());
        assert!(r.text_of("calendar-data").unwrap().starts_with("BEGIN:VCALENDAR"));
        let comps: Vec<_> = r.all("comp").iter().filter_map(|c| c.attr("name")).collect();
        assert_eq!(comps, ["VEVENT", "VTODO"]);
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<a>").is_err());
        assert_eq!(decode_entities("&#233;&#x41;&bogus;"), "éA&bogus;");
    }
}
