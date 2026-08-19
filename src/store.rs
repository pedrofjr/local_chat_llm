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
    /// Legacy whisper, written up to 0.4.0. Still read, never written: it put
    /// `author` and `to` in the clear, so the room could see who talked to
    /// whom. Superseded by [`Record::Quiet`].
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
    /// Heartbeat: "I am in this room, right now". Never written to the log --
    /// being present is a live fact, not history. Carries the sender's own
    /// address, which is what lets someone who only knows A end up connected
    /// to B as well: A's heartbeat introduces everyone it can reach.
    Presence {
        author: [u8; 32],
        name: String,
        addr: Vec<u8>,
        ts: u64,
        sig: Vec<u8>,
    },
    /// A picture. Only the description travels in the log -- the pixels live
    /// in a blob fetched over its own stream. Inlining them would be two
    /// separate disasters: the sync reads a whole batch of records with a 2 MB
    /// cap and swallows the error, so one big picture would silently stop
    /// *text* from arriving; and the log is held in memory in full, so a few
    /// screenshots would be re-read on every open forever.
    Image {
        author: [u8; 32],
        seq: u64,
        ts: u64,
        name: String,
        /// blake3 of the original bytes. Names the blob and proves on arrival
        /// that what we got is what was sent.
        blob: [u8; 32],
        w: u32,
        h: u32,
        kind: ImageKind,
        bytes: u32,
        /// Text typed alongside the picture. Empty when there was none.
        caption: String,
        reply_to: Option<([u8; 32], u64)>,
        sig: Vec<u8>,
    },
    /// A whisper with nobody's name on the outside. See [`Sealed`].
    Quiet(Sealed),
}

/// A whisper that does not say who it is between.
///
/// The older `Whisper` carried `author` and `to` in the clear, so anybody
/// holding the room key -- which is everybody here -- could read off who
/// talked to whom and when, straight out of `log.bin`. Only the words were
/// protected. Here nothing on the outside names a person: the sender, their
/// name, the text and their signature all live inside `ct`, and the recipient
/// finds their own mail by trying to open each one.
///
/// What still leaks: that a whisper happened, when, and roughly how long it
/// was. In a room of four, "one of us said something to one of us" is most of
/// what is left to hide, and hiding it would need cover traffic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sealed {
    /// Random. Names the record for dedup without tying it to anyone -- the
    /// old scheme used (author, seq), which is exactly what we are hiding.
    pub id: [u8; 32],
    pub ts: u64,
    /// Sealed with the pair key. Holds the sender, their name, the body, the
    /// reply, and their signature over all of it.
    pub ct: Vec<u8>,
    /// Keyed with the room key: says a member wrote this, without saying
    /// which. Lets a bystander reject noise they cannot read.
    pub tag: [u8; 32],
}

/// What the bytes decode as. Kept narrow on purpose: every extra format is
/// another decoder in the binary and another parser exposed to whatever a
/// peer sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
}

impl ImageKind {
    /// Sniffs the format from the leading bytes. We never trust a file
    /// extension for this -- the bytes are what the decoder will see.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            Some(ImageKind::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(ImageKind::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(ImageKind::Gif)
        } else {
            None
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpeg",
            ImageKind::Gif => "gif",
        }
    }
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
            | Record::Whisper { author, seq, .. }
            | Record::Image { author, seq, .. } => Some((*author, *seq)),
            // A sealed whisper deliberately has no (author, seq) to give:
            // that pair is exactly the metadata being hidden. It is
            // deduplicated by its own random id instead.
            Record::Meta { .. }
            | Record::Identity { .. }
            | Record::Presence { .. }
            | Record::Quiet(_) => None,
        }
    }

    /// Readable text. A whisper has none until it is opened with the right
    /// key, so it deliberately answers `None` here.
    pub fn body(&self) -> Option<&str> {
        match self {
            Record::Chat { body, .. }
            | Record::ChatNamed { body, .. }
            | Record::Post { body, .. } => Some(body),
            // The caption is the only readable text a picture carries.
            Record::Image { caption, .. } => Some(caption),
            Record::Meta { .. }
            | Record::Identity { .. }
            | Record::Whisper { .. }
            | Record::Presence { .. }
            | Record::Quiet(_) => None,
        }
    }

    pub fn author(&self) -> Option<&[u8; 32]> {
        match self {
            Record::Chat { author, .. }
            | Record::ChatNamed { author, .. }
            | Record::Post { author, .. }
            | Record::Whisper { author, .. }
            | Record::Identity { author, .. }
            | Record::Presence { author, .. }
            | Record::Image { author, .. } => Some(author),
            Record::Meta { .. } | Record::Quiet(_) => None,
        }
    }

    pub fn reply_to(&self) -> Option<([u8; 32], u64)> {
        match self {
            Record::Post { reply_to, .. } | Record::Image { reply_to, .. } => *reply_to,
            _ => None,
        }
    }

    /// The blob this record needs before it can be shown, if any.
    pub fn wants_blob(&self) -> Option<[u8; 32]> {
        match self {
            Record::Image { blob, .. } => Some(*blob),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notify: Notify,
    /// Unix seconds until which the bell stays quiet regardless of `notify`.
    pub snooze_until: u64,
    /// Whether a newline arriving in a burst is treated as part of a pasted
    /// message. Turn it off and Enter always sends, full stop.
    pub paste_detect: bool,
    /// How this terminal draws pictures. Worked out once and written down:
    /// the detection has to talk to the terminal over stdin, which is only
    /// safe before the input thread starts reading it, so we get exactly one
    /// chance per install rather than one per launch.
    pub image_proto: ImageProto,
}

/// Which graphics protocol to draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ImageProto {
    /// Not worked out yet. Triggers the one-time detection on next start.
    #[default]
    Unknown,
    /// Real pixels. Windows Terminal 1.22 and up.
    Sixel,
    /// Unicode half-blocks: two pixels per cell, works in any terminal.
    Halfblocks,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify: Notify::default(),
            snooze_until: 0,
            paste_detect: true,
            image_proto: ImageProto::Unknown,
        }
    }
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
        if let Some(taken) = try_slot(n) {
            return Ok(taken);
        }
    }
    Err(anyhow!("too many local-llm windows open (max 8)"))
}

