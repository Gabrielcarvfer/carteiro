//! Finds attachment bodies inside a raw message so the store can keep each
//! distinct one only once (content-addressed by SHA-256). The cut is done on
//! the encoded bytes, so splicing them back yields the original message
//! byte-for-byte.

use mail_parser::{MessageParser, MimeHeaders};
use ring::digest::{digest, SHA256};

/// Bodies smaller than this aren't worth a separate row.
const MIN_SIZE: usize = 4096;

pub struct Part {
    /// Range of the attachment body in the original message.
    pub start: usize,
    pub end: usize,
    /// Where to re-insert it in the stripped message.
    pub offset: usize,
    pub hash: String,
}

pub fn split_attachments(raw: &[u8]) -> (Vec<u8>, Vec<Part>) {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(msg) = MessageParser::default().parse(raw) {
        for p in msg.attachments() {
            let (start, end) = (p.raw_body_offset(), p.raw_end_offset());
            if end > start && end <= raw.len() && end - start >= MIN_SIZE && p.content_type().is_none_or(|c| !c.ctype().eq_ignore_ascii_case("message")) {
                ranges.push((start, end));
            }
        }
    }
    ranges.sort();
    ranges.dedup();
    let mut stripped = Vec::with_capacity(raw.len());
    let mut parts = Vec::new();
    let mut pos = 0;
    for (start, end) in ranges {
        if start < pos {
            continue; // overlapping (nested) part already covered
        }
        stripped.extend_from_slice(&raw[pos..start]);
        let hash = digest(&SHA256, &raw[start..end]).as_ref().iter().map(|b| format!("{b:02x}")).collect();
        parts.push(Part { start, end, offset: stripped.len(), hash });
        pos = end;
    }
    stripped.extend_from_slice(&raw[pos..]);
    (stripped, parts)
}
