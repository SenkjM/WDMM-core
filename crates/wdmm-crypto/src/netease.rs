//! Netease Music (网易云音乐) weapi + linuxapi encryption.
//!
//! ## weapi
//! - 2-layer AES-CBC: preset key "0CoJUm6Qyw8W8jud" + IV 0102030405060708,
//!   then random 16-byte key (reversed) + same IV
//! - Raw RSA (1024-bit modulus, e=65537): 128-byte buffer with random key in last 16 bytes
//! - Output: `params` (base64 of outer AES), `encSecKey` (256 hex chars of RSA ciphertext)
//!
//! ## linuxapi
//! - AES-ECB with key "rFgB&h#%2?^eDg:Q"
//! - Output: `eparams` (uppercase hex)
//!
//! Golden test vectors match Dart implementation from WMM-dlna-ui.

use aes::cipher::{BlockEncryptMut, KeyIvInit, KeyInit};
use aes::Aes128;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use num_bigint::BigUint;
use rand::Rng;
use serde_json::Value;

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128EcbEnc = ecb::Encryptor<Aes128>;

/// Preset AES key for weapi first layer.
const PRESET_KEY: &[u8] = b"0CoJUm6Qyw8W8jud";

/// IV for both weapi layers (ASCII "0102030405060708", 16 bytes).
const IV: &[u8] = b"0102030405060708";

/// Linuxapi AES-ECB key.
const LINUX_API_KEY: &[u8] = b"rFgB&h#%2?^eDg:Q";

/// RSA modulus (1024-bit, 128 bytes).
const RSA_MODULUS_HEX: &str = "e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb\
7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c9387011\
4af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289\
dc6935b3ece0462db0a22b8e7";

/// RSA public exponent.
const RSA_EXPONENT: u32 = 65537;

/// Character set for random key generation (62 chars: a-z A-Z 0-9).
const STD_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// weapi encryption result.
#[derive(Debug, Clone)]
pub struct WeapiResult {
    /// Base64-encoded outer AES ciphertext.
    pub params: String,
    /// 256-char lowercase hex RSA ciphertext.
    pub enc_sec_key: String,
}

/// Generate a random 16-byte key from STD_CHARS.
fn random_key() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    (0..16).map(|_| STD_CHARS[rng.gen_range(0..STD_CHARS.len())]).collect()
}