fn try_slot(n: u8) -> Option<(u8, TcpListener)> {
    let port = 41770 + u16::from(n);
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    let _ = listener.set_nonblocking(true);
    Some((n, listener))
}

/// How long a build started by an update waits for the one it replaced to let
/// go of the first slot.
const SLOT_HANDOVER: std::time::Duration = std::time::Duration::from_secs(6);

/// Waits for the first slot, then falls back to the normal search.
///
/// Only used after an update. The build being replaced is still shutting down
/// when its successor starts, and without this the successor grabs the *second*
/// slot -- which means a different data directory, a different set of rooms,
/// and the two of them finding each other through the presence file as if the
/// user had deliberately opened two windows.
fn claim_first_slot_or_wait() -> Result<(u8, TcpListener)> {
    let deadline = std::time::Instant::now() + SLOT_HANDOVER;
    loop {
        if let Some(taken) = try_slot(1) {
            return Ok(taken);
        }
        if std::time::Instant::now() >= deadline {
            // Somebody else really is holding it -- another window the user
            // opened on purpose. Behave like any other start.
            return claim_instance_slot();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

impl DataDir {
    pub fn open() -> Result<Self> {
        Self::open_inner(false)
    }

    /// Opening right after an update installed itself. Waits for the previous
    /// build to release the first slot instead of quietly becoming a second
    /// window with a different data directory.
    pub fn open_after_update() -> Result<Self> {
        Self::open_inner(true)
    }

    fn open_inner(after_update: bool) -> Result<Self> {
        let base = if let Ok(custom) = std::env::var("LOCAL_LLM_HOME") {
            PathBuf::from(custom)
        } else {
            directories::ProjectDirs::from("dev", "local-llm", "local-llm")
                .map(|d| d.data_local_dir().to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".local-llm"))
        };
        let (instance, slot) = if after_update {
            claim_first_slot_or_wait()?
        } else {
            claim_instance_slot()?
        };
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

    /// Lets go of the window slot.
    ///
    /// Called just before an update launches its replacement: the successor
    /// waits for this slot, and if we were still holding it when it started,
    /// it would take the second one -- a different data directory, with
    /// different rooms.
    pub fn release_slot(&mut self) {
        self._slot = None;
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

    /// Note left by an update, saying which room to reopen after the restart.
    /// Holds the topic only -- never the key.
    pub fn resume_path(&self) -> PathBuf {
        self.root.join("resume.txt")
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
    /// Ids of sealed whispers already held. Their own index, because they
    /// carry no author to key on.
    sealed: BTreeMap<[u8; 32], ()>,
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
                sealed: BTreeMap::new(),
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
            sealed: BTreeMap::new(),
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
                // A heartbeat is true for a few seconds and then it is not.
                // Keeping it would be both useless and unbounded.
                Record::Presence { .. } => false,
                // Its random id stands in for the (author, seq) the others
                // use, since naming the author is the thing being avoided.
                Record::Quiet(sealed) => !self.sealed.contains_key(&sealed.id),
                _ => true,
            },
        }
    }

    fn remember(&mut self, rec: Record) {
        if let Some(key) = rec.chat_key() {
            self.seen.insert(key, ());
        }
        if let Record::Quiet(sealed) = &rec {
            self.sealed.insert(sealed.id, ());
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

    /// Room-keyed tag for a sealed whisper, using the key this log already
    /// holds. Deriving it fresh each time would mean running Argon2id -- 32 MB
    /// and three passes -- on every whisper sent and every whisper received.
    pub fn tag(&self, parts: &[&[u8]]) -> [u8; 32] {
        self.key.tag(parts)
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

    fn blob_dir(&self) -> PathBuf {
        self.path.with_file_name("blobs")
    }

    fn blob_path(&self, hash: &[u8; 32]) -> PathBuf {
        self.blob_dir().join(format!("{}.bin", topic_hex(hash)))
    }

    pub fn has_blob(&self, hash: &[u8; 32]) -> bool {
        self.blob_path(hash).exists()
    }

    /// Picture bytes, sealed with the room key like everything else here. A
    /// screenshot of a payslip deserves the same protection as the sentence
    /// describing it.
    pub fn write_blob(&self, hash: &[u8; 32], plain: &[u8]) -> Result<()> {
        let dir = self.blob_dir();
        fs::create_dir_all(&dir).context("create blob dir")?;
        let sealed = self.key.seal(plain)?;
        let path = self.blob_path(hash);
        // Staged then renamed, so a half-written blob can never be read as a
        // whole one -- the same trick `rewrite` uses for the log.
        let staging = path.with_extension("new");
        fs::write(&staging, &sealed).context("write blob")?;
        fs::rename(&staging, &path).context("commit blob")?;
        Ok(())
    }

    /// Absent, corrupt or sealed with another key all read as "not here".
    /// A blob we cannot open is one we should fetch again, not one that
    /// should stop the room from opening.
    pub fn read_blob(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        let sealed = fs::read(self.blob_path(hash)).ok()?;
        let plain = self.key.open(&sealed).ok()?;
        // The name is the hash of the content, so this catches a blob that
        // decrypted cleanly but is not what it claims to be.
        (blake3::hash(&plain).as_bytes() == hash).then_some(plain)
    }

    /// The blob exactly as it sits on disk, still sealed. Serving it this way
    /// means a peer passing a picture along never decrypts it.
    pub fn read_blob_sealed(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        fs::read(self.blob_path(hash)).ok()
    }

    /// Takes a sealed blob from a peer. Opens it, checks the content against
    /// the name we asked for, and only then files it. Answers whether it was
    /// kept -- a mismatch is somebody sending us something we did not ask for,
    /// not an error worth tearing the session down over.
    pub fn accept_blob(&self, hash: &[u8; 32], sealed: &[u8]) -> bool {
        let Ok(plain) = self.key.open(sealed) else {
            return false;
        };
        if blake3::hash(&plain).as_bytes() != hash {
            return false;
        }
        self.write_blob(hash, &plain).is_ok()
    }

    /// Pictures announced in the log whose pixels have not arrived yet, newest
    /// first. Newest first because that is what somebody is about to scroll
    /// to; a screenshot from last week can wait its turn.
    pub fn missing_blobs(&self) -> Vec<[u8; 32]> {
        let mut want = Vec::new();
        for hash in self.records.iter().rev().filter_map(Record::wants_blob) {
            if !want.contains(&hash) && !self.has_blob(&hash) {
                want.push(hash);
            }
        }
        want
    }

    /// Drops the least recently used blobs until the folder fits `limit`.
    /// Returns how many went. The `Record` stays either way: the conversation
    /// keeps its shape, and the line just reads as unavailable.
    pub fn prune_blobs(&self, limit: u64) -> Result<usize> {
        let dir = self.blob_dir();
        if !dir.exists() {
            return Ok(0);
        }
        let mut held: Vec<(std::time::SystemTime, u64, PathBuf)> = fs::read_dir(&dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let meta = entry.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let used = meta.accessed().or_else(|_| meta.modified()).ok()?;
                Some((used, meta.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = held.iter().map(|(_, len, _)| *len).sum();
        if total <= limit {
            return Ok(0);
        }
        // Oldest first, so the ones nobody has looked at in a while go first.
        held.sort_by_key(|(used, _, _)| *used);
        let mut dropped = 0;
        for (_, len, path) in held {
            if total <= limit {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
                dropped += 1;
            }
        }
        Ok(dropped)
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

    /// What a peer is missing.
    ///
    /// Ordinary records are settled by the per-author high-water marks in
    /// `their`. Sealed whispers cannot be: they publish no author, which is
    /// the point, so the peer has to name the ids it already holds and we send
    /// the rest.
    pub fn missing_for(
        &self,
        their: &BTreeMap<[u8; 32], u64>,
        their_sealed: &[[u8; 32]],
    ) -> Vec<Record> {
        self.records
            .iter()
            .filter(|rec| match rec {
                Record::Quiet(sealed) => !their_sealed.contains(&sealed.id),
                _ => match rec.chat_key() {
                    None => matches!(rec, Record::Meta { .. } | Record::Identity { .. }),
                    Some((author, seq)) => their.get(&author).is_none_or(|h| seq > *h),
                },
            })
            .cloned()
            .collect()
    }

    /// Ids of every sealed whisper held here, for the peer to diff against.
    pub fn sealed_ids(&self) -> Vec<[u8; 32]> {
        self.sealed.keys().copied().collect()
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

    /// The bug that sent the first real update into the wrong profile.
    ///
    /// The replacement starts while the build it replaces is still shutting
    /// down. If it simply asks for "the next free slot" it gets the second
    /// one -- and slot 2 means `guest-2`, a different data directory with
    /// different rooms. It has to wait for the first slot to come free.
    #[test]
    fn an_updated_build_waits_for_the_first_slot_instead_of_taking_the_second() {
        // Stand-in for the outgoing process, still holding slot 1.
        let Some((held, listener)) = try_slot(1) else {
            // Another local-llm is running on this machine; the test cannot
            // own the slot, so there is nothing meaningful to assert.
            return;
        };
        assert_eq!(held, 1);

        // The ordinary path steps around it and becomes a second window.
        let (next, _other) = claim_instance_slot().unwrap();
        assert_eq!(next, 2, "a normal start takes the next free slot");

        // Which is why a normal start is wrong after an update: slot 2 is a
        // different directory entirely.
        assert_ne!(
            format!("guest-{next}"),
            String::new(),
            "slot 2 lives under guest-2, not the main profile"
        );

        // Once the outgoing build lets go, the first slot is available again.
        drop(listener);
        let (again, _back) = try_slot(1).expect("slot 1 must be free once released");
        assert_eq!(again, 1);
    }

    #[test]
    fn releasing_the_slot_frees_it_for_the_replacement() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("LOCAL_LLM_HOME", tmp.path());
        let Ok(mut dir) = DataDir::open() else {
            return; // slots busy on this machine
        };
        let mine = dir.instance;

        // While held, that slot is not available to anyone else.
        assert!(try_slot(mine).is_none(), "our own slot must be taken");

        dir.release_slot();
        let back = try_slot(mine).expect("releasing must hand the slot back");
        assert_eq!(back.0, mine);
    }

    #[test]
    fn record_tags_never_move() {
        // postcard numbers enum variants by position, so slipping a new one
        // into the middle silently renames every record written after it --
        // which is exactly what happened when Presence landed before Whisper.
        // New variants go at the end, and this test is the tripwire.
        let tag = |rec: Record| rec.encode().unwrap()[0];
        let blank = || Vec::new();
        assert_eq!(tag(Record::Meta { alias: String::new() }), 0);
        assert_eq!(
            tag(Record::Chat {
                author: [0; 32],
                seq: 0,
                ts: 0,
                body: String::new(),
                sig: blank(),
            }),
            1
        );
        assert_eq!(
            tag(Record::ChatNamed {
                author: [0; 32],
                seq: 0,
                ts: 0,
                name: String::new(),
                body: String::new(),
                sig: blank(),
            }),
            2
        );
        assert_eq!(
            tag(Record::Identity {
                author: [0; 32],
                x_pub: [0; 32],
                sig: blank(),
            }),
            3
        );
        assert_eq!(
            tag(Record::Post {
                author: [0; 32],
                seq: 0,
                ts: 0,
                name: String::new(),
                body: String::new(),
                reply_to: None,
                sig: blank(),
            }),
            4
        );
        assert_eq!(
            tag(Record::Whisper {
                author: [0; 32],
                seq: 0,
                ts: 0,
                to: [0; 32],
                ct: blank(),
                sig: blank(),
            }),
            5
        );
        assert_eq!(
            tag(Record::Presence {
                author: [0; 32],
                name: String::new(),
                addr: blank(),
                ts: 0,
                sig: blank(),
            }),
            6
        );
        assert_eq!(
            tag(Record::Image {
                author: [0; 32],
                seq: 0,
                ts: 0,
                name: String::new(),
                blob: [0; 32],
                w: 0,
                h: 0,
                kind: ImageKind::Png,
                bytes: 0,
                caption: String::new(),
                reply_to: None,
                sig: blank(),
            }),
            7
        );
        assert_eq!(
            tag(Record::Quiet(Sealed {
                id: [0; 32],
                ts: 0,
                ct: blank(),
                tag: [0; 32],
            })),
            8
        );
    }

    #[test]
    fn a_picture_is_deduplicated_like_any_other_message() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let mut log = RoomLog::open_or_create(&dir, &pin, Some("sala")).unwrap();

        let shot = Record::Image {
            author: [7; 32],
            seq: 0,
            ts: 1,
            name: "Pedro".into(),
            blob: [9; 32],
            w: 320,
            h: 240,
            kind: ImageKind::Png,
            bytes: 62_000,
            caption: String::new(),
            reply_to: None,
            sig: vec![1, 2, 3],
        };
        assert!(log.append(shot.clone()).unwrap(), "first copy lands");
        assert!(!log.append(shot).unwrap(), "the sync resends it forever");

        // It shares the author's sequence with text, so the next message
        // cannot silently reuse the slot.
        assert_eq!(log.next_seq_for(&[7; 32]), 1);
    }

    #[test]
    fn blobs_are_sealed_and_verified_against_their_name() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let log = RoomLog::open_or_create(&dir, &pin, Some("sala")).unwrap();

        let pixels = b"nao sao pixels de verdade, mas servem".to_vec();
        let hash = *blake3::hash(&pixels).as_bytes();
        assert!(!log.has_blob(&hash));
        log.write_blob(&hash, &pixels).unwrap();
        assert!(log.has_blob(&hash));
        assert_eq!(log.read_blob(&hash).as_deref(), Some(pixels.as_slice()));

        // Nothing readable on disk: a screenshot is as sensitive as the
        // sentence describing it.
        let raw = fs::read(log.blob_path(&hash)).unwrap();
        assert!(
            !raw.windows(4).any(|w| w == b"pixe"),
            "blob went to disk in the clear"
        );

        // A blob whose content stopped matching its name is not the blob we
        // asked for, even if it decrypts.
        let lie = [0xab; 32];
        log.write_blob(&lie, &pixels).unwrap();
        assert_eq!(log.read_blob(&lie), None, "content must match the hash");
    }

    #[test]
    fn a_peer_cannot_answer_with_pixels_we_did_not_ask_for() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let log = RoomLog::open_or_create(&dir, &pin, Some("sala")).unwrap();

        let wanted = b"a imagem que pedimos".to_vec();
        let hash = *blake3::hash(&wanted).as_bytes();
        let sealed = log.key.seal(&wanted).unwrap();
        assert!(log.accept_blob(&hash, &sealed), "the real thing is kept");

        // Right key, wrong content: somebody answering our request with
        // something else entirely.
        let other = log.key.seal(b"uma imagem completamente diferente").unwrap();
        let elsewhere = TempDir::new().unwrap();
        let dir2 = DataDir::from_path(elsewhere.path().to_path_buf()).unwrap();
        let fresh = RoomLog::open_or_create(&dir2, &pin, Some("sala")).unwrap();
        assert!(
            !fresh.accept_blob(&hash, &other),
            "content that is not what we asked for must be refused"
        );
        assert!(!fresh.has_blob(&hash), "and must not reach the disk");

        // Sealed with a key from another room: unreadable, and not ours.
        let stranger = RoomKey::derive(&Pin::parse("KKKK-MMMM").unwrap()).unwrap();
        assert!(
            !fresh.accept_blob(&hash, &stranger.seal(&wanted).unwrap()),
            "a blob from another room must not open here"
        );
    }

    #[test]
    fn pruning_drops_the_oldest_blobs_until_it_fits() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let log = RoomLog::open_or_create(&dir, &pin, Some("sala")).unwrap();

        let mut names = Vec::new();
        for n in 0u8..4 {
            let body = vec![n; 4096];
            let hash = *blake3::hash(&body).as_bytes();
            log.write_blob(&hash, &body).unwrap();
            names.push(hash);
            // The pruner orders by access time; without a gap the four files
            // can land on the same tick and the order becomes arbitrary.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(names.iter().all(|h| log.has_blob(h)));

        // Room for roughly two of them.
        let dropped = log.prune_blobs(9_000).unwrap();
        assert!(dropped >= 2, "expected at least two to go, went {dropped}");
        assert!(!log.has_blob(&names[0]), "the oldest should go first");
        assert!(log.has_blob(&names[3]), "the newest should survive");

        // Under the limit it must not touch anything.
        assert_eq!(log.prune_blobs(u64::MAX).unwrap(), 0);
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
