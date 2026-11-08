//! Text inside attachments, for search: PDF, Word / PowerPoint / Excel
//! (.docx, .pptx, .xlsx) and their OpenDocument counterparts, plain text,
//! CSV and HTML. Written here with one decompressor (miniz_oxide, already
//! part of the app); best effort, bounded, never trusted: a damaged or
//! hostile file yields less text, not a crash or a memory blow-up.

use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};

/// Text kept per attachment.
const MAX_TEXT: usize = 200_000;
/// Files larger than this aren't read.
pub const MAX_FILE: usize = 30 << 20;
/// Decompressed bytes allowed per part, and per file (zip bombs).
const MAX_PART: usize = 20 << 20;
const MAX_TOTAL: usize = 60 << 20;

/// The searchable text of an attachment, if its type is one we read.
pub fn extract(name: &str, mime: &str, data: &[u8]) -> Option<String> {
    if data.len() > MAX_FILE {
        return None;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let mime = mime.to_ascii_lowercase();
    let text = match ext.as_str() {
        "pdf" => pdf_text(data),
        "docx" | "docm" | "dotx" => ooxml(data, |n| n == "word/document.xml" || (n.starts_with("word/") && (n.contains("header") || n.contains("footer") || n.contains("footnotes")) && n.ends_with(".xml")), &["</w:p>"]),
        "pptx" | "pptm" | "ppsx" => ooxml(data, |n| (n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/")) && n.ends_with(".xml"), &["</a:p>"]),
        "xlsx" | "xlsm" => ooxml(data, |n| n == "xl/sharedStrings.xml" || (n.starts_with("xl/worksheets/") && n.ends_with(".xml")), &["</si>", "</c>"]),
        "odt" | "odp" | "ods" | "odg" => ooxml(data, |n| n == "content.xml", &["</text:p>", "</text:h>"]),
        "txt" | "csv" | "md" | "log" | "ics" | "vcf" | "json" | "xml" => Some(String::from_utf8_lossy(data).into_owned()),
        "htm" | "html" => Some(crate::safety::strip_tags(&String::from_utf8_lossy(data))),
        _ if mime == "application/pdf" => pdf_text(data),
        _ if mime.starts_with("text/") => Some(String::from_utf8_lossy(data).into_owned()),
        _ => None,
    }?;
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(MAX_TEXT).collect())
}

// ---- zip (Office Open XML, OpenDocument) ------------------------------------------

/// Entries of a zip archive whose names `want` picks, decompressed.
fn zip_entries(data: &[u8], want: impl Fn(&str) -> bool) -> Vec<(String, Vec<u8>)> {
    let u16_at = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |i: usize| data.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    // End of central directory: the last "PK\x05\x06" (a comment may follow it).
    let Some(eocd) = (0..data.len().saturating_sub(21)).rev().take(70_000).find(|&i| data[i..].starts_with(b"PK\x05\x06")) else { return Vec::new() };
    let (Some(count), Some(mut at)) = (u16_at(eocd + 10), u32_at(eocd + 16)) else { return Vec::new() };
    let mut out = Vec::new();
    let mut total = 0usize;
    for _ in 0..count.min(5000) {
        if !data.get(at..).is_some_and(|d| d.starts_with(b"PK\x01\x02")) {
            break;
        }
        let (Some(method), Some(csize), Some(nlen), Some(xlen), Some(clen), Some(local)) =
            (u16_at(at + 10), u32_at(at + 20), u16_at(at + 28), u16_at(at + 30), u16_at(at + 32), u32_at(at + 42))
        else {
            break;
        };
        let name = String::from_utf8_lossy(data.get(at + 46..at + 46 + nlen).unwrap_or_default()).into_owned();
        at += 46 + nlen + xlen + clen;
        if !want(&name) || total >= MAX_TOTAL {
            continue;
        }
        let (Some(lnlen), Some(lxlen)) = (u16_at(local + 26), u16_at(local + 28)) else { continue };
        let start = local + 30 + lnlen + lxlen;
        let Some(raw) = data.get(start..start.saturating_add(csize)) else { continue };
        let body = match method {
            0 => raw.to_vec(),
            8 => match decompress_to_vec_with_limit(raw, MAX_PART.min(MAX_TOTAL - total)) {
                Ok(b) => b,
                Err(_) => continue,
            },
            _ => continue,
        };
        total += body.len();
        out.push((name, body));
    }
    // Slides and sheets in their natural order (slide2 before slide10).
    out.sort_by_key(|(n, _)| {
        let digits: String = n.chars().filter(char::is_ascii_digit).collect();
        (n.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_alphabetic()).to_string(), digits.parse::<u64>().unwrap_or(0), n.clone())
    });
    out
}

/// Text of the XML parts of an office document; `breaks` end paragraphs.
fn ooxml(data: &[u8], want: impl Fn(&str) -> bool, breaks: &[&str]) -> Option<String> {
    let parts = zip_entries(data, want);
    if parts.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (_, xml) in parts {
        let mut xml = String::from_utf8_lossy(&xml).into_owned();
        for b in breaks {
            xml = xml.replace(b, &format!("{b}\n"));
        }
        out.push_str(&xml_text(&xml));
        out.push('\n');
        if out.len() > MAX_TEXT * 2 {
            break;
        }
    }
    Some(out)
}

/// Character data of an XML document, entities decoded.
fn xml_text(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 3);
    let mut in_tag = false;
    for c in xml.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    crate::xml::decode_entities(&out)
}

