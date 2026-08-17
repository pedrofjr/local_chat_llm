use anyhow::{anyhow, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;
use zeroize::Zeroize;

pub const PIN_CHARS: usize = 8;
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin(String);

impl Pin {
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let mut raw = [0u8; PIN_CHARS];
        for slot in &mut raw {
            *slot = CROCKFORD[rng.next_u32() as usize % CROCKFORD.len()];
        }
        Self(String::from_utf8(raw.to_vec()).expect("crockford is ascii"))
    }

    pub fn parse(input: &str) -> Result<Self> {
        let normalized = normalize_pin(input);
        if normalized.len() != PIN_CHARS {
            return Err(anyhow!(
                "pin must be {PIN_CHARS} characters (example 7K2M-9QXP)"
            ));
        }
        if !normalized.bytes().all(|b| CROCKFORD.contains(&b)) {
            return Err(anyhow!("pin has invalid characters"));
        }
        Ok(Self(normalized))
    }

    pub fn normalized(&self) -> &str {
        &self.0
    }

    pub fn display(&self) -> String {
        format!("{}-{}", &self.0[..4], &self.0[4..])
    }
}

pub fn normalize_pin(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

pub fn topic_id(pin: &Pin) -> [u8; 32] {
    *blake3::hash(format!("local-llm/v1{}", pin.normalized()).as_bytes()).as_bytes()
}

pub fn topic_hex(id: &[u8; 32]) -> String {
    data_encoding::HEXLOWER.encode(id)
}

#[derive(Clone)]
pub struct RoomKey([u8; 32]);

impl Drop for RoomKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl RoomKey {
    pub fn derive(pin: &Pin) -> Result<Self> {
        let salt = blake3::hash(format!("local-llm/kdf{}", pin.normalized()).as_bytes());
        let params = Params::new(32 * 1024, 3, 1, Some(32)).context("argon2 params")?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut key = [0u8; 32];
        argon
            .hash_password_into(
                pin.normalized().as_bytes(),
                &salt.as_bytes()[..16],
                &mut key,
            )
            .map_err(|e| anyhow!("argon2: {e}"))?;
        Ok(Self(key))
    }

    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher =
            ChaCha20Poly1305::new_from_slice(&self.0).map_err(|e| anyhow!("chacha key: {e}"))?;
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from(nonce_bytes);
        let mut out = nonce_bytes.to_vec();
        let ct = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| anyhow!("encrypt: {e}"))?;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < 13 {
            return Err(anyhow!("ciphertext too short"));
        }
        let cipher =
            ChaCha20Poly1305::new_from_slice(&self.0).map_err(|e| anyhow!("chacha key: {e}"))?;
        let nonce = Nonce::from_slice(&sealed[..12]);
        cipher
            .decrypt(nonce, &sealed[12..])
            .map_err(|_| anyhow!("wrong pin or corrupt log"))
    }
}

pub fn role_for(author: &[u8; 32]) -> &'static str {
    const ROLES: &[&str] = &[
        "assistant",
        "gpt-oss",
        "qwen",
        "llama",
        "mistral",
        "phi",
        "gemma",
        "deepseek",
    ];
    let n = u32::from_le_bytes(author[0..4].try_into().unwrap()) as usize;
    ROLES[n % ROLES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_roundtrip_and_normalize() {
        let pin = Pin::parse("7k2m-9qxp").unwrap();
        assert_eq!(pin.normalized(), "7K2M9QXP");
        assert_eq!(pin.display(), "7K2M-9QXP");
        assert_eq!(normalize_pin("7k2m 9qxp"), "7K2M9QXP");
        assert_eq!(normalize_pin("OI-IL"), "0111");
        assert!(Pin::parse("short").is_err());
    }

    #[test]
    fn topic_is_stable() {
        let a = Pin::parse("7K2M-9QXP").unwrap();
        let b = Pin::parse("7k2m9qxp").unwrap();
        assert_eq!(topic_id(&a), topic_id(&b));
    }

    #[test]
    fn seal_open() {
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let key = RoomKey::derive(&pin).unwrap();
        let sealed = key.seal(b"hello room").unwrap();
        assert_eq!(key.open(&sealed).unwrap(), b"hello room");
        let other = RoomKey::derive(&Pin::parse("AAAA-BBBB").unwrap()).unwrap();
        assert!(other.open(&sealed).is_err());
    }

    #[test]
    fn generate_valid_pin() {
        let pin = Pin::generate();
        assert_eq!(pin.normalized().len(), 8);
        Pin::parse(&pin.display()).unwrap();
    }
}
