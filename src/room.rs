use crate::crypto::{role_for, Pin};
use crate::store::{now_ts, DataDir, Record, RoomLog};
use anyhow::{anyhow, Result};
use iroh::{PublicKey, SecretKey};

pub struct OpenRoom {
    pub pin: Pin,
    pub log: RoomLog,
    pub secret: SecretKey,
    pub author: [u8; 32],
    pub nick: String,
}

impl OpenRoom {
    pub fn create(dir: &DataDir, alias: &str) -> Result<Self> {
        let pin = Pin::generate();
        Self::open(dir, pin, Some(alias))
    }

    pub fn join(dir: &DataDir, pin: Pin, alias: Option<&str>) -> Result<Self> {
        Self::open(dir, pin, alias)
    }

    fn open(dir: &DataDir, pin: Pin, alias: Option<&str>) -> Result<Self> {
        let secret = load_or_create_device_key(dir)?;
        let author = *secret.public().as_bytes();
        let log = RoomLog::open_or_create(dir, &pin, alias)?;
        Ok(Self {
            pin,
            log,
            secret,
            author,
            nick: dir.load_nick(),
        })
    }

    pub fn alias(&self) -> String {
        self.log.alias().unwrap_or_else(|| "session".into())
    }

    pub fn set_nick(&mut self, dir: &DataDir, nick: String) -> Result<()> {
        let nick = normalize_nick(&nick)?;
        dir.save_nick(&nick)?;
        self.nick = nick;
        Ok(())
    }

    pub fn compose(&mut self, body: String) -> Result<Record> {
        let seq = self.log.next_seq_for(&self.author);
        let ts = now_ts();
        let name = self.nick.clone();
        let sig = self.secret.sign(&sign_payload(&body, seq, ts, Some(&name)));
        let rec = Record::ChatNamed {
            author: self.author,
            seq,
            ts,
            name,
            body,
            sig: sig.to_bytes().to_vec(),
        };
        self.log.append(rec.clone())?;
        Ok(rec)
    }

    pub fn ingest(&mut self, rec: Record) -> Result<bool> {
        match &rec {
            Record::Chat {
                author,
                seq,
                ts,
                body,
                sig,
            } => verify_sig(author, sig, &sign_payload(body, *seq, *ts, None))?,
            Record::ChatNamed {
                author,
                seq,
                ts,
                name,
                body,
                sig,
            } => verify_sig(author, sig, &sign_payload(body, *seq, *ts, Some(name)))?,
            Record::Meta { .. } => {}
        }
        self.log.append(rec)
    }

    pub fn label_of(&self, rec: &Record) -> String {
        match rec {
            Record::ChatNamed { name, .. } => name.clone(),
            Record::Chat { author, .. } => {
                if *author == self.author {
                    self.nick.clone()
                } else {
                    role_for(author).to_string()
                }
            }
            Record::Meta { .. } => "system".into(),
        }
    }

    pub fn is_mine(&self, rec: &Record) -> bool {
        rec.author().is_some_and(|a| *a == self.author)
    }
}

pub fn normalize_nick(raw: &str) -> Result<String> {
    let nick = raw.trim();
    if nick.is_empty() {
        return Err(anyhow!("nick cannot be empty"));
    }
    if nick.chars().count() > 24 {
        return Err(anyhow!("nick max 24 characters"));
    }
    if nick.contains(['\n', '\r', '/']) {
        return Err(anyhow!("nick cannot contain / or line breaks"));
    }
    Ok(nick.to_string())
}

fn sign_payload(body: &str, seq: u64, ts: u64, name: Option<&str>) -> Vec<u8> {
    let mut unsigned = body.as_bytes().to_vec();
    unsigned.extend_from_slice(&seq.to_le_bytes());
    unsigned.extend_from_slice(&ts.to_le_bytes());
    if let Some(name) = name {
        unsigned.extend_from_slice(name.as_bytes());
    }
    unsigned
}

fn verify_sig(author: &[u8; 32], sig: &[u8], payload: &[u8]) -> Result<()> {
    let pk = PublicKey::from_bytes(author)?;
    let sig_arr: [u8; 64] = sig
        .try_into()
        .map_err(|_| anyhow!("bad signature length"))?;
    pk.verify(payload, &iroh::Signature::from_bytes(&sig_arr))?;
    Ok(())
}

fn load_or_create_device_key(dir: &DataDir) -> Result<SecretKey> {
    let path = dir.device_key_path();
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let arr: [u8; 32] = bytes.as_slice().try_into()?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    std::fs::write(path, key.to_bytes())?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::normalize_nick;

    #[test]
    fn nick_rules() {
        assert_eq!(normalize_nick("  Diamante  ").unwrap(), "Diamante");
        assert!(normalize_nick("").is_err());
        assert!(normalize_nick("no/slash").is_err());
        assert!(normalize_nick(&"x".repeat(25)).is_err());
    }
}