// ---- PDF ------------------------------------------------------------------------------

/// Text shown by a PDF's content streams (Tj, TJ, ' and "), through the
/// fonts' ToUnicode maps where there are any. Compressed streams
/// (FlateDecode) are inflated; other filters are skipped.
fn pdf_text(data: &[u8]) -> Option<String> {
    if !data.starts_with(b"%PDF") {
        return None;
    }
    let mut streams = Vec::new();
    let mut total = 0usize;
    let mut i = 0;
    while let Some(pos) = find(&data[i..], b"stream") {
        let s = i + pos;
        i = s + 6;
        // "endstream" contains "stream": skip it, and require a line break after "stream".
        if s >= 3 && &data[s - 3..s] == b"end" {
            continue;
        }
        let body_start = match data.get(i..i + 2) {
            Some(b"\r\n") => i + 2,
            Some([b'\n', _]) | Some([b'\r', _]) => i + 1,
            _ => continue,
        };
        let Some(end) = find(&data[body_start..], b"endstream").map(|e| body_start + e) else { break };
        let dict = &data[s.saturating_sub(400)..s];
        let dict = &dict[dict.windows(2).rposition(|w| w == b"<<").unwrap_or(0)..];
        let raw = &data[body_start..end];
        let body = if find(dict, b"/FlateDecode").is_some() {
            match decompress_to_vec_zlib_with_limit(raw, MAX_PART.min(MAX_TOTAL.saturating_sub(total))) {
                Ok(b) => b,
                Err(_) => continue,
            }
        } else if find(dict, b"/Filter").is_some() {
            continue; // images and other encodings
        } else {
            raw.to_vec()
        };
        total += body.len();
        streams.push(body);
        i = end + 9;
        if total >= MAX_TOTAL {
            break;
        }
    }
    let mut cmap = std::collections::HashMap::<Vec<u8>, String>::new();
    for s in streams.iter().filter(|s| find(s, b"begincmap").is_some()) {
        read_cmap(s, &mut cmap);
    }
    let mut out = String::new();
    for s in streams.iter().filter(|s| find(s, b"begincmap").is_none()) {
        content_text(s, &cmap, &mut out);
        if out.len() > MAX_TEXT * 2 {
            break;
        }
    }
    Some(out)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn hex_bytes(s: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = s.iter().filter_map(|c| (*c as char).to_digit(16).map(|d| d as u8)).collect();
    digits.chunks(2).map(|p| p[0] << 4 | p.get(1).copied().unwrap_or(0)).collect()
}

fn utf16be(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks(2).map(|p| u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)])).collect();
    String::from_utf16_lossy(&units)
}

