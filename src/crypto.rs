use anyhow::{anyhow, Context, Result};
use curve25519_dalek::montgomery::MontgomeryPoint;
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

/// Stable per-person colours. Picked to stay apart from each other on a dark
/// terminal, and to keep clear of the red that reads as an error. Derived from
/// the author id, so every machine shows the same person in the same colour
/// without anyone configuring anything.
const PALETTE: &[(u8, u8, u8)] = &[
    (130, 180, 235),
    (150, 205, 150),
    (225, 190, 120),
    (200, 160, 225),
    (130, 210, 205),
    (230, 170, 150),
    (190, 205, 130),
    (215, 165, 195),
];

pub fn color_for(author: &[u8; 32]) -> (u8, u8, u8) {
    let n = u32::from_le_bytes(author[0..4].try_into().unwrap()) as usize;
    PALETTE[n % PALETTE.len()]
}

/// X25519 secret for whispers, derived from the Ed25519 device seed. Deriving
/// rather than reusing the signing key keeps signing and encryption apart, and
/// costs nothing: both come out of the same file.
pub fn whisper_secret(device_seed: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("local-llm x25519 v1", device_seed)
}

pub fn whisper_public(secret: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint::mul_base_clamped(*secret).0
}

/// Symmetric key shared by exactly two people. Both sides derive the same one,
/// which is what lets the sender reread what they sent. The price is no
/// forward secrecy: someone who later steals a device key can open old
/// whispers it took part in.
pub fn whisper_key(
    secret: &[u8; 32],
    their_x_pub: &[u8; 32],
    me: &[u8; 32],
    them: &[u8; 32],
) -> RoomKey {
    let shared = MontgomeryPoint(*their_x_pub).mul_clamped(*secret);
    // Canonical order, so both ends mix the identities the same way round.
    let (first, second) = if me <= them { (me, them) } else { (them, me) };
    let mut material = Vec::with_capacity(96);
    material.extend_from_slice(&shared.0);
    material.extend_from_slice(first);
    material.extend_from_slice(second);
    RoomKey(blake3::derive_key("local-llm whisper v1", &material))
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
    fn whisper_keys_agree_between_the_two_sides() {
        let a_seed = [7u8; 32];
        let b_seed = [9u8; 32];
        let (a_id, b_id) = ([1u8; 32], [2u8; 32]);
        let (a_sec, b_sec) = (whisper_secret(&a_seed), whisper_secret(&b_seed));
        let (a_pub, b_pub) = (whisper_public(&a_sec), whisper_public(&b_sec));

        let from_a = whisper_key(&a_sec, &b_pub, &a_id, &b_id);
        let from_b = whisper_key(&b_sec, &a_pub, &b_id, &a_id);
        let sealed = from_a.seal(b"segredo").unwrap();
        assert_eq!(from_b.open(&sealed).unwrap(), b"segredo");
        // And the sender can reread it, which is the point of a static ECDH.
        assert_eq!(from_a.open(&sealed).unwrap(), b"segredo");

        // A third person with the room key gets nothing.
        let c_sec = whisper_secret(&[3u8; 32]);
        let outsider = whisper_key(&c_sec, &b_pub, &[3u8; 32], &b_id);
        assert!(outsider.open(&sealed).is_err());
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
