use crate::crypto::{topic_hex, Pin, RoomKey};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;

pub const MAGIC: &[u8; 6] = b"LLLM1\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Record {
    Meta {
        alias: String,
    },
    Chat {
        author: [u8; 32],
        seq: u64,
        ts: u64,
        body: String,
        sig: Vec<u8>,
    },
    ChatNamed {
        author: [u8; 32],
        seq: u64,
        ts: u64,
        name: String,
        body: String,
        sig: Vec<u8>,
    },
    /// Published once per person: the X25519 public key used to seal whispers
    /// to them, signed by their Ed25519 identity so nobody can plant a key in
    /// someone else's name.
    Identity {
        author: [u8; 32],
        x_pub: [u8; 32],
        sig: Vec<u8>,
    },
    /// What the app writes from v2 on: a chat message that may answer another.
    Post {
        author: [u8; 32],
        seq: u64,
        ts: u64,
        name: String,
        body: String,
        reply_to: Option<([u8; 32], u64)>,
        sig: Vec<u8>,
    },
    /// A message only `to` can read. Everyone else keeps the bytes and can
    /// still check the signature, but the plaintext is not theirs to have.
    Whisper {
        author: [u8; 32],
        seq: u64,
        ts: u64,
        to: [u8; 32],
        /// Sealed with the pair key; the nonce rides in the first bytes, the
        /// same way the room log stores its own records.
        ct: Vec<u8>,
        sig: Vec<u8>,
    },
}

impl Record {
    pub fn encode(&self) -> Result<Vec<u8>> {
        postcard::to_stdvec(self).context("encode record")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).context("decode record")
    }

    /// Whispers share the author's sequence with ordinary messages on
    /// purpose: one counter per person means the two can never collide.
    pub fn chat_key(&self) -> Option<([u8; 32], u64)> {
        match self {
            Record::Chat { author, seq, .. }
            | Record::ChatNamed { author, seq, .. }
            | Record::Post { author, seq, .. }
            | Record::Whisper { author, seq, .. } => Some((*author, *seq)),
            Record::Meta { .. } | Record::Identity { .. } => None,
        }
    }

    /// Readable text. A whisper has none until it is opened with the right
    /// key, so it deliberately answers `None` here.
    pub fn body(&self) -> Option<&str> {
        match self {
            Record::Chat { body, .. }
            | Record::ChatNamed { body, .. }
            | Record::Post { body, .. } => Some(body),
            Record::Meta { .. } | Record::Identity { .. } | Record::Whisper { .. } => None,
        }
    }

    pub fn author(&self) -> Option<&[u8; 32]> {
        match self {
            Record::Chat { author, .. }
            | Record::ChatNamed { author, .. }
            | Record::Post { author, .. }
            | Record::Whisper { author, .. }
            | Record::Identity { author, .. } => Some(author),
            Record::Meta { .. } => None,
        }
    }

    pub fn reply_to(&self) -> Option<([u8; 32], u64)> {
        match self {
            Record::Post { reply_to, .. } => *reply_to,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct IndexFile {
    sessions: Vec<IndexEntry>,
}

/// When the terminal bell is allowed to ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Notify {
    #[default]
    All,
    Mention,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub notify: Notify,
    /// Unix seconds until which the bell stays quiet regardless of `notify`.
    pub snooze_until: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub alias: String,
    pub topic: String,
}

pub struct DataDir {
    root: PathBuf,
    base: PathBuf,
    pub instance: u8,
    _slot: Option<TcpListener>,
}

fn claim_instance_slot() -> Result<(u8, TcpListener)> {
    for n in 1u8..=8 {
        let port = 41770 + u16::from(n);
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            let _ = listener.set_nonblocking(true);
            return Ok((n, listener));
        }
    }
    Err(anyhow!("too many local-llm windows open (max 8)"))
}

