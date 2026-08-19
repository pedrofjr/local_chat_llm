use crate::crypto::{role_for, whisper_key, whisper_public, whisper_secret, Pin, RoomKey};
use crate::store::{now_ts, DataDir, ImageKind, Record, RoomLog};
use anyhow::{anyhow, Result};
use iroh::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

/// What actually travels inside a whisper. The name goes in the ciphertext so
/// bystanders learn who talked to whom, but not under which name.
#[derive(Serialize, Deserialize)]
struct WhisperBody {
    name: String,
    body: String,
}

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

    pub fn compose(&mut self, body: String, reply_to: Option<([u8; 32], u64)>) -> Result<Record> {
        let seq = self.log.next_seq_for(&self.author);
        let ts = now_ts();
        let name = self.nick.clone();
        let sig = self
            .secret
            .sign(&post_payload(&self.author, seq, ts, &name, &body, reply_to));
        let rec = Record::Post {
            author: self.author,
            seq,
            ts,
            name,
            body,
            reply_to,
            sig: sig.to_bytes().to_vec(),
        };
        self.log.append(rec.clone())?;
        Ok(rec)
    }

    /// Files the pixels away as a blob and puts only their description in the
    /// log. The caller has already sized and re-encoded them; this decides
    /// nothing about the picture, it just names and signs it.
    pub fn compose_image(
        &mut self,
        pixels: &[u8],
        w: u32,
        h: u32,
        kind: ImageKind,
        caption: String,
        reply_to: Option<([u8; 32], u64)>,
    ) -> Result<Record> {
        let blob = *blake3::hash(pixels).as_bytes();
        // Written before the record exists, so the log can never point at a
        // blob this machine does not have.
        self.log.write_blob(&blob, pixels)?;
        let seq = self.log.next_seq_for(&self.author);
        let ts = now_ts();
        let name = self.nick.clone();
        let bytes = u32::try_from(pixels.len()).map_err(|_| anyhow!("picture too large"))?;
        let sig = self.secret.sign(&image_payload(
            &self.author,
            seq,
            ts,
            &name,
            &blob,
            w,
            h,
            kind,
            bytes,
            &caption,
            reply_to,
        ));
        let rec = Record::Image {
            author: self.author,
            seq,
            ts,
            name,
            blob,
            w,
            h,
            kind,
            bytes,
            caption,
            reply_to,
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
            Record::Post {
                author,
                seq,
                ts,
                name,
                body,
                reply_to,
                sig,
            } => verify_sig(
                author,
                sig,
                &post_payload(author, *seq, *ts, name, body, *reply_to),
            )?,
            // Checked even though we may never read it: without this anyone
            // could publish a key in someone else's name and read their mail.
            Record::Identity { author, x_pub, sig } => {
                verify_sig(author, sig, &identity_payload(author, x_pub))?
            }
            // The signature covers the ciphertext, so a peer who is not the
            // recipient still validates and stores the record.
            Record::Whisper {
                author,
                seq,
                ts,
                to,
                ct,
                sig,
            } => verify_sig(author, sig, &whisper_payload(author, *seq, *ts, to, ct))?,
            // Verified like anything else, but deliberately not stored: it
            // says where someone is now, which is worthless a minute later.
            Record::Presence {
                author,
                name,
                addr,
                ts,
                sig,
            } => {
                verify_sig(author, sig, &presence_payload(author, name, addr, *ts))?;
                return Ok(false);
            }
            Record::Image {
                author,
                seq,
                ts,
                name,
                blob,
                w,
                h,
                kind,
                bytes,
                caption,
                reply_to,
                sig,
            } => verify_sig(
                author,
                sig,
                &image_payload(
                    author, *seq, *ts, name, blob, *w, *h, *kind, *bytes, caption, *reply_to,
                ),
            )?,
            Record::Meta { .. } => {}
        }
        self.log.append(rec)
    }

    /// The X25519 key other people use to whisper to us.
    pub fn whisper_public(&self) -> [u8; 32] {
        whisper_public(&whisper_secret(&self.secret.to_bytes()))
    }

    /// Publishes our whisper key, signed. Returns the record only the first
    /// time, since the log keeps one identity per person.
    pub fn announce_identity(&mut self) -> Result<Option<Record>> {
        let x_pub = self.whisper_public();
        let sig = self.secret.sign(&identity_payload(&self.author, &x_pub));
        let rec = Record::Identity {
            author: self.author,
            x_pub,
            sig: sig.to_bytes().to_vec(),
        };
        Ok(self.log.append(rec.clone())?.then_some(rec))
    }

    /// Whisper key someone published, if it has reached us yet.
    pub fn identity_of(&self, author: &[u8; 32]) -> Option<[u8; 32]> {
        self.log.records().iter().find_map(|rec| match rec {
            Record::Identity {
                author: held,
                x_pub,
                ..
            } if held == author => Some(*x_pub),
            _ => None,
        })
    }

    fn pair_key(&self, them: &[u8; 32]) -> Result<RoomKey> {
        let their_pub = self
            .identity_of(them)
            .ok_or_else(|| anyhow!("they have not published a key here yet"))?;
        let secret = whisper_secret(&self.secret.to_bytes());
        Ok(whisper_key(&secret, &their_pub, &self.author, them))
    }

    pub fn compose_whisper(&mut self, to: [u8; 32], body: String) -> Result<Record> {
        let sealed = self.pair_key(&to)?.seal(&postcard::to_stdvec(&WhisperBody {
            name: self.nick.clone(),
            body,
        })?)?;
        let seq = self.log.next_seq_for(&self.author);
        let ts = now_ts();
        let sig = self
            .secret
            .sign(&whisper_payload(&self.author, seq, ts, &to, &sealed));
        let rec = Record::Whisper {
            author: self.author,
            seq,
            ts,
            to,
            ct: sealed,
            sig: sig.to_bytes().to_vec(),
        };
        self.log.append(rec.clone())?;
        Ok(rec)
    }

    /// Opens a whisper we are a party to. Returns the sender's name, the text,
    /// and the person on the other end. Anything else answers None, which is
    /// exactly what a bystander gets.
    pub fn open_whisper(&self, rec: &Record) -> Option<(String, String, [u8; 32])> {
        let Record::Whisper {
            author, to, ct, ..
        } = rec
        else {
            return None;
        };
        let mine = *author == self.author;
        let them = if mine {
            *to
        } else if *to == self.author {
            *author
        } else {
            return None;
        };
        let plain = self.pair_key(&them).ok()?.open(ct).ok()?;
        let opened: WhisperBody = postcard::from_bytes(&plain).ok()?;
        Some((opened.name, opened.body, them))
    }

    /// Says we are here, under the name we are using, and where to find us.
    pub fn compose_presence(&self, addr: Vec<u8>) -> Record {
        let ts = now_ts();
        let sig = self
            .secret
            .sign(&presence_payload(&self.author, &self.nick, &addr, ts));
        Record::Presence {
            author: self.author,
            name: self.nick.clone(),
            addr,
            ts,
            sig: sig.to_bytes().to_vec(),
        }
    }

    pub fn label_of(&self, rec: &Record) -> String {
        match rec {
            Record::ChatNamed { name, .. }
            | Record::Post { name, .. }
            | Record::Image { name, .. } => name.clone(),
            Record::Chat { author, .. } | Record::Whisper { author, .. } => {
                if *author == self.author {
                    self.nick.clone()
                } else {
                    role_for(author).to_string()
                }
            }
            Record::Presence { name, .. } => name.clone(),
            Record::Meta { .. } | Record::Identity { .. } => "system".into(),
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

/// Length-prefixed framing under a domain tag. The legacy `sign_payload` below
/// just concatenated its fields, so ("ab", "c") and ("abc", "") signed the very
/// same bytes. Everything added from v2 on goes through this instead.
fn signed_bytes(domain: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(domain.len() as u64).to_le_bytes());
    out.extend_from_slice(domain.as_bytes());
    for part in parts {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn post_payload(
    author: &[u8; 32],
    seq: u64,
    ts: u64,
    name: &str,
    body: &str,
    reply_to: Option<([u8; 32], u64)>,
) -> Vec<u8> {
    let mut answered = Vec::new();
    if let Some((target, target_seq)) = reply_to {
        answered.extend_from_slice(&target);
        answered.extend_from_slice(&target_seq.to_le_bytes());
    }
    signed_bytes(
        "local-llm/post/v1",
        &[
            author,
            &seq.to_le_bytes(),
            &ts.to_le_bytes(),
            name.as_bytes(),
            body.as_bytes(),
            &answered,
        ],
    )
}

pub fn presence_payload(author: &[u8; 32], name: &str, addr: &[u8], ts: u64) -> Vec<u8> {
    signed_bytes(
        "local-llm/presence/v1",
        &[author, name.as_bytes(), addr, &ts.to_le_bytes()],
    )
}

pub fn identity_payload(author: &[u8; 32], x_pub: &[u8; 32]) -> Vec<u8> {
    signed_bytes("local-llm/identity/v1", &[author, x_pub])
}

pub fn whisper_payload(author: &[u8; 32], seq: u64, ts: u64, to: &[u8; 32], ct: &[u8]) -> Vec<u8> {
    signed_bytes(
        "local-llm/whisper/v1",
        &[author, &seq.to_le_bytes(), &ts.to_le_bytes(), to, ct],
    )
}

/// Covers the blob *hash* rather than the pixels, which are not here. Since
/// `read_blob` refuses any blob whose content stops matching its name, signing
/// the hash authenticates the picture itself: swapping the bytes on disk or in
/// flight makes them stop opening.
#[allow(clippy::too_many_arguments)]
pub fn image_payload(
    author: &[u8; 32],
    seq: u64,
    ts: u64,
    name: &str,
    blob: &[u8; 32],
    w: u32,
    h: u32,
    kind: ImageKind,
    bytes: u32,
    caption: &str,
    reply_to: Option<([u8; 32], u64)>,
) -> Vec<u8> {
    let mut answered = Vec::new();
    if let Some((target, target_seq)) = reply_to {
        answered.extend_from_slice(&target);
        answered.extend_from_slice(&target_seq.to_le_bytes());
    }
    signed_bytes(
        "local-llm/image/v1",
        &[
            author,
            &seq.to_le_bytes(),
            &ts.to_le_bytes(),
            name.as_bytes(),
            blob,
            &w.to_le_bytes(),
            &h.to_le_bytes(),
            &[kind as u8],
            &bytes.to_le_bytes(),
            caption.as_bytes(),
            &answered,
        ],
    )
}

/// Legacy framing, kept only to verify records written before v2.
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
    use super::*;
    use crate::crypto::Pin;
    use crate::store::DataDir;
    use tempfile::TempDir;

    /// Three people in the same room, each on their own machine.
    fn open_at(tmp: &TempDir, pin: &Pin) -> OpenRoom {
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        OpenRoom::join(&dir, pin.clone(), Some("sala")).unwrap()
    }

    #[test]
    fn a_picture_cannot_be_swapped_for_another() {
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let (mut a, mut b) = (open_at(&ta, &pin), open_at(&tb, &pin));

        let pixels = b"conteudo original da imagem".to_vec();
        let rec = a
            .compose_image(&pixels, 320, 240, ImageKind::Png, "o erro".into(), None)
            .unwrap();

        // The sender files the pixels away before announcing them, so the log
        // never points at a blob this machine does not hold.
        let Record::Image { blob, .. } = &rec else {
            panic!("expected a picture");
        };
        assert_eq!(a.log.read_blob(blob).as_deref(), Some(pixels.as_slice()));

        assert!(b.ingest(rec.clone()).unwrap(), "a good picture is accepted");

        // Repointing the record at different pixels breaks the signature,
        // because the hash is what was signed.
        let mut forged = rec.clone();
        if let Record::Image { blob, .. } = &mut forged {
            *blob = [0xcd; 32];
        }
        assert!(
            b.ingest(forged).is_err(),
            "a picture repointed at other bytes must not verify"
        );

        // Same for the caption, which is what people actually read.
        let mut relabelled = rec;
        if let Record::Image { caption, .. } = &mut relabelled {
            *caption = "outra legenda".into();
        }
        assert!(
            b.ingest(relabelled).is_err(),
            "the caption is covered by the signature"
        );
    }

    #[test]
    fn a_whisper_opens_only_for_the_two_ends() {
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let (ta, tb, tc) = (
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        );
        let (mut a, mut b, mut c) = (open_at(&ta, &pin), open_at(&tb, &pin), open_at(&tc, &pin));

        // Everyone publishes their whisper key and it reaches the others.
        let ia = a.announce_identity().unwrap().unwrap();
        let ib = b.announce_identity().unwrap().unwrap();
        for peer in [&mut b, &mut c] {
            peer.ingest(ia.clone()).unwrap();
        }
        for peer in [&mut a, &mut c] {
            peer.ingest(ib.clone()).unwrap();
        }

        let sealed = a.compose_whisper(b.author, "o chefe vem quinta".into()).unwrap();
        assert!(
            !format!("{sealed:?}").contains("chefe"),
            "the plaintext must not survive anywhere in the record"
        );

        b.ingest(sealed.clone()).unwrap();
        c.ingest(sealed.clone()).unwrap();

        let (name, text, other) = b.open_whisper(&sealed).expect("the recipient reads it");
        assert_eq!(text, "o chefe vem quinta");
        assert_eq!(other, a.author);
        assert_eq!(name, a.nick);

        // A third person holds the same room key and still gets nothing.
        assert!(
            c.open_whisper(&sealed).is_none(),
            "the room key must not open a whisper"
        );

        // And the sender can reread what they sent.
        let (_, mine, _) = a.open_whisper(&sealed).expect("sender rereads it");
        assert_eq!(mine, "o chefe vem quinta");
    }

    #[test]
    fn a_heartbeat_is_checked_but_never_stored() {
        let pin = Pin::parse("HHHH-JJJJ").unwrap();
        let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let (a, mut b) = (open_at(&ta, &pin), open_at(&tb, &pin));

        let before = b.log.records().len();
        let beat = a.compose_presence(b"endereco".to_vec());
        assert!(
            !b.ingest(beat.clone()).unwrap(),
            "presence is a live fact, it must not enter the history"
        );
        assert_eq!(b.log.records().len(), before, "and must not grow the log");

        // Even repeated, it never accumulates.
        for _ in 0..20 {
            b.ingest(beat.clone()).unwrap();
        }
        assert_eq!(b.log.records().len(), before);

        // Somebody else cannot beat in your name.
        let Record::Presence { name, addr, ts, sig, .. } = beat else {
            panic!("expected presence");
        };
        let forged = Record::Presence {
            author: b.author,
            name,
            addr,
            ts,
            sig,
        };
        assert!(b.ingest(forged).is_err());
    }

    #[test]
    fn a_whisper_key_cannot_be_published_in_someone_elses_name() {
        let pin = Pin::parse("AAAA-BBBB").unwrap();
        let (ta, tc) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let (a, mut c) = (open_at(&ta, &pin), open_at(&tc, &pin));

        // The impostor signs their own key but claims to be someone else.
        let x_pub = c.whisper_public();
        let sig = c.secret.sign(&identity_payload(&a.author, &x_pub));
        let forged = Record::Identity {
            author: a.author,
            x_pub,
            sig: sig.to_bytes().to_vec(),
        };
        assert!(
            c.ingest(forged).is_err(),
            "a key signed by the wrong person must be rejected"
        );
    }

    #[test]
    fn whispering_needs_their_key_to_have_arrived() {
        let pin = Pin::parse("CCCC-DDDD").unwrap();
        let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let (mut a, b) = (open_at(&ta, &pin), open_at(&tb, &pin));
        // b never announced as far as a is concerned.
        assert!(a.compose_whisper(b.author, "oi".into()).is_err());
    }

    #[test]
    fn a_reply_survives_the_signature_check() {
        let pin = Pin::parse("EEEE-FFFF").unwrap();
        let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let (mut a, mut b) = (open_at(&ta, &pin), open_at(&tb, &pin));

        let first = a.compose("pergunta".to_string(), None).unwrap();
        b.ingest(first.clone()).unwrap();
        let key = first.chat_key().unwrap();

        let answer = b.compose("resposta".into(), Some(key)).unwrap();
        assert_eq!(answer.reply_to(), Some(key));
        // The pointer is covered by the signature, so tampering is caught.
        a.ingest(answer.clone()).unwrap();

        let Record::Post { author, seq, ts, name, body, sig, .. } = answer else {
            panic!("expected a post");
        };
        let tampered = Record::Post {
            author,
            seq,
            ts,
            name,
            body,
            reply_to: Some(([0u8; 32], 0)),
            sig,
        };
        assert!(
            a.ingest(tampered).is_err(),
            "moving the reply pointer must break the signature"
        );
    }

    use super::normalize_nick;

    #[test]
    fn framing_keeps_fields_apart() {
        use super::signed_bytes;
        // The old concatenation made these two identical.
        assert_ne!(
            signed_bytes("d", &[b"ab", b"c"]),
            signed_bytes("d", &[b"abc", b""])
        );
        assert_ne!(signed_bytes("a", &[b"x"]), signed_bytes("b", &[b"x"]));
        assert_eq!(signed_bytes("d", &[b"ab"]), signed_bytes("d", &[b"ab"]));
    }

    #[test]
    fn nick_rules() {
        assert_eq!(normalize_nick("  Diamante  ").unwrap(), "Diamante");
        assert!(normalize_nick("").is_err());
        assert!(normalize_nick("no/slash").is_err());
        assert!(normalize_nick(&"x".repeat(25)).is_err());
    }
}