/// bfchar / bfrange entries of a ToUnicode CMap.
fn read_cmap(s: &[u8], map: &mut std::collections::HashMap<Vec<u8>, String>) {
    let text = String::from_utf8_lossy(s);
    // Entries are <hex> tokens in sequence (several may share a line).
    let hexes = |block: &str| -> Vec<Vec<u8>> { block.split('<').skip(1).filter_map(|p| p.split_once('>')).map(|(h, _)| hex_bytes(h.as_bytes())).collect() };
    for block in text.split("beginbfchar").skip(1) {
        for pair in hexes(block.split("endbfchar").next().unwrap_or("")).chunks_exact(2) {
            map.insert(pair[0].clone(), utf16be(&pair[1]));
        }
    }
    for block in text.split("beginbfrange").skip(1) {
        // Ranges whose target is an array ([<…> <…>]) are left out.
        let body = block.split("endbfrange").next().unwrap_or("");
        if body.contains('[') {
            continue;
        }
        for t in hexes(body).chunks_exact(3) {
            let (lo, hi, dst) = (&t[0], &t[1], &t[2]);
            if lo.len() != hi.len() || lo.is_empty() || lo.len() > 4 {
                continue;
            }
            let n = |b: &[u8]| b.iter().fold(0u32, |a, x| a << 8 | *x as u32);
            let (a, b) = (n(lo), n(hi));
            let base = n(dst);
            for (k, code) in (a..=b.min(a.saturating_add(0xffff))).enumerate() {
                let key = code.to_be_bytes()[4 - lo.len()..].to_vec();
                let target = if dst.len() == 2 { String::from_utf16_lossy(&[(base as usize + k) as u16]) } else { utf16be(dst) };
                map.insert(key, target);
            }
        }
    }
}

/// Decodes a string shown on the page: through the CMap (2-byte codes, then
/// 1-byte) when there is one, else as PDFDocEncoding (≈ Latin-1).
fn show(bytes: &[u8], cmap: &std::collections::HashMap<Vec<u8>, String>, out: &mut String) {
    if !cmap.is_empty() {
        let mut i = 0;
        let mut mapped = String::new();
        let mut hits = 0;
        while i < bytes.len() {
            if let Some(t) = bytes.get(i..i + 2).and_then(|k| cmap.get(k)) {
                mapped.push_str(t);
                hits += 1;
                i += 2;
            } else if let Some(t) = cmap.get(&bytes[i..i + 1]) {
                mapped.push_str(t);
                hits += 1;
                i += 1;
            } else {
                i += 1;
            }
        }
        if hits > 0 {
            out.push_str(&mapped);
            return;
        }
    }
    out.extend(bytes.iter().map(|&b| b as char));
}

