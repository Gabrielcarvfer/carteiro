//! Encrypted backups: a copy of the whole database (with the passwords that
//! normally live in the system keyring) sealed with a passphrase, so a new
//! install on any device can take over.
//!
//! Format: the 16-byte magic, a 16-byte salt, the PBKDF2-HMAC-SHA256
//! iteration count (u32 BE) and a 7-byte nonce prefix, then chunks of
//! `u32 BE length || AES-256-GCM ciphertext`. Each chunk's nonce is the
//! prefix, the chunk number (u32 BE) and a last-chunk flag, and the header is
//! the associated data, so reordered, dropped or truncated chunks fail to
//! decrypt (the STREAM construction). Chunks keep memory use flat for
//! mailboxes of any size.

use crate::error::{Error, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use std::io::{Read, Write};
use std::num::NonZeroU32;

const MAGIC: &[u8; 16] = b"CARTEIRO-BACKUP1";
const HEADER_LEN: usize = 16 + 16 + 4 + 7;
const CHUNK: usize = 1 << 20;
const TAG: usize = 16;
/// OWASP's 2023 recommendation for PBKDF2-HMAC-SHA256.
const ITERATIONS: u32 = 600_000;
/// Refuses headers asking for absurd work (a hostile file could stall us).
const MAX_ITERATIONS: u32 = 10_000_000;
pub const MIN_PASSPHRASE: usize = 8;

fn key(passphrase: &str, salt: &[u8], iterations: u32) -> Result<LessSafeKey> {
    let mut k = [0u8; 32];
    let n = NonZeroU32::new(iterations).ok_or_else(|| Error::Other("not a Carteiro backup".into()))?;
    pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, n, salt, passphrase.as_bytes(), &mut k);
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &k).map_err(|_| Error::Other("key".into()))?))
}

fn nonce(prefix: &[u8], counter: u32, last: bool) -> Nonce {
    let mut n = [0u8; 12];
    n[..7].copy_from_slice(prefix);
    n[7..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    Nonce::assume_unique_for_key(n)
}

/// Fills `buf` as far as the reader allows; returns how much was read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

pub fn encrypt(src: &mut impl Read, dst: &mut impl Write, passphrase: &str) -> Result<()> {
    encrypt_with(src, dst, passphrase, ITERATIONS)
}

/// `encrypt` with a chosen work factor; only tests want a cheaper one.
#[doc(hidden)]
pub fn encrypt_with(src: &mut impl Read, dst: &mut impl Write, passphrase: &str, iterations: u32) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(Error::Other(format!("Use a passphrase of at least {MIN_PASSPHRASE} characters")));
    }
    let rng = SystemRandom::new();
    let mut header = [0u8; HEADER_LEN];
    header[..16].copy_from_slice(MAGIC);
    rng.fill(&mut header[16..32]).map_err(|_| Error::Other("no randomness".into()))?;
    header[32..36].copy_from_slice(&iterations.to_be_bytes());
    rng.fill(&mut header[36..]).map_err(|_| Error::Other("no randomness".into()))?;
    let key = key(passphrase, &header[16..32], iterations)?;
    dst.write_all(&header)?;

    // Read one chunk ahead so the last one is known when it's sealed.
    let mut cur = vec![0u8; CHUNK];
    let mut cur_len = read_full(src, &mut cur)?;
    let mut next = vec![0u8; CHUNK];
    let mut counter = 0u32;
    loop {
        let next_len = if cur_len == CHUNK { read_full(src, &mut next)? } else { 0 };
        let last = next_len == 0;
        let mut buf = cur[..cur_len].to_vec();
        key.seal_in_place_append_tag(nonce(&header[36..], counter, last), Aad::from(&header), &mut buf)
            .map_err(|_| Error::Other("encryption failed".into()))?;
        dst.write_all(&(buf.len() as u32).to_be_bytes())?;
        dst.write_all(&buf)?;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        counter = counter.checked_add(1).ok_or_else(|| Error::Other("backup too large".into()))?;
    }
    dst.flush()?;
    Ok(())
}

