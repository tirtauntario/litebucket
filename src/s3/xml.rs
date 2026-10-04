//! Minimal, bounded XML for S3 control bodies.
//!
//! The reader accepts elements, attributes (ignored), text, CDATA, comments,
//! and the XML declaration. DTDs and entity declarations are rejected, so
//! external entities cannot be resolved. Depth and element count are capped.

use super::error::S3Error;

pub const S3_NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

pub struct Limits {
    pub max_depth: usize,
    pub max_elements: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 16,
            max_elements: 50_000,
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    elements: usize,
    limits: Limits,
}

fn malformed() -> S3Error {
    S3Error::malformed_xml()
}

pub fn parse(bytes: &[u8]) -> Result<Element, S3Error> {
    parse_with(bytes, Limits::default())
}

pub fn parse_with(bytes: &[u8], limits: Limits) -> Result<Element, S3Error> {
    let s = std::str::from_utf8(bytes).map_err(|_| malformed())?;
    let s = s.strip_prefix('\u{FEFF}').unwrap_or(s);
    let mut p = Parser {
        s,
        pos: 0,
        elements: 0,
        limits,
    };
    p.skip_misc()?;
    if p.rest().starts_with("<?xml") {
        let end = p.rest().find("?>").ok_or_else(malformed)?;
        p.pos += end + 2;
    }
    p.skip_misc()?;
    let root = p.element(0)?;
    p.skip_misc()?;
    if p.pos != p.s.len() {
        return Err(malformed());
    }
    Ok(root)
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }

    fn skip_ws(&mut self) {
        let r = self.rest();
        let trimmed = r.trim_start_matches([' ', '\t', '\r', '\n']);
        self.pos += r.len() - trimmed.len();
    }

    fn skip_misc(&mut self) -> Result<(), S3Error> {
        loop {
            self.skip_ws();
            let r = self.rest();
            if r.starts_with("<!--") {
                let end = r[4..].find("-->").ok_or_else(malformed)?;
                self.pos += 4 + end + 3;
            } else if r.starts_with("<!") {
                // DOCTYPE / ENTITY declarations are never accepted.
                return Err(malformed());
            } else if r.starts_with("<?") && !r.starts_with("<?xml ") {
                let end = r.find("?>").ok_or_else(malformed)?;
                self.pos += end + 2;
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, S3Error> {
        let r = self.rest();
        let end = r
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/' || c == '=')
            .unwrap_or(r.len());
        if end == 0 {
            return Err(malformed());
        }
        let raw = &r[..end];
        self.pos += end;
        // Drop any namespace prefix.
        Ok(raw.rsplit(':').next().unwrap_or(raw).to_string())
    }

    fn element(&mut self, depth: usize) -> Result<Element, S3Error> {
        if depth >= self.limits.max_depth {
            return Err(malformed());
        }
        self.elements += 1;
        if self.elements > self.limits.max_elements {
            return Err(malformed());
        }
        if !self.rest().starts_with('<') {
            return Err(malformed());
        }
        self.pos += 1;
        let name = self.name()?;
        // Attributes (ignored, but must be well-formed).
        loop {
            self.skip_ws();
            let r = self.rest();
            if r.starts_with("/>") {
                self.pos += 2;
                return Ok(Element {
                    name,
                    ..Default::default()
                });
            }
            if r.starts_with('>') {
                self.pos += 1;
                break;
            }
            self.name()?;
            self.skip_ws();
            if !self.rest().starts_with('=') {
                return Err(malformed());
            }
            self.pos += 1;
            self.skip_ws();
            let q = self.rest().chars().next().ok_or_else(malformed)?;
            if q != '"' && q != '\'' {
                return Err(malformed());
            }
            self.pos += 1;
            let end = self.rest().find(q).ok_or_else(malformed)?;
            if self.rest()[..end].contains('<') {
                return Err(malformed());
            }
            self.pos += end + 1;
        }
        let mut el = Element {
            name,
            ..Default::default()
        };
        loop {
            let r = self.rest();
            if r.is_empty() {
                return Err(malformed());
            }
            if let Some(after) = r.strip_prefix("</") {
                let end = after.find('>').ok_or_else(malformed)?;
                let close = after[..end].trim_end();
                let close = close.rsplit(':').next().unwrap_or(close);
                if close != el.name {
                    return Err(malformed());
                }
                self.pos += 2 + end + 1;
                if !el.children.is_empty() && !el.text.trim().is_empty() {
                    return Err(malformed());
                }
                if !el.children.is_empty() {
                    el.text.clear();
                }
                return Ok(el);
            } else if r.starts_with("<!--") {
                let end = r[4..].find("-->").ok_or_else(malformed)?;
                self.pos += 4 + end + 3;
            } else if let Some(after) = r.strip_prefix("<![CDATA[") {
                let end = after.find("]]>").ok_or_else(malformed)?;
                el.text.push_str(&after[..end]);
                self.pos += 9 + end + 3;
            } else if r.starts_with("<!") || r.starts_with("<?") {
                return Err(malformed());
            } else if r.starts_with('<') {
                let child = self.element(depth + 1)?;
                el.children.push(child);
            } else {
                let end = r.find('<').unwrap_or(r.len());
                decode_text(&r[..end], &mut el.text)?;
                self.pos += end;
            }
        }
    }
}

fn decode_text(raw: &str, out: &mut String) -> Result<(), S3Error> {
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').ok_or_else(malformed)?;
        let ent = &after[..end];
        let c = match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(h) = ent.strip_prefix("#x") {
                    u32::from_str_radix(h, 16).map_err(|_| malformed())?
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse::<u32>().map_err(|_| malformed())?
                } else {
                    return Err(malformed());
                };
                let c = char::from_u32(code).ok_or_else(malformed)?;
                if !crate::keys::is_xml_char(c) {
                    return Err(malformed());
                }
                c
            }
        };
        out.push(c);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(())
}

