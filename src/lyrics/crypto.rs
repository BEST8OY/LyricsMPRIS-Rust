//! Cryptographic, XOR, and decompression helpers for lyrics providers
//! (NetEase EAPI AES-ECB, QQ QRC 3DES-ECB + zlib, KuGou KRC XOR + zlib, Kuwo XOR + zlib).

use aes::Aes128;
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use flate2::read::ZlibDecoder;
use md5::{Digest, Md5};
use std::io::Read;

type Aes128EcbEnc = ecb::Encryptor<Aes128>;
type Aes128EcbDec = ecb::Decryptor<Aes128>;

#[rustfmt::skip]
const QRC_SBOX: [[u8; 64]; 8] = [
    [14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13],
    [15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 15, 12, 0, 1, 10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9],
    [10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12],
    [7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 10, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14],
    [2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3],
    [12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13],
    [4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12],
    [13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11],
];

pub fn md5_bytes(input: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(input);
    hasher.finalize().into()
}

pub fn md5_hex(input: &str) -> String {
    to_hex(&md5_bytes(input.as_bytes()))
}

pub fn to_hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(char::from_digit((b >> 4) as u32 & 0xf, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

pub fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

pub fn aes_ecb_pkcs7_encrypt_hex(data: &str, key: &[u8; 16]) -> String {
    let enc = Aes128EcbEnc::new(key.into());
    let pt = data.as_bytes();
    let pad_len = 16 - (pt.len() % 16);
    let mut buf = vec![0u8; pt.len() + pad_len];
    let encrypted = enc
        .encrypt_padded_b2b_mut::<Pkcs7>(pt, &mut buf)
        .expect("PKCS#7 output buffer sized");
    to_hex(encrypted)
}

pub fn aes_ecb_pkcs7_decrypt_bytes(raw: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    let dec = Aes128EcbDec::new(key.into());
    let mut buf = raw.to_vec();
    let plain = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    Some(plain.to_vec())
}

/// Inflates a zlib-compressed byte slice to UTF-8 text (lossy fallback avoided; strict UTF-8).
pub fn zlib_inflate_bytes(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

pub fn zlib_inflate_utf8(data: &[u8]) -> Option<String> {
    let bytes = zlib_inflate_bytes(data)?;
    String::from_utf8(bytes).ok()
}

#[inline]
fn qrc_bitnum(bytes: &[u8], b: usize, c: u32) -> u32 {
    let byte_index = (b / 32) * 4 + 3 - ((b % 32) / 8);
    if byte_index >= bytes.len() {
        return 0;
    }
    (((bytes[byte_index] as u32) >> (7 - (b % 8))) & 1) << c
}

#[inline]
fn qrc_bitnum_intr(value: u32, b: u32, c: u32) -> u32 {
    ((value >> (31 - b)) & 1) << c
}

#[inline]
fn qrc_bitnum_intl(value: u32, b: u32, c: u32) -> u32 {
    ((value.wrapping_shl(b)) & 0x8000_0000) >> c
}

#[inline]
fn qrc_sbox_bit(value: u32) -> usize {
    ((value & 32) | ((value & 31) >> 1) | ((value & 1) << 4)) as usize
}

fn qrc_initial_permutation(input: &[u8]) -> (u32, u32) {
    let s0 = qrc_bitnum(input, 57, 31)
        | qrc_bitnum(input, 49, 30)
        | qrc_bitnum(input, 41, 29)
        | qrc_bitnum(input, 33, 28)
        | qrc_bitnum(input, 25, 27)
        | qrc_bitnum(input, 17, 26)
        | qrc_bitnum(input, 9, 25)
        | qrc_bitnum(input, 1, 24)
        | qrc_bitnum(input, 59, 23)
        | qrc_bitnum(input, 51, 22)
        | qrc_bitnum(input, 43, 21)
        | qrc_bitnum(input, 35, 20)
        | qrc_bitnum(input, 27, 19)
        | qrc_bitnum(input, 19, 18)
        | qrc_bitnum(input, 11, 17)
        | qrc_bitnum(input, 3, 16)
        | qrc_bitnum(input, 61, 15)
        | qrc_bitnum(input, 53, 14)
        | qrc_bitnum(input, 45, 13)
        | qrc_bitnum(input, 37, 12)
        | qrc_bitnum(input, 29, 11)
        | qrc_bitnum(input, 21, 10)
        | qrc_bitnum(input, 13, 9)
        | qrc_bitnum(input, 5, 8)
        | qrc_bitnum(input, 63, 7)
        | qrc_bitnum(input, 55, 6)
        | qrc_bitnum(input, 47, 5)
        | qrc_bitnum(input, 39, 4)
        | qrc_bitnum(input, 31, 3)
        | qrc_bitnum(input, 23, 2)
        | qrc_bitnum(input, 15, 1)
        | qrc_bitnum(input, 7, 0);

    let s1 = qrc_bitnum(input, 56, 31)
        | qrc_bitnum(input, 48, 30)
        | qrc_bitnum(input, 40, 29)
        | qrc_bitnum(input, 32, 28)
        | qrc_bitnum(input, 24, 27)
        | qrc_bitnum(input, 16, 26)
        | qrc_bitnum(input, 8, 25)
        | qrc_bitnum(input, 0, 24)
        | qrc_bitnum(input, 58, 23)
        | qrc_bitnum(input, 50, 22)
        | qrc_bitnum(input, 42, 21)
        | qrc_bitnum(input, 34, 20)
        | qrc_bitnum(input, 26, 19)
        | qrc_bitnum(input, 18, 18)
        | qrc_bitnum(input, 10, 17)
        | qrc_bitnum(input, 2, 16)
        | qrc_bitnum(input, 60, 15)
        | qrc_bitnum(input, 52, 14)
        | qrc_bitnum(input, 44, 13)
        | qrc_bitnum(input, 36, 12)
        | qrc_bitnum(input, 28, 11)
        | qrc_bitnum(input, 20, 10)
        | qrc_bitnum(input, 12, 9)
        | qrc_bitnum(input, 4, 8)
        | qrc_bitnum(input, 62, 7)
        | qrc_bitnum(input, 54, 6)
        | qrc_bitnum(input, 46, 5)
        | qrc_bitnum(input, 38, 4)
        | qrc_bitnum(input, 30, 3)
        | qrc_bitnum(input, 22, 2)
        | qrc_bitnum(input, 14, 1)
        | qrc_bitnum(input, 6, 0);

    (s0, s1)
}

fn qrc_inverse_permutation(s0: u32, s1: u32) -> [u8; 8] {
    let mut r = [0u8; 8];
    let rows = [4u32, 5, 6, 7, 0, 1, 2, 3];
    for (i, &base) in rows.iter().enumerate() {
        let v = qrc_bitnum_intr(s1, base, 7)
            | qrc_bitnum_intr(s0, base, 6)
            | qrc_bitnum_intr(s1, base + 8, 5)
            | qrc_bitnum_intr(s0, base + 8, 4)
            | qrc_bitnum_intr(s1, base + 16, 3)
            | qrc_bitnum_intr(s0, base + 16, 2)
            | qrc_bitnum_intr(s1, base + 24, 1)
            | qrc_bitnum_intr(s0, base + 24, 0);
        r[i] = (v & 0xff) as u8;
    }
    r
}

fn qrc_des_f(state: u32, key: &[u32; 6]) -> u32 {
    let t1 = qrc_bitnum_intl(state, 31, 0)
        | ((state & 0xf000_0000) >> 1)
        | qrc_bitnum_intl(state, 4, 5)
        | qrc_bitnum_intl(state, 3, 6)
        | ((state & 0x0f00_0000) >> 3)
        | qrc_bitnum_intl(state, 8, 11)
        | qrc_bitnum_intl(state, 7, 12)
        | ((state & 0x00f0_0000) >> 5)
        | qrc_bitnum_intl(state, 12, 17)
        | qrc_bitnum_intl(state, 11, 18)
        | ((state & 0x000f_0000) >> 7)
        | qrc_bitnum_intl(state, 16, 23);

    let t2 = qrc_bitnum_intl(state, 15, 0)
        | ((state & 0x0000_f000) << 15)
        | qrc_bitnum_intl(state, 20, 5)
        | qrc_bitnum_intl(state, 19, 6)
        | ((state & 0x0000_0f00) << 13)
        | qrc_bitnum_intl(state, 24, 11)
        | qrc_bitnum_intl(state, 23, 12)
        | ((state & 0x0000_00f0) << 11)
        | qrc_bitnum_intl(state, 28, 17)
        | qrc_bitnum_intl(state, 27, 18)
        | ((state & 0x0000_000f) << 9)
        | qrc_bitnum_intl(state, 0, 23);

    let l = [
        ((t1 >> 24) & 255) ^ key[0],
        ((t1 >> 16) & 255) ^ key[1],
        ((t1 >> 8) & 255) ^ key[2],
        ((t2 >> 24) & 255) ^ key[3],
        ((t2 >> 16) & 255) ^ key[4],
        ((t2 >> 8) & 255) ^ key[5],
    ];

    let r = ((QRC_SBOX[0][qrc_sbox_bit(l[0] >> 2)] as u32) << 28)
        | ((QRC_SBOX[1][qrc_sbox_bit(((l[0] & 3) << 4) | (l[1] >> 4))] as u32) << 24)
        | ((QRC_SBOX[2][qrc_sbox_bit(((l[1] & 15) << 2) | (l[2] >> 6))] as u32) << 20)
        | ((QRC_SBOX[3][qrc_sbox_bit(l[2] & 63)] as u32) << 16)
        | ((QRC_SBOX[4][qrc_sbox_bit(l[3] >> 2)] as u32) << 12)
        | ((QRC_SBOX[5][qrc_sbox_bit(((l[3] & 3) << 4) | (l[4] >> 4))] as u32) << 8)
        | ((QRC_SBOX[6][qrc_sbox_bit(((l[4] & 15) << 2) | (l[5] >> 6))] as u32) << 4)
        | (QRC_SBOX[7][qrc_sbox_bit(l[5] & 63)] as u32);

    qrc_bitnum_intl(r, 15, 0)
        | qrc_bitnum_intl(r, 6, 1)
        | qrc_bitnum_intl(r, 19, 2)
        | qrc_bitnum_intl(r, 20, 3)
        | qrc_bitnum_intl(r, 28, 4)
        | qrc_bitnum_intl(r, 11, 5)
        | qrc_bitnum_intl(r, 27, 6)
        | qrc_bitnum_intl(r, 16, 7)
        | qrc_bitnum_intl(r, 0, 8)
        | qrc_bitnum_intl(r, 14, 9)
        | qrc_bitnum_intl(r, 22, 10)
        | qrc_bitnum_intl(r, 25, 11)
        | qrc_bitnum_intl(r, 4, 12)
        | qrc_bitnum_intl(r, 17, 13)
        | qrc_bitnum_intl(r, 30, 14)
        | qrc_bitnum_intl(r, 9, 15)
        | qrc_bitnum_intl(r, 1, 16)
        | qrc_bitnum_intl(r, 7, 17)
        | qrc_bitnum_intl(r, 23, 18)
        | qrc_bitnum_intl(r, 13, 19)
        | qrc_bitnum_intl(r, 31, 20)
        | qrc_bitnum_intl(r, 26, 21)
        | qrc_bitnum_intl(r, 2, 22)
        | qrc_bitnum_intl(r, 8, 23)
        | qrc_bitnum_intl(r, 18, 24)
        | qrc_bitnum_intl(r, 12, 25)
        | qrc_bitnum_intl(r, 29, 26)
        | qrc_bitnum_intl(r, 5, 27)
        | qrc_bitnum_intl(r, 21, 28)
        | qrc_bitnum_intl(r, 10, 29)
        | qrc_bitnum_intl(r, 3, 30)
        | qrc_bitnum_intl(r, 24, 31)
}

fn qrc_key_schedule(key: &[u8], decrypt: bool) -> [[u32; 6]; 16] {
    let mut schedule = [[0u32; 6]; 16];
    let shifts = [1u32, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
    let pc = [
        56usize, 48, 40, 32, 24, 16, 8, 0, 57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18,
        10, 2, 59, 51, 43, 35,
    ];
    let pd = [
        62usize, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 60, 52, 44, 36, 28, 20,
        12, 4, 27, 19, 11, 3,
    ];
    let kc = [
        13u32, 16, 10, 23, 0, 4, 2, 27, 14, 5, 20, 9, 22, 18, 11, 3, 25, 7, 15, 6, 26, 19, 12, 1,
        40, 51, 30, 36, 46, 54, 29, 39, 50, 44, 32, 47, 43, 48, 38, 55, 33, 52, 45, 41, 49, 35, 28,
        31,
    ];

    let mut c = 0u32;
    let mut d = 0u32;
    for i in 0..28 {
        c = c.wrapping_add(qrc_bitnum(key, pc[i], (31 - i) as u32));
        d = d.wrapping_add(qrc_bitnum(key, pd[i], (31 - i) as u32));
    }

    for (i, &shift) in shifts.iter().enumerate() {
        c = ((c << shift) | (c >> (28 - shift))) & 0xffff_fff0;
        d = ((d << shift) | (d >> (28 - shift))) & 0xffff_fff0;
        let idx = if decrypt { 15 - i } else { i };
        for (j, &bit) in kc[..24].iter().enumerate() {
            schedule[idx][j / 8] |= qrc_bitnum_intr(c, bit, (7 - (j % 8)) as u32);
        }
        for (j, &bit) in kc[24..48].iter().enumerate() {
            let slot = (j + 24) / 8;
            schedule[idx][slot] |= qrc_bitnum_intr(d, bit - 27, (7 - ((j + 24) % 8)) as u32);
        }
    }
    schedule
}

fn qrc_crypt_block(input: &[u8], schedule: &[[u32; 6]; 16]) -> [u8; 8] {
    let (mut s0, mut s1) = qrc_initial_permutation(input);
    for key in &schedule[..15] {
        let prev = s1;
        s1 = qrc_des_f(s1, key) ^ s0;
        s0 = prev;
    }
    s0 ^= qrc_des_f(s1, &schedule[15]);
    qrc_inverse_permutation(s0, s1)
}

/// Decrypts QQ Music QRC payloads (3DES-ECB matching QQ's upstream key layout).
pub fn triple_des_ecb_decrypt(data: &[u8], key: &str) -> Vec<u8> {
    let key_bytes = key.as_bytes();
    if data.is_empty() || !data.len().is_multiple_of(8) || key_bytes.len() < 24 {
        return Vec::new();
    }
    let k3 = qrc_key_schedule(&key_bytes[16..24], true);
    let k2 = qrc_key_schedule(&key_bytes[8..16], false);
    let k1 = qrc_key_schedule(&key_bytes[0..8], true);

    let mut out = vec![0u8; data.len()];
    for (i, chunk) in data.as_chunks::<8>().0.iter().enumerate() {
        let b = qrc_crypt_block(chunk, &k3);
        let b = qrc_crypt_block(&b, &k2);
        let b = qrc_crypt_block(&b, &k1);
        out[i * 8..(i + 1) * 8].copy_from_slice(&b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    #[test]
    fn test_md5_and_hex_roundtrip() {
        assert_eq!(md5_hex("hello"), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(hex_to_bytes("0a1bff"), Some(vec![0x0a, 0x1b, 0xff]));
        assert_eq!(hex_to_bytes("abc"), None);
    }

    #[test]
    fn test_aes_ecb_pkcs7_roundtrip() {
        let key = *b"e82ckenh8dichen8";
        let hex = aes_ecb_pkcs7_encrypt_hex("lyrics-test-payload", &key);
        let raw = hex_to_bytes(&hex).unwrap();
        let dec = aes_ecb_pkcs7_decrypt_bytes(&raw, &key).unwrap();
        assert_eq!(String::from_utf8(dec).unwrap(), "lyrics-test-payload");
    }

    #[test]
    fn test_zlib_inflate() {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"[00:01.00]Hello").unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(
            zlib_inflate_utf8(&compressed).as_deref(),
            Some("[00:01.00]Hello")
        );
    }
}