impl DataDir {
    pub fn open() -> Result<Self> {
        let base = if let Ok(custom) = std::env::var("LOCAL_LLM_HOME") {
            PathBuf::from(custom)
        } else {
            directories::ProjectDirs::from("dev", "local-llm", "local-llm")
                .map(|d| d.data_local_dir().to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".local-llm"))
        };
        let (instance, slot) = claim_instance_slot()?;
        let root = if instance == 1 {
            base.clone()
        } else {
            base.join(format!("guest-{instance}"))
        };
        fs::create_dir_all(root.join("rooms")).context("create data dir")?;
        Ok(Self {
            root,
            base,
            instance,
            _slot: Some(slot),
        })
    }

    #[cfg(test)]
    pub fn from_path(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("rooms"))?;
        Ok(Self {
            base: root.clone(),
            root,
            instance: 1,
            _slot: None,
        })
    }

    pub fn presence_dir(&self) -> PathBuf {
        self.base.join("presence")
    }

    pub fn nick_path(&self) -> PathBuf {
        self.root.join("nick")
    }

    pub fn load_nick(&self) -> String {
        fs::read_to_string(self.nick_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "user".into())
    }

    pub fn save_nick(&self, nick: &str) -> Result<()> {
        fs::write(self.nick_path(), nick.trim()).context("save nick")
    }

    pub fn device_key_path(&self) -> PathBuf {
        self.root.join("device.key")
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.toml")
    }

    /// Missing or unreadable settings fall back to the defaults rather than
    /// failing: a corrupt preferences file must never keep the chat shut.
    pub fn load_settings(&self) -> Settings {
        fs::read_to_string(self.settings_path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        let text = toml::to_string_pretty(settings).context("encode settings")?;
        fs::write(self.settings_path(), text).context("save settings")
    }

    fn pin_path(&self, topic: &[u8; 32]) -> PathBuf {
        self.room_dir(topic).join("pin.dpapi")
    }

    /// Stores the pin under DPAPI so this Windows user can reopen the room
    /// without retyping it. The topic doubles as entropy, so a blob lifted
    /// from one room cannot unlock another.
    pub fn remember_pin(&self, topic: &[u8; 32], pin: &Pin) -> Result<()> {
        let sealed = crate::sys::protect(pin.normalized().as_bytes(), topic)
            .ok_or_else(|| anyhow!("windows refused to protect the key"))?;
        fs::create_dir_all(self.room_dir(topic))?;
        fs::write(self.pin_path(topic), sealed).context("save protected pin")
    }

    pub fn recall_pin(&self, topic: &[u8; 32]) -> Option<Pin> {
        let sealed = fs::read(self.pin_path(topic)).ok()?;
        let plain = crate::sys::unprotect(&sealed, topic)?;
        Pin::parse(&String::from_utf8(plain).ok()?).ok()
    }

    pub fn has_pin(&self, topic: &[u8; 32]) -> bool {
        self.pin_path(topic).exists()
    }

    pub fn forget_pin(&self, topic: &[u8; 32]) -> Result<()> {
        let path = self.pin_path(topic);
        if path.exists() {
            fs::remove_file(path).context("remove protected pin")?;
        }
        Ok(())
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.toml")
    }

    pub fn room_dir(&self, topic: &[u8; 32]) -> PathBuf {
        self.root.join("rooms").join(topic_hex(topic))
    }

    pub fn list_sessions(&self) -> Result<Vec<IndexEntry>> {
        if !self.index_path().exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(self.index_path())?;
        let file: IndexFile = toml::from_str(&text).context("parse index.toml")?;
        Ok(file.sessions)
    }

    pub fn upsert_session(&self, alias: &str, topic: &[u8; 32]) -> Result<()> {
        let hex = topic_hex(topic);
        let mut sessions = self.list_sessions()?;
        if let Some(existing) = sessions.iter_mut().find(|s| s.topic == hex) {
            if !alias.is_empty() {
                existing.alias = alias.to_string();
            }
        } else {
            sessions.push(IndexEntry {
                alias: if alias.is_empty() {
                    "session".into()
                } else {
                    alias.into()
                },
                topic: hex,
            });
        }
        let text = toml::to_string_pretty(&IndexFile { sessions })?;
        fs::write(self.index_path(), text)?;
        Ok(())
    }

    pub fn forget(&self, topic: &[u8; 32]) -> Result<()> {
        let hex = topic_hex(topic);
        let sessions: Vec<_> = self
            .list_sessions()?
            .into_iter()
            .filter(|s| s.topic != hex)
            .collect();
        fs::write(
            self.index_path(),
            toml::to_string_pretty(&IndexFile { sessions })?,
        )?;
        let dir = self.room_dir(topic);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
}

pub struct RoomLog {
    path: PathBuf,
    key: RoomKey,
    records: Vec<Record>,
    seen: BTreeMap<([u8; 32], u64), ()>,
}

impl RoomLog {
    pub fn open_or_create(dir: &DataDir, pin: &Pin, alias: Option<&str>) -> Result<Self> {
        let topic = crate::crypto::topic_id(pin);
        let key = RoomKey::derive(pin)?;
        let room = dir.room_dir(&topic);
        fs::create_dir_all(&room)?;
        let path = room.join("log.bin");
        let log = if path.exists() {
            Self::load(path, key)?
        } else {
            let mut log = Self {
                path,
                key,
                records: Vec::new(),
                seen: BTreeMap::new(),
            };
            if let Some(alias) = alias {
                log.append(Record::Meta {
                    alias: alias.to_string(),
                })?;
            }
            log
        };
        let shown = log
            .alias()
            .unwrap_or_else(|| alias.unwrap_or("session").to_string());
        dir.upsert_session(&shown, &topic)?;
        Ok(log)
    }

    fn load(path: PathBuf, key: RoomKey) -> Result<Self> {
        let bytes = fs::read(&path).context("read log")?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(anyhow!("not a local-llm log"));
        }
        let mut log = Self {
            path,
            key,
            records: Vec::new(),
            seen: BTreeMap::new(),
        };
        let mut duplicates = 0usize;
        let mut unreadable = 0usize;
        let mut offset = MAGIC.len();
        while offset + 4 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + len > bytes.len() {
                return Err(anyhow!("truncated log"));
            }
            let plain = log.key.open(&bytes[offset..offset + len])?;
            offset += len;
            // A record this build cannot parse is almost certainly one a newer
            // build wrote. Skipping beats refusing to open the room.
            let Ok(rec) = Record::decode(&plain) else {
                unreadable += 1;
                continue;
            };
            if log.is_new(&rec) {
                log.remember(rec);
            } else {
                duplicates += 1;
            }
        }
        // Older builds appended a fresh Meta on every sync round, so logs in
        // the wild carry thousands of copies. Rewrite them once, on open --
        // but never while holding records we could not parse, since rewriting
        // from memory would silently delete them.
        if duplicates > 0 && unreadable == 0 {
            log.rewrite().context("compact log")?;
        }
        Ok(log)
    }

    /// A chat record is identified by (author, seq); a Meta by its alias. The
    /// sync protocol has no way to tell that the other side already has a
    /// Meta, so this is the only thing standing between it and an
    /// ever-growing log.
    fn is_new(&self, rec: &Record) -> bool {
        match rec.chat_key() {
            Some(key) => !self.seen.contains_key(&key),
            None => match rec {
                Record::Meta { alias } => !self
                    .records
                    .iter()
                    .any(|held| matches!(held, Record::Meta { alias: had } if had == alias)),
                // One published key per person; the sync resends it forever.
                Record::Identity { author, .. } => !self.records.iter().any(
                    |held| matches!(held, Record::Identity { author: had, .. } if had == author),
                ),
                _ => true,
            },
        }
    }

    fn remember(&mut self, rec: Record) {
        if let Some(key) = rec.chat_key() {
            self.seen.insert(key, ());
        }
        self.records.push(rec);
    }

    /// Rewrites the whole log from what is in memory, atomically.
    fn rewrite(&self) -> Result<()> {
        let mut bytes = MAGIC.to_vec();
        for rec in &self.records {
            let sealed = self.key.seal(&rec.encode()?)?;
            let len = u32::try_from(sealed.len()).context("record too large")?;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(&sealed);
        }
        let staging = self.path.with_extension("bin.new");
        fs::write(&staging, &bytes)?;
        fs::rename(&staging, &self.path)?;
        Ok(())
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    fn side_path(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    /// Encrypted scratch file next to the log, for things that belong to the
    /// room without being part of its history: known peer addresses, local
    /// preferences. Same key as the log, because a list of who you talk to and
    /// where they live is as sensitive as what was said.
    pub fn write_side(&self, name: &str, plain: &[u8]) -> Result<()> {
        let sealed = self.key.seal(plain)?;
        fs::write(self.side_path(name), sealed).with_context(|| format!("write {name}"))
    }

    /// Missing, corrupt or wrong-key files read as absent. None of these are
    /// worth refusing to open a room over.
    pub fn read_side(&self, name: &str) -> Option<Vec<u8>> {
        let sealed = fs::read(self.side_path(name)).ok()?;
        self.key.open(&sealed).ok()
    }

    pub fn alias(&self) -> Option<String> {
        self.records.iter().rev().find_map(|r| match r {
            Record::Meta { alias } => Some(alias.clone()),
            _ => None,
        })
    }

    pub fn next_seq_for(&self, author: &[u8; 32]) -> u64 {
        self.heads().get(author).map(|s| s + 1).unwrap_or(0)
    }

    pub fn heads(&self) -> BTreeMap<[u8; 32], u64> {
        let mut heads = BTreeMap::new();
        for rec in &self.records {
            if let Some((author, seq)) = rec.chat_key() {
                heads
                    .entry(author)
                    .and_modify(|s: &mut u64| *s = (*s).max(seq))
                    .or_insert(seq);
            }
        }
        heads
    }

    pub fn missing_for(&self, their: &BTreeMap<[u8; 32], u64>) -> Vec<Record> {
        self.records
            .iter()
            .filter(|rec| match rec.chat_key() {
                None => matches!(rec, Record::Meta { .. } | Record::Identity { .. }),
                Some((author, seq)) => their.get(&author).is_none_or(|h| seq > *h),
            })
            .cloned()
            .collect()
    }

    pub fn append(&mut self, rec: Record) -> Result<bool> {
        if !self.is_new(&rec) {
            return Ok(false);
        }
        let sealed = self.key.seal(&rec.encode()?)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(MAGIC)?;
        }
        let len = u32::try_from(sealed.len()).context("record too large")?;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(&sealed)?;
        file.flush()?;
        self.remember(rec);
        Ok(true)
    }

    #[cfg(test)]
    pub fn merge(&mut self, incoming: Vec<Record>) -> Result<usize> {
        let mut added = 0;
        for rec in incoming {
            if self.append(rec)? {
                added += 1;
            }
        }
        Ok(added)
    }
}

pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Pin;
    use tempfile::TempDir;

    #[test]
    fn persist_and_reload() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        {
            let mut log = RoomLog::open_or_create(&dir, &pin, Some("gpt-oss-20b")).unwrap();
            log.append(Record::Chat {
                author: [1u8; 32],
                seq: 0,
                ts: 1,
                body: "hi".into(),
                sig: vec![0u8; 64],
            })
            .unwrap();
        }
        let log = RoomLog::open_or_create(&dir, &pin, None).unwrap();
        assert_eq!(log.alias().as_deref(), Some("gpt-oss-20b"));
        assert_eq!(log.records().len(), 2);
        match &log.records()[1] {
            Record::Chat { body, .. } => assert_eq!(body, "hi"),
            _ => panic!("expected chat"),
        }
        let sessions = dir.list_sessions().unwrap();
        assert_eq!(sessions[0].alias, "gpt-oss-20b");
    }

    #[test]
    fn merge_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("AAAA-BBBB").unwrap();
        let mut a = RoomLog::open_or_create(&dir, &pin, Some("qwen")).unwrap();
        let rec = Record::Chat {
            author: [2u8; 32],
            seq: 3,
            ts: 9,
            body: "once".into(),
            sig: vec![7u8; 64],
        };
        assert_eq!(a.merge(vec![rec.clone()]).unwrap(), 1);
        assert_eq!(a.merge(vec![rec]).unwrap(), 0);
        assert_eq!(a.heads().get(&[2u8; 32]).copied(), Some(3));
    }

    #[test]
    fn nick_persists() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        assert_eq!(dir.load_nick(), "user");
        dir.save_nick("Diamante").unwrap();
        assert_eq!(dir.load_nick(), "Diamante");
    }

    #[test]
    fn side_files_are_sealed_with_the_room_key() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let log = RoomLog::open_or_create(&dir, &pin, Some("sala")).unwrap();

        log.write_side("peers.bin", b"enderecos dos amigos").unwrap();
        assert_eq!(
            log.read_side("peers.bin").as_deref(),
            Some(&b"enderecos dos amigos"[..])
        );

        // On disk it must not be readable.
        let raw = fs::read(dir.room_dir(&crate::crypto::topic_id(&pin)).join("peers.bin")).unwrap();
        assert!(!raw.windows(9).any(|w| w == b"enderecos"));

        // And a different room cannot open it.
        let other = Pin::parse("AAAA-BBBB").unwrap();
        let elsewhere = RoomLog::open_or_create(&dir, &other, Some("outra")).unwrap();
        fs::copy(
            dir.room_dir(&crate::crypto::topic_id(&pin)).join("peers.bin"),
            dir.room_dir(&crate::crypto::topic_id(&other)).join("peers.bin"),
        )
        .unwrap();
        assert!(elsewhere.read_side("peers.bin").is_none());

        assert!(log.read_side("nao-existe.bin").is_none());
    }

    #[test]
    fn repeated_meta_from_sync_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("EEEE-FFFF").unwrap();
        let mut log = RoomLog::open_or_create(&dir, &pin, Some("qwen")).unwrap();
        assert_eq!(log.records().len(), 1);

        // Every sync round hands us the peer's Meta again.
        for _ in 0..50 {
            assert!(!log
                .append(Record::Meta {
                    alias: "qwen".into()
                })
                .unwrap());
        }
        assert_eq!(log.records().len(), 1);
    }

    #[test]
    fn reopening_compacts_a_log_polluted_by_the_old_sync_bug() {
        use crate::crypto::RoomKey;

        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("GGGG-HHHH").unwrap();
        let topic = crate::crypto::topic_id(&pin);
        let key = RoomKey::derive(&pin).unwrap();
        let room = dir.room_dir(&topic);
        fs::create_dir_all(&room).unwrap();
        let path = room.join("log.bin");

        let mut bytes = MAGIC.to_vec();
        for _ in 0..500 {
            let sealed = key
                .seal(
                    &Record::Meta {
                        alias: "teste".into(),
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap();
            bytes.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&sealed);
        }
        fs::write(&path, &bytes).unwrap();
        let bloated = fs::metadata(&path).unwrap().len();

        let log = RoomLog::open_or_create(&dir, &pin, None).unwrap();
        assert_eq!(log.records().len(), 1, "should collapse to one Meta");
        assert!(
            fs::metadata(&path).unwrap().len() < bloated / 10,
            "the file itself should shrink, not just the in-memory view"
        );
    }

    #[test]
    fn forget_wipes_room() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("CCCC-DDDD").unwrap();
        let topic = crate::crypto::topic_id(&pin);
        RoomLog::open_or_create(&dir, &pin, Some("gone")).unwrap();
        assert!(!dir.list_sessions().unwrap().is_empty());
        dir.forget(&topic).unwrap();
        assert!(dir.list_sessions().unwrap().is_empty());
        assert!(!dir.room_dir(&topic).exists());
    }
}
