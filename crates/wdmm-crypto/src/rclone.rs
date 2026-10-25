//! rclone/OpenList crypt-compatible file and filename decryption.
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes256;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crypto_secretbox::{aead::Aead, XSalsa20Poly1305};
use scrypt::{scrypt, Params};
use thiserror::Error;
const MAGIC: &[u8; 8] = b"RCLONE\0\0";
const BLOCK: usize = 65536;
const DEFAULT_SALT: &[u8] = &[
    0xa8, 0x0d, 0xf4, 0x3a, 0x8f, 0xbd, 0x03, 0x08, 0xa7, 0xca, 0xb8, 0x3e, 0x58, 0x1f, 0x86, 0xb1,
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameMode {
    Off,
    Standard,
    Obfuscate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameEncoding {
    Base32,
    Base64,
    Base32768,
}
#[derive(Debug, Error)]
pub enum RcloneError {
    #[error("invalid encrypted content")]
    InvalidContent,
    #[error("decryption failed")]
    Decryption,
    #[error("invalid filename encoding")]
    NameEncoding,
    #[error("key derivation failed")]
    KeyDerivation,
}
pub type Result<T> = std::result::Result<T, RcloneError>;
pub struct RcloneCipher {
    data_key: [u8; 32],
    name_key: [u8; 32],
    tweak: [u8; 16],
    mode: NameMode,
    dirs: bool,
    encoding: NameEncoding,
    suffix: String,
}
impl RcloneCipher {
    pub fn new(
        password: &str,
        salt: &str,
        mode: NameMode,
        dirs: bool,
        encoding: NameEncoding,
        suffix: impl Into<String>,
    ) -> Result<Self> {
        let mut k = [0; 80];
        if !password.is_empty() {
            let s = if salt.is_empty() {
                DEFAULT_SALT.to_vec()
            } else {
                salt.as_bytes().to_vec()
            };
            scrypt(
                password.as_bytes(),
                &s,
                &Params::new(14, 8, 1, 64).map_err(|_| RcloneError::KeyDerivation)?,
                &mut k,
            )
            .map_err(|_| RcloneError::KeyDerivation)?
        }
        Ok(Self {
            data_key: k[..32].try_into().unwrap(),
            name_key: k[32..64].try_into().unwrap(),
            tweak: k[64..].try_into().unwrap(),
            mode,
            dirs,
            encoding,
            suffix: suffix.into(),
        })
    }
    pub fn encrypt_file_name(&self, n: &str) -> Result<String> {
        self.enc_name(n, false)
    }
    pub fn decrypt_file_name(&self, n: &str) -> Result<String> {
        self.dec_name(n, false)
    }
    pub fn encrypt_dir_name(&self, n: &str) -> Result<String> {
        self.enc_name(n, true)
    }
    pub fn decrypt_dir_name(&self, n: &str) -> Result<String> {
        self.dec_name(n, true)
    }
    fn enc_name(&self, n: &str, d: bool) -> Result<String> {
        if self.mode == NameMode::Off {
            return Ok(if d {
                n.into()
            } else {
                format!("{n}{}", self.suffix)
            });
        }
        if d && !self.dirs {
            return Ok(n.into());
        }
        if self.mode == NameMode::Obfuscate {
            return Ok(obfuscate(n, &self.name_key));
        }
        let mut p = n.as_bytes().to_vec();
        let pad = 16 - p.len() % 16;
        p.extend(std::iter::repeat(pad as u8).take(pad));
        let b = eme(&p, &self.name_key, &self.tweak, false);
        Ok(match self.encoding {
            NameEncoding::Base32 => base32hex(&b),
            NameEncoding::Base64 => URL_SAFE_NO_PAD.encode(b),
            NameEncoding::Base32768 => return Err(RcloneError::NameEncoding),
        })
    }
    fn dec_name(&self, n: &str, d: bool) -> Result<String> {
        if self.mode == NameMode::Off {
            return if d {
                Ok(n.into())
            } else {
                n.strip_suffix(&self.suffix)
                    .map(str::to_owned)
                    .ok_or(RcloneError::NameEncoding)
            };
        }
        if d && !self.dirs {
            return Ok(n.into());
        }
        if self.mode == NameMode::Obfuscate {
            return deobfuscate(n, &self.name_key);
        }
        let b = match self.encoding {
            NameEncoding::Base32 => unbase32hex(n)?,
            NameEncoding::Base64 => URL_SAFE_NO_PAD
                .decode(n)
                .map_err(|_| RcloneError::NameEncoding)?,
            NameEncoding::Base32768 => return Err(RcloneError::NameEncoding),
        };
        if b.is_empty() || b.len() % 16 != 0 || b.len() > 2048 {
            return Err(RcloneError::NameEncoding);
        }
        let mut p = eme(&b, &self.name_key, &self.tweak, true);
        let pad = *p.last().ok_or(RcloneError::NameEncoding)? as usize;
        if pad == 0
            || pad > 16
            || p.len() < pad
            || p[p.len() - pad..].iter().any(|x| *x as usize != pad)
        {
            return Err(RcloneError::NameEncoding);
        }
        p.truncate(p.len() - pad);
        String::from_utf8(p).map_err(|_| RcloneError::NameEncoding)
    }
    pub fn decrypt(&self, b: &[u8]) -> Result<Vec<u8>> {
        if b.len() < 32 || &b[..8] != MAGIC {
            return Err(RcloneError::InvalidContent);
        }
        let nonce = &b[8..32];
        let mut out = Vec::new();
        let mut off = 32;
        let mut i = 0;
        while off < b.len() {
            let end = (off + BLOCK + 16).min(b.len());
            out.extend(self.decrypt_block(nonce, i, &b[off..end])?);
            off = end;
            i += 1
        }
        Ok(out)
    }
    pub fn decrypt_block(&self, nonce: &[u8], index: u64, b: &[u8]) -> Result<Vec<u8>> {
        if nonce.len() != 24 {
            return Err(RcloneError::InvalidContent);
        }
        let mut n: [u8; 24] = nonce.try_into().unwrap();
        let mut carry = index;
        for x in &mut n[..8] {
            let (v, c) = x.overflowing_add(carry as u8);
            *x = v;
            carry = (carry >> 8) + u64::from(c)
        }
        XSalsa20Poly1305::new((&self.data_key).into())
            .decrypt((&n).into(), b)
            .map_err(|_| RcloneError::Decryption)
    }
    pub fn decrypted_size(size: u64) -> Result<u64> {
        if size < 32 {
            return Err(RcloneError::InvalidContent);
        }
        let n = size - 32;
        let full = n / (BLOCK as u64 + 16);
        let rem = n % (BLOCK as u64 + 16);
        if rem > 0 && rem <= 16 {
            return Err(RcloneError::InvalidContent);
        }
        Ok(full * BLOCK as u64 + rem.saturating_sub(16))
    }
}
fn xor(a: &mut [u8], b: &[u8]) {
    for (i, x) in a.iter_mut().enumerate() {
        *x ^= b[i]
    }
}
fn mul2(b: &[u8; 16]) -> [u8; 16] {
    let mut o = [0; 16];
    o[0] = ((2u16 * b[0] as u16) & 255) as u8 ^ (135 & ((0u8).wrapping_sub(b[15] >> 7)));
    for j in 1..16 {
        o[j] = ((2u16 * b[j] as u16 + (b[j - 1] >> 7) as u16) & 255) as u8
    }
    o
}
fn eme(input: &[u8], key: &[u8; 32], tweak: &[u8; 16], dec: bool) -> Vec<u8> {
    let m = input.len() / 16;
    let aes = Aes256::new(key.into());
    let mut z = GenericArray::default();
    aes.encrypt_block(&mut z);
    let mut chain: [u8; 16] = z.into();
    let mut l = Vec::with_capacity(m);
    for _ in 0..m {
        chain = mul2(&chain);
        l.push(chain)
    }
    let mut c = vec![0; input.len()];
    for j in 0..m {
        let mut p: [u8; 16] = input[j * 16..j * 16 + 16].try_into().unwrap();
        xor(&mut p, &l[j]);
        let mut b = GenericArray::clone_from_slice(&p);
        if dec {
            aes.decrypt_block(&mut b)
        } else {
            aes.encrypt_block(&mut b)
        }
        c[j * 16..j * 16 + 16].copy_from_slice(&b)
    }
    let mut mp: [u8; 16] = c[..16].try_into().unwrap();
    xor(&mut mp, tweak);
    for j in 1..m {
        xor(&mut mp, &c[j * 16..j * 16 + 16])
    }
    let mut mc = GenericArray::clone_from_slice(&mp);
    if dec {
        aes.decrypt_block(&mut mc)
    } else {
        aes.encrypt_block(&mut mc)
    }
    let mut mm = mp;
    xor(&mut mm, &mc);
    for j in 1..m {
        mm = mul2(&mm);
        xor(&mut c[j * 16..j * 16 + 16], &mm)
    }
    let mut first: [u8; 16] = mc.into();
    xor(&mut first, tweak);
    for j in 1..m {
        xor(&mut first, &c[j * 16..j * 16 + 16])
    }
    c[..16].copy_from_slice(&first);
    for j in 0..m {
        let mut b = GenericArray::clone_from_slice(&c[j * 16..j * 16 + 16]);
        if dec {
            aes.decrypt_block(&mut b)
        } else {
            aes.encrypt_block(&mut b)
        }
        let mut x: [u8; 16] = b.into();
        xor(&mut x, &l[j]);
        c[j * 16..j * 16 + 16].copy_from_slice(&x)
    }
    c
}
fn base32hex(b: &[u8]) -> String {
    const A: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    let (mut a, mut bits) = (0u32, 0);
    let mut s = String::new();
    for x in b {
        a = (a << 8) | *x as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            s.push(A[((a >> bits) & 31) as usize] as char)
        }
    }
    if bits > 0 {
        s.push(A[((a << (5 - bits)) & 31) as usize] as char)
    }
    s
}
fn unbase32hex(s: &str) -> Result<Vec<u8>> {
    let (mut a, mut bits) = (0u32, 0);
    let mut o = Vec::new();
    for x in s.bytes() {
        let c = x.to_ascii_lowercase();
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'v' => c - b'a' + 10,
            _ => return Err(RcloneError::NameEncoding),
        };
        a = (a << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            o.push((a >> bits) as u8)
        }
    }
    Ok(o)
}
fn obfuscate(s: &str, key: &[u8; 32]) -> String {
    let head = s.chars().map(|c| c as u32).sum::<u32>() % 256;
    let mut d = head;
    for b in key {
        d += *b as u32
    }
    let mut o = format!("{head}.");
    for c in s.chars().map(|x| x as u32) {
        if c == 33 {
            o.push_str("!!");
            continue;
        }
        let r = if (48..=57).contains(&c) {
            9
        } else if (65..=90).contains(&c) || (97..=122).contains(&c) {
            25
        } else if (160..=255).contains(&c) {
            95
        } else if c >= 256 {
            127
        } else {
            0
        };
        if r == 0 {
            o.push(char::from_u32(c).unwrap_or('?'));
            continue;
        }
        let step = d % r + 1;
        let base = if c >= 256 {
            c - c % 256
        } else if c >= 160 {
            160
        } else if c >= 65 {
            65
        } else {
            48
        };
        let size = if c >= 256 {
            256
        } else if c >= 160 {
            96
        } else if c >= 65 {
            52
        } else {
            10
        };
        let mut p = c - base;
        if base == 65 && p >= 32 {
            p -= 6
        }
        p = (p + step) % size;
        if base == 65 && p >= 26 {
            p += 6
        }
        o.push(char::from_u32(base + p).unwrap_or('?'))
    }
    o
}
fn deobfuscate(s: &str, key: &[u8; 32]) -> Result<String> {
    let (h, t) = s.split_once('.').ok_or(RcloneError::NameEncoding)?;
    let mut d = h.parse::<u32>().map_err(|_| RcloneError::NameEncoding)?;
    for b in key {
        d += *b as u32
    }
    let mut it = t.chars();
    let mut o = String::new();
    while let Some(c) = it.next() {
        if c == '!' {
            if it.next() != Some('!') {
                return Err(RcloneError::NameEncoding);
            }
            o.push('!');
            continue;
        }
        let x = c as u32;
        let r = if (48..=57).contains(&x) {
            9
        } else if (65..=90).contains(&x) || (97..=122).contains(&x) {
            25
        } else if (160..=255).contains(&x) {
            95
        } else if x >= 256 {
            127
        } else {
            0
        };
        if r == 0 {
            o.push(c);
            continue;
        }
        let step = d % r + 1;
        let base = if x >= 256 {
            x - x % 256
        } else if x >= 160 {
            160
        } else if x >= 65 {
            65
        } else {
            48
        };
        let size = if x >= 256 {
            256
        } else if x >= 160 {
            96
        } else if x >= 65 {
            52
        } else {
            10
        };
        let mut p = x - base;
        if base == 65 && p >= 32 {
            p -= 6
        }
        p = (p + size - step) % size;
        if base == 65 && p >= 26 {
            p += 6
        }
        o.push(char::from_u32(base + p).ok_or(RcloneError::NameEncoding)?)
    }
    Ok(o)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn c(enc: NameEncoding) -> RcloneCipher {
        RcloneCipher::new(
            "testpass",
            "testsalt",
            NameMode::Standard,
            true,
            enc,
            ".bin",
        )
        .unwrap()
    }
    #[test]
    fn names() {
        assert_eq!(
            c(NameEncoding::Base32)
                .encrypt_file_name("hello.txt")
                .unwrap(),
            "9ph489uqiu6hppkcbp0g4328c4"
        );
        assert_eq!(
            c(NameEncoding::Base64)
                .encrypt_file_name("hello.txt")
                .unwrap(),
            "TmJEJ9qXjRzmjF5BAgxIYQ"
        );
        assert_eq!(
            c(NameEncoding::Base32)
                .decrypt_file_name("9ph489uqiu6hppkcbp0g4328c4")
                .unwrap(),
            "hello.txt"
        )
    }
    #[test]
    fn content() {
        let x=hex::decode("52434c4f4e4500005aec6b92d0a9c18d271aa8a7d9f0820eb5781dbfe10c58a410abcbafd5e66a17a5042ee1ad2c02a0f495731a3606e633bcd0a3b7caa5647ce643").unwrap();
        assert_eq!(
            c(NameEncoding::Base32).decrypt(&x).unwrap(),
            "hello crypt 你好".as_bytes()
        );
    }
    #[test]
    fn obfuscation() {
        let x = RcloneCipher::new(
            "testpass",
            "testsalt",
            NameMode::Obfuscate,
            true,
            NameEncoding::Base32,
            ".bin",
        )
        .unwrap();
        assert_eq!(x.encrypt_file_name("hello.txt").unwrap(), "162.vszzC.HLH");
        assert_eq!(x.decrypt_file_name("162.vszzC.HLH").unwrap(), "hello.txt")
    }
}