/// AES-CBC encrypt with PKCS7 padding.
fn aes_cbc_encrypt(data: &[u8], key: &[u8], iv: &[u8]) -> Vec<u8> {
    use aes::cipher::block_padding::Pkcs7;

    // Allocate buffer with enough space for padding
    let block_size = 16;
    let padded_len = ((data.len() / block_size) + 1) * block_size;
    let mut buf = vec![0u8; padded_len];
    buf[..data.len()].copy_from_slice(data);

    let cipher = Aes128CbcEnc::new(key.into(), iv.into());
    let len = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

/// AES-ECB encrypt with PKCS7 padding.
fn aes_ecb_encrypt(data: &[u8], key: &[u8]) -> Vec<u8> {
    use aes::cipher::block_padding::Pkcs7;

    // Allocate buffer with enough space for padding
    let block_size = 16;
    let padded_len = ((data.len() / block_size) + 1) * block_size;
    let mut buf = vec![0u8; padded_len];
    buf[..data.len()].copy_from_slice(data);

    let cipher = Aes128EcbEnc::new(key.into());
    let len = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

/// Raw RSA encryption (no padding): c = m^e mod n, where m is a 128-byte buffer
/// with the 16-byte key in the last 16 positions (first 112 bytes are zero).
fn rsa_raw_encrypt(key: &[u8]) -> Vec<u8> {
    assert_eq!(key.len(), 16, "RSA raw encrypt requires 16-byte key");

    // Build 128-byte message: 112 zero bytes + 16 key bytes
    let mut msg_bytes = vec![0u8; 112];
    msg_bytes.extend_from_slice(key);

    // Convert to BigInt (big-endian)
    let m = msg_bytes.iter().fold(BigUint::from(0u32), |acc, &b| {
        (acc << 8) | BigUint::from(b)
    });

    let n = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16)
        .expect("invalid RSA modulus");
    let e = BigUint::from(RSA_EXPONENT);

    // c = m^e mod n
    let c = m.modpow(&e, &n);

    // Convert back to 128 bytes (pad with leading zeros if needed)
    let c_bytes = c.to_bytes_be();
    let mut out = vec![0u8; 128 - c_bytes.len()];
    out.extend_from_slice(&c_bytes);
    out
}

/// weapi encryption: 2-layer AES-CBC + raw RSA.
///
/// # Arguments
/// - `params`: JSON object to encrypt (e.g., `{"limit": "200", "offset": "0"}`)
/// - `secret_key`: Optional fixed key for testing; if None, generates random key
///
/// # Returns
/// WeapiResult with base64 `params` and hex `enc_sec_key`.
pub fn weapi_encrypt(params: &Value, secret_key: Option<&[u8]>) -> WeapiResult {
    let key = secret_key.map(|k| k.to_vec()).unwrap_or_else(random_key);
    assert_eq!(key.len(), 16, "secret key must be 16 bytes");

    let text = serde_json::to_string(params).expect("JSON serialize failed");

    // Layer 1: AES-CBC(text, preset_key, iv) → base64
    let inner = aes_cbc_encrypt(text.as_bytes(), PRESET_KEY, IV);
    let inner_b64 = BASE64.encode(&inner);

    // Layer 2: AES-CBC(inner_b64, reversed_key, iv) → base64
    let reversed_key: Vec<u8> = key.iter().copied().rev().collect();
    let outer = aes_cbc_encrypt(inner_b64.as_bytes(), &reversed_key, IV);
    let params_b64 = BASE64.encode(&outer);

    // RSA: encrypt original key (not reversed)
    let rsa_ct = rsa_raw_encrypt(&key);
    let enc_sec_key = rsa_ct.iter().map(|b| format!("{:02x}", b)).collect::<String>();

    WeapiResult {
        params: params_b64,
        enc_sec_key,
    }
}

/// linuxapi encryption: AES-ECB → uppercase hex.
///
/// # Arguments
/// - `payload`: JSON object with keys `url`, `method`, `params` (in that order for golden match)
///
/// # Returns
/// Form field `eparams` as uppercase hex string.
pub fn linuxapi_encrypt(payload: &Value) -> String {
    let text = serde_json::to_string(payload).expect("JSON serialize failed");
    let ct = aes_ecb_encrypt(text.as_bytes(), LINUX_API_KEY);
    ct.iter().map(|b| format!("{:02X}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fixed 16-byte key for golden vectors: ASCII "0123456789abcdef".
    const FIXED_KEY: &[u8] = b"0123456789abcdef";

    /// Golden vectors from Dart netease_music_crypto_test.dart.
    const GOLDEN_WEAPI_GET_PARAMS: &str = "lIy2fgilVhtQddtJjyhJ0lcRvibfdvY/ielCDQoJ3Y2586R1yEsEGQ6pW94S/ke4";
    const GOLDEN_WEAPI_DEL_PARAMS: &str = "IyhNrvDB9u7Z0+TkXLI77v66fDll2u3EIw0GPcfvJtLm5EDkGgGBvYCU0N/iy42I";
    const GOLDEN_RSA_RAW_HEX: &str = "ac744caab466f1fd5a75228365d15df7add288e8982a2c8f80da5f8f22d54834b5e1eab04dde9dac7341851a98de37463d8ba01d9e9c4d2c546c0f948e3163c667785a1b9c4f160305b0fb1ec3b698de0c704719b2fbc469582654b9c2595317dc40c3ecee32baa6d9753970b66667c01bde9ad34048fa8b2b0f628e56a5bf49";
    const GOLDEN_LINUXAPI: &str = "3134729CC09796CB31F2FFF45684A58FCFA972FA5EA3D6247CD6247C8198CB878588AFAA94B0E0C872FF37D6781726EEB744C5CDC5914382AEC70AD44947D2F727496CC1F9B4C3FE11F193E0A3E577DC636893A25F4A1093F3C145C6323F35F73B3FD2350E52650636E8C49EE9EDFD1ABDDF2A378F55BEE59918B2C7146A2D0E";

    #[test]
    fn test_rsa_raw_golden() {
        let ct = rsa_raw_encrypt(FIXED_KEY);
        let hex = ct.iter().map(|b| format!("{:02x}", b)).collect::<String>();
        assert_eq!(hex, GOLDEN_RSA_RAW_HEX);
        assert_eq!(ct.len(), 128);
    }

    #[test]
    fn test_weapi_cloud_get() {
        let params = json!({"limit": "200", "offset": "0"});
        let result = weapi_encrypt(&params, Some(FIXED_KEY));
        assert_eq!(result.params, GOLDEN_WEAPI_GET_PARAMS);
        assert_eq!(result.enc_sec_key, GOLDEN_RSA_RAW_HEX);
    }

    #[test]
    fn test_weapi_cloud_del() {
        let params = json!({"songIds": "[123456]"});
        let result = weapi_encrypt(&params, Some(FIXED_KEY));
        assert_eq!(result.params, GOLDEN_WEAPI_DEL_PARAMS);
        assert_eq!(result.enc_sec_key, GOLDEN_RSA_RAW_HEX);
    }

    #[test]
    fn test_weapi_random_key_different() {
        let params = json!({"limit": "1", "offset": "0"});
        let r1 = weapi_encrypt(&params, None);
        let r2 = weapi_encrypt(&params, None);
        assert_ne!(r1.params, r2.params);
        assert_ne!(r1.enc_sec_key, r2.enc_sec_key);
        assert_eq!(r1.enc_sec_key.len(), 256);
        assert_eq!(r2.enc_sec_key.len(), 256);
    }

    #[test]
    fn test_linuxapi_golden() {
        // Dart test uses key order: url, method, params (worker order)
        // We need to manually construct JSON to ensure key order
        let json_str = r#"{"url":"https://music.163.com/api/song/enhance/player/url","method":"POST","params":{"ids":"[123456]","br":"999000"}}"#;
        let payload: Value = serde_json::from_str(json_str).unwrap();
        let eparams = linuxapi_encrypt(&payload);
        assert_eq!(eparams, GOLDEN_LINUXAPI);
    }

    #[test]
    fn test_linuxapi_uppercase_hex() {
        let payload = json!({"url": "u", "method": "POST"});
        let eparams = linuxapi_encrypt(&payload);
        assert!(eparams.chars().all(|c| c.is_ascii_hexdigit() && (c.is_uppercase() || c.is_digit(10))));
        assert_eq!(eparams.len() % 32, 0);
    }
}
