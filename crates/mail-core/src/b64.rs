//! Base64 (RFC 4648), plus the alphabet variants used by IMAP mailbox names
//! (RFC 3501) and OAuth PKCE (base64url).

pub const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
pub const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
pub const IMAP: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

pub fn encode_with(data: impl AsRef<[u8]>, alphabet: &[u8; 64], pad: bool) -> String {
    let data = data.as_ref();
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = chunk.len() + 1;
        for i in 0..4 {
            if i < chars {
                out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Standard alphabet with padding.
pub fn encode(data: impl AsRef<[u8]>) -> String {
    encode_with(data, STANDARD, true)
}

/// Decodes, ignoring padding and ASCII whitespace. None on any other
/// character or a truncated final group.
pub fn decode_with(s: &str, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    let mut map = [255u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        map[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = map[c as usize];
        if v == 255 {
            return None;
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bits >= 6 {
        return None; // a lone trailing character can't encode a byte
    }
    Some(out)
}

pub fn decode(s: &str) -> Option<Vec<u8>> {
    decode_with(s, STANDARD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(encode(plain), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
        assert_eq!(encode_with([0xfb, 0xff], URL_SAFE, false), "-_8");
        assert_eq!(decode("Zm9v\r\nYg==").unwrap(), b"foob");
        assert!(decode("Zm9v!").is_none());
        assert!(decode("Z").is_none());
    }
}