/// The text operators of one content stream.
fn content_text(s: &[u8], cmap: &std::collections::HashMap<Vec<u8>, String>, out: &mut String) {
    let mut i = 0;
    let mut pending: Vec<Vec<u8>> = Vec::new();
    while i < s.len() {
        match s[i] {
            b'(' => {
                // Literal string with escapes and balanced parentheses.
                let mut depth = 1;
                let mut buf = Vec::new();
                i += 1;
                while i < s.len() && depth > 0 {
                    match s[i] {
                        b'\\' if i + 1 < s.len() => {
                            i += 1;
                            match s[i] {
                                b'n' => buf.push(b'\n'),
                                b'r' => buf.push(b'\r'),
                                b't' => buf.push(b'\t'),
                                b'b' | b'f' => {}
                                d @ b'0'..=b'7' => {
                                    let mut v = (d - b'0') as u32;
                                    for _ in 0..2 {
                                        if let Some(n @ b'0'..=b'7') = s.get(i + 1) {
                                            v = v * 8 + (n - b'0') as u32;
                                            i += 1;
                                        }
                                    }
                                    buf.push(v as u8);
                                }
                                b'\r' | b'\n' => {}
                                c => buf.push(c),
                            }
                        }
                        b'(' => {
                            depth += 1;
                            buf.push(b'(');
                        }
                        b')' => {
                            depth -= 1;
                            if depth > 0 {
                                buf.push(b')');
                            }
                        }
                        c => buf.push(c),
                    }
                    i += 1;
                }
                pending.push(buf);
            }
            b'<' if s.get(i + 1) != Some(&b'<') => {
                let end = s[i..].iter().position(|&c| c == b'>').map(|e| i + e).unwrap_or(s.len());
                pending.push(hex_bytes(&s[i + 1..end]));
                i = end + 1;
            }
            b'-' | b'0'..=b'9' if !pending.is_empty() => {
                // A large negative TJ adjustment is a word gap.
                let end = s[i..].iter().position(|c| !(c.is_ascii_digit() || *c == b'.' || *c == b'-')).map(|e| i + e).unwrap_or(s.len());
                if std::str::from_utf8(&s[i..end]).ok().and_then(|n| n.parse::<f32>().ok()).is_some_and(|n| n < -200.0) {
                    pending.push(b" ".to_vec());
                }
                i = end.max(i + 1);
            }
            b'T' if matches!(s.get(i + 1), Some(b'j' | b'J')) => {
                for p in pending.drain(..) {
                    if p == b" " { out.push(' ') } else { show(&p, cmap, out) }
                }
                i += 2;
            }
            b'\'' | b'"' => {
                out.push('\n');
                for p in pending.drain(..) {
                    show(&p, cmap, out);
                }
                i += 1;
            }
            b'E' if s.get(i + 1) == Some(&b'T') => {
                out.push('\n');
                pending.clear();
                i += 2;
            }
            b'T' if matches!(s.get(i + 1), Some(b'd' | b'D' | b'*')) => {
                out.push(' ');
                i += 2;
            }
            b'%' => {
                while i < s.len() && s[i] != b'\n' && s[i] != b'\r' {
                    i += 1;
                }
            }
            b'[' | b']' => i += 1,
            c if c.is_ascii_alphabetic() => {
                // Any other operator ends the operands collected so far.
                let end = s[i..].iter().position(|c| !c.is_ascii_alphanumeric() && *c != b'*').map(|e| i + e).unwrap_or(s.len());
                if !matches!(&s[i..end], b"TJ" | b"Tj") {
                    pending.clear();
                }
                i = end.max(i + 1);
            }
            _ => i += 1,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zip with the given (name, contents); deflated when `deflate`.
    pub fn zip(files: &[(&str, &str)], deflate: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in files {
            let data = if deflate { miniz_oxide::deflate::compress_to_vec(body.as_bytes(), 6) } else { body.as_bytes().to_vec() };
            let local = out.len() as u32;
            let method: u16 = if deflate { 8 } else { 0 };
            let header = |sig: &[u8], extra_central: bool| {
                let mut h = sig.to_vec();
                if extra_central {
                    h.extend(20u16.to_le_bytes());
                }
                h.extend(20u16.to_le_bytes());
                h.extend(0u16.to_le_bytes());
                h.extend(method.to_le_bytes());
                h.extend([0u8; 8]); // time, date, crc (not checked)
                h.extend((data.len() as u32).to_le_bytes());
                h.extend((body.len() as u32).to_le_bytes());
                h.extend((name.len() as u16).to_le_bytes());
                h.extend(0u16.to_le_bytes());
                h
            };
            out.extend(header(b"PK\x03\x04", false));
            out.extend(name.as_bytes());
            out.extend(&data);
            let mut c = header(b"PK\x01\x02", true);
            c.extend([0u8; 10]); // comment length, disk, internal and external attributes
            c.extend(local.to_le_bytes());
            c.extend(name.as_bytes());
            central.push(c);
        }
        let start = out.len() as u32;
        let size: usize = central.iter().map(Vec::len).sum();
        for c in &central {
            out.extend(c);
        }
        out.extend(b"PK\x05\x06\0\0\0\0");
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((size as u32).to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn office_documents() {
        let docx = zip(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", "<w:document><w:body><w:p><w:r><w:t>Quarterly</w:t></w:r><w:r><w:t xml:space=\"preserve\"> budget &amp; plan</w:t></w:r></w:p><w:p><w:r><w:t>Second line</w:t></w:r></w:p></w:body></w:document>")], true);
        assert_eq!(extract("Report.DOCX", "", &docx).as_deref(), Some("Quarterly budget & plan Second line"));
        let pptx = zip(&[("ppt/slides/slide10.xml", "<a:p><a:t>ten</a:t></a:p>"), ("ppt/slides/slide2.xml", "<a:p><a:t>two</a:t></a:p>")], false);
        assert_eq!(extract("deck.pptx", "", &pptx).as_deref(), Some("two ten"), "slides in order");
        let xlsx = zip(&[("xl/sharedStrings.xml", "<sst><si><t>Invoice</t></si><si><t>Total</t></si></sst>")], true);
        assert_eq!(extract("sheet.xlsx", "", &xlsx).as_deref(), Some("Invoice Total"));
        let odt = zip(&[("content.xml", "<office:text><text:p>Olá mundo</text:p></office:text>")], true);
        assert_eq!(extract("x.odt", "", &odt).as_deref(), Some("Olá mundo"));
        assert_eq!(extract("photo.jpg", "image/jpeg", b"\xff\xd8"), None, "images aren't read");
        // Written by another program (Python's zipfile), headers included.
        let real = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sample.docx")).unwrap();
        assert_eq!(extract("sample.docx", "", &real).as_deref(), Some("Fuzz seed: quarterly report Second paragraph & more Header"));
        assert_eq!(extract("broken.docx", "", b"PK\x03\x04 not really"), None);
    }

    #[test]
    fn zip_bombs_are_bounded() {
        let huge = "A".repeat(MAX_PART + 1000);
        let bomb = zip(&[("word/document.xml", &huge)], true);
        assert!(bomb.len() < 200_000, "test needs a small file");
        let t = extract("b.docx", "", &bomb);
        assert!(t.is_none_or(|t| t.len() <= MAX_TEXT));
    }

    fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        for (n, o) in objects.iter().enumerate() {
            out.extend(format!("{} 0 obj\n", n + 1).as_bytes());
            out.extend(o);
            out.extend(b"\nendobj\n");
        }
        out.extend(b"%%EOF\n");
        out
    }
    fn stream(dict: &str, body: &[u8]) -> Vec<u8> {
        let mut o = format!("<< {dict} /Length {} >>\nstream\n", body.len()).into_bytes();
        o.extend(body);
        o.extend(b"\nendstream");
        o
    }

    #[test]
    fn pdf_text_plain_compressed_and_cmapped() {
        let content = b"BT /F1 12 Tf 72 700 Td (Invoice \\(paid\\)) Tj 0 -14 Td [(Tot) -20 (al:) -300 (42) ] TJ ET";
        let plain = pdf(&[stream("", content)]);
        assert_eq!(extract("a.pdf", "", &plain).as_deref(), Some("Invoice (paid) Total: 42"));
        let packed = pdf(&[stream("/Filter /FlateDecode", &miniz_oxide::deflate::compress_to_vec_zlib(content, 6))]);
        assert_eq!(extract("a.pdf", "application/pdf", &packed).as_deref(), Some("Invoice (paid) Total: 42"));
        // A CID font: 2-byte codes mapped to Unicode by a ToUnicode CMap.
        let cmap = b"/CIDInit /ProcSet findresource begin begincmap 2 beginbfchar <0001> <004F> <0002> <00E1> endbfchar 1 beginbfrange <0010> <0012> <0061> endbfrange endcmap";
        let cid = pdf(&[stream("", cmap), stream("", b"BT <000100020010001100120002> Tj ET")]);
        assert_eq!(extract("c.pdf", "", &cid).as_deref(), Some("Oáabcá"));
        assert_eq!(extract("x.pdf", "", b"not a pdf"), None);
    }
}
