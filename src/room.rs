use crate::crypto::{role_for, Pin};
use crate::store::{now_ts, DataDir, Record, RoomLog};
use anyhow::Result;
use iroh::{PublicKey, SecretKey};

pub struct OpenRoom {
    pub pin: Pin,
    pub log: RoomLog,
    pub secret: SecretKey,
    pub author: [u8; 32],
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
        })
    }

    pub fn alias(&self) -> String {
        self.log.alias().unwrap_or_else(|| "session".into())
    }

    pub fn compose(&mut self, body: String) -> Result<Record> {
        let seq = self.log.next_seq_for(&self.author);
        let ts = now_ts();
        let mut unsigned = body.as_bytes().to_vec();
        unsigned.extend_from_slice(&seq.to_le_bytes());
        unsigned.extend_from_slice(&ts.to_le_bytes());
        let sig = self.secret.sign(&unsigned);
        let rec = Record::Chat {
            author: self.author,
            seq,
            ts,
            body,
            sig: sig.to_bytes().to_vec(),
        };
        self.log.append(rec.clone())?;
        Ok(rec)
    }

    pub fn ingest(&mut self, rec: Record) -> Result<bool> {
        if let Record::Chat {
            author,
            seq,
            ts,
            body,
            sig,
        } = &rec
        {
            let pk = PublicKey::from_bytes(author)?;
            let mut unsigned = body.as_bytes().to_vec();
            unsigned.extend_from_slice(&seq.to_le_bytes());
            unsigned.extend_from_slice(&ts.to_le_bytes());
            let sig_arr: [u8; 64] = sig
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("bad signature length"))?;
            pk.verify(&unsigned, &iroh::Signature::from_bytes(&sig_arr))?;
        }
        self.log.append(rec)
    }

    pub fn role_of(&self, author: &[u8; 32]) -> &'static str {
        if *author == self.author {
            "user"
        } else {
            role_for(author)
        }
    }
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