fn not_backup() -> Error {
    Error::Other("This isn't a Carteiro backup, or it's damaged".into())
}

/// The next chunk's length, or None at the end of the stream.
fn read_len(src: &mut impl Read) -> Result<Option<usize>> {
    let mut l = [0u8; 4];
    match read_full(src, &mut l)? {
        0 => Ok(None),
        4 => {
            let n = u32::from_be_bytes(l) as usize;
            if (TAG..=CHUNK + TAG).contains(&n) { Ok(Some(n)) } else { Err(not_backup()) }
        }
        _ => Err(not_backup()),
    }
}

pub fn decrypt(src: &mut impl Read, dst: &mut impl Write, passphrase: &str) -> Result<()> {
    let mut header = [0u8; HEADER_LEN];
    if read_full(src, &mut header)? != HEADER_LEN || &header[..16] != MAGIC {
        return Err(not_backup());
    }
    let iterations = u32::from_be_bytes(header[32..36].try_into().unwrap());
    if iterations > MAX_ITERATIONS {
        return Err(not_backup());
    }
    let key = key(passphrase, &header[16..32], iterations)?;

    let mut len = read_len(src)?.ok_or_else(not_backup)?;
    let mut counter = 0u32;
    loop {
        let mut buf = vec![0u8; len];
        if read_full(src, &mut buf)? != len {
            return Err(not_backup());
        }
        let next = read_len(src)?;
        let plain = key
            .open_in_place(nonce(&header[36..], counter, next.is_none()), Aad::from(&header), &mut buf)
            .map_err(|_| {
                if counter == 0 {
                    Error::Other("Wrong passphrase, or the backup is damaged".into())
                } else {
                    Error::Other("The backup is damaged or incomplete".into())
                }
            })?;
        dst.write_all(plain)?;
        match next {
            None => break,
            Some(n) => len = n,
        }
        counter = counter.checked_add(1).ok_or_else(not_backup)?;
    }
    dst.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(size: usize) {
        let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
        let mut sealed = Vec::new();
        encrypt(&mut &data[..], &mut sealed, "correct horse").unwrap();
        assert!(!sealed.windows(16).any(|w| data.len() >= 16 && w == &data[..16]) || size < 16);
        let mut out = Vec::new();
        decrypt(&mut &sealed[..], &mut out, "correct horse").unwrap();
        assert_eq!(out, data, "size {size}");
    }

    #[test]
    fn roundtrips_any_size() {
        for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 5] {
            roundtrip(size);
        }
    }

    #[test]
    fn rejects_wrong_passphrase_tampering_and_truncation() {
        let data = vec![7u8; CHUNK * 2 + 10];
        let mut sealed = Vec::new();
        encrypt(&mut &data[..], &mut sealed, "correct horse").unwrap();
        let fails = |b: &[u8], p: &str| decrypt(&mut &b[..], &mut Vec::new(), p).is_err();
        assert!(fails(&sealed, "wrong horse!"));
        let mut flipped = sealed.clone();
        flipped[HEADER_LEN + 100] ^= 1;
        assert!(fails(&flipped, "correct horse"));
        let mut header = sealed.clone();
        header[20] ^= 1; // salt is authenticated too
        assert!(fails(&header, "correct horse"));
        // Dropping the final chunk leaves a stream that ends on a non-final chunk.
        let first_two = HEADER_LEN + 2 * (4 + CHUNK + TAG);
        assert!(fails(&sealed[..first_two], "correct horse"));
        assert!(fails(&sealed[..sealed.len() - 1], "correct horse"));
        assert!(fails(b"not a backup at all, just text....................", "correct horse"));
        assert!(encrypt(&mut &b"x"[..], &mut Vec::new(), "short").is_err());
    }
}