/// Escape text for element content.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

pub struct XmlWriter {
    buf: String,
}

impl Default for XmlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl XmlWriter {
    pub fn new() -> Self {
        Self {
            buf: String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
        }
    }

    /// Root element with the S3 namespace.
    pub fn root(&mut self, name: &str) -> &mut Self {
        self.buf.push_str(&format!("<{name} xmlns=\"{S3_NS}\">"));
        self
    }

    pub fn open(&mut self, name: &str) -> &mut Self {
        self.buf.push('<');
        self.buf.push_str(name);
        self.buf.push('>');
        self
    }

    pub fn close(&mut self, name: &str) -> &mut Self {
        self.buf.push_str("</");
        self.buf.push_str(name);
        self.buf.push('>');
        self
    }

    pub fn elem(&mut self, name: &str, text: &str) -> &mut Self {
        self.open(name);
        self.buf.push_str(&escape(text));
        self.close(name)
    }

    pub fn opt(&mut self, name: &str, text: Option<&str>) -> &mut Self {
        if let Some(t) = text {
            self.elem(name, t);
        }
        self
    }

    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_bodies() {
        let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Quiet>true</Quiet>
  <Object><Key>a&amp;b&#x41;&#13;</Key></Object>
  <!-- comment -->
  <Object><Key><![CDATA[x<y]]></Key></Object>
</Delete>"#;
        let el = parse(doc).unwrap();
        assert_eq!(el.name, "Delete");
        assert_eq!(el.child_text("Quiet"), Some("true"));
        let keys: Vec<_> = el
            .children_named("Object")
            .map(|o| o.child_text("Key").unwrap().to_string())
            .collect();
        assert_eq!(keys, vec!["a&bA\r", "x<y"]);
    }

    #[test]
    fn rejects_dtd_and_entities() {
        for bad in [
            &br#"<!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]><a>&e;</a>"#[..],
            b"<a>&e;</a>",
            b"<a><b></a>",
            b"<a>",
            b"<a></a><b></b>",
            b"<a>&#0;</a>",
            b"\xff\xfe",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn bounds_depth_and_count() {
        let deep = "<a>".repeat(100) + &"</a>".repeat(100);
        assert!(parse(deep.as_bytes()).is_err());
        let many = format!("<r>{}</r>", "<x/>".repeat(60_000));
        assert!(parse(many.as_bytes()).is_err());
    }

    #[test]
    fn writer_escapes() {
        let mut w = XmlWriter::new();
        w.root("R").elem("Key", "a<&>\"'\r\tb").close("R");
        let s = w.finish();
        assert!(s.contains("<Key>a&lt;&amp;&gt;&quot;&apos;&#13;\tb</Key>"));
        let back = parse(s.as_bytes()).unwrap();
        assert_eq!(back.child_text("Key"), Some("a<&>\"'\r\tb"));
    }
}
