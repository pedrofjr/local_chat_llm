//! The room as two pipes, for programs instead of people.
//!
//! `local-llm bot --room <PIN>` joins a room, writes one JSON object per line
//! to stdout for everything it hears, and sends one message for every JSON
//! object it reads on stdin. No interface, no terminal setup, no keys to
//! handle.
//!
//! The point is what the caller does *not* have to know. The log is encrypted
//! with the room key and every record is signed by this machine's device key,
//! so anything writing to it from outside would need both, get the framing
//! exactly right, and keep the per-author sequence honest -- and a record that
//! fails any of that is refused by every peer. Here the app does all of it,
//! and a bot only ever handles text.
//!
//! The bot is its own participant either way -- its own device key, its own
//! name in the room -- because a second instance on a machine already gets its
//! own data directory. Where it runs decides how it is reached:
//!
//! * **Same machine as somebody's chat:** run it with no `LOCAL_LLM_HOME`. It
//!   becomes instance 2 and the two find each other through the presence file
//!   they share. mDNS cannot see two processes on one machine, so that file is
//!   the only way, and it lives under the *base* directory -- pointing the bot
//!   at a different `LOCAL_LLM_HOME` puts it somewhere the chat never looks,
//!   and the two sit in the same room without ever meeting.
//! * **Its own machine:** anything goes, including `LOCAL_LLM_HOME`. mDNS
//!   finds it the way it finds any colleague.

use crate::net::{NetEvent, NetSession, Presence};
use crate::room::OpenRoom;
use crate::crypto::Pin;
use crate::store::{DataDir, Record};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// One line of stdout. Serialised by hand: the project has no json library,
/// and pulling one in for six fields would cost more binary than the whole
/// mode is worth.
enum Out<'a> {
    /// The room is open and the network is up (or deliberately is not).
    Ready {
        room: &'a str,
        nick: &'a str,
        author: &'a str,
        online: bool,
    },
    Message {
        from: &'a str,
        author: &'a str,
        text: &'a str,
        ts: u64,
        mine: bool,
        /// Whether this names us. The bot's whole job usually starts here, so
        /// it is worked out once and handed over rather than left to string
        /// matching on the far side.
        mentioned: bool,
        /// Present when the message was private, naming the other end.
        whisper: Option<&'a str>,
    },
    /// One of ours went out. Without this a bot has no way to tell a message
    /// it sent from one that silently failed to compose.
    Sent {
        text: &'a str,
        delivered: bool,
    },
    Error {
        message: &'a str,
    },
}

/// Escapes a string into a JSON string literal, quotes included.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Control characters have to go as \u escapes or the line is not
            // valid JSON. Everything else, including accented text and emoji,
            // passes through as UTF-8.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Out<'_> {
    fn line(&self) -> String {
        match self {
            Out::Ready {
                room,
                nick,
                author,
                online,
            } => format!(
                "{{\"type\":\"ready\",\"room\":{},\"nick\":{},\"author\":{},\"online\":{online}}}",
                quote(room),
                quote(nick),
                quote(author)
            ),
            Out::Message {
                from,
                author,
                text,
                ts,
                mine,
                mentioned,
                whisper,
            } => {
                let whisper = match whisper {
                    Some(who) => format!(",\"whisper\":{}", quote(who)),
                    None => String::new(),
                };
                format!(
                    "{{\"type\":\"message\",\"from\":{},\"author\":{},\"text\":{},\
                     \"ts\":{ts},\"mine\":{mine},\"mentioned\":{mentioned}{whisper}}}",
                    quote(from),
                    quote(author),
                    quote(text)
                )
            }
            Out::Sent { text, delivered } => format!(
                "{{\"type\":\"sent\",\"text\":{},\"delivered\":{delivered}}}",
                quote(text)
            ),
            Out::Error { message } => {
                format!("{{\"type\":\"error\",\"message\":{}}}", quote(message))
            }
        }
    }
}

/// Pulls the `text` field out of an input line.
///
/// Deliberately strict about the shape and forgiving about nothing: a bot that
/// sends a malformed line should hear about it, not have the app guess. The
/// parser is small because the accepted shape is small -- `{"text": "..."}`
/// and nothing else is required.
fn text_of(line: &str) -> Result<String> {
    let line = line.trim();
    if !line.starts_with('{') {
        bail!("expected a json object, for example {{\"text\":\"hello\"}}");
    }
    let key = "\"text\"";
    let Some(at) = line.find(key) else {
        bail!("no \"text\" field");
    };
    let rest = line[at + key.len()..].trim_start();
    let Some(rest) = rest.strip_prefix(':') else {
        bail!("\"text\" is not followed by a value");
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('"') else {
        bail!("\"text\" must be a string");
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Ok(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let point =
                        u32::from_str_radix(&hex, 16).context("bad \\u escape in \"text\"")?;
                    out.push(char::from_u32(point).unwrap_or('\u{fffd}'));
                }
                Some(other) => out.push(other),
                None => bail!("string ends in a backslash"),
            },
            c => out.push(c),
        }
    }
    bail!("unterminated string in \"text\"")
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    // Flushed every line: a bot on the other end of a pipe is waiting on this,
    // and a buffered line is a bot that appears to have hung.
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Runs the room headless until stdin closes.
pub async fn run(pin: &str, nick: Option<&str>) -> Result<()> {
    let dir = DataDir::open().context("open the data directory")?;
    if let Some(nick) = nick {
        dir.save_nick(nick).context("save the nick")?;
    }
    let pin = Pin::parse(pin)?;
    let room = OpenRoom::join(&dir, pin, None).context("open the room")?;

    let secret = room.secret.clone();
    let author = hex32(&room.author);
    let nick = room.nick.clone();
    let alias = room.alias();
    let shared = Arc::new(Mutex::new(room));

    let (tx, mut events) = mpsc::unbounded_channel();
    let announced = shared.lock().await.announce_identity().ok().flatten();
    let net = if std::env::var_os("LOCAL_LLM_OFFLINE").is_some() {
        None
    } else {
        let presence = Presence {
            dir: dir.presence_dir(),
            instance: dir.instance,
        };
        match NetSession::start(secret, shared.clone(), tx, Vec::new(), presence).await {
            Ok(net) => {
                if let Some(rec) = &announced {
                    let _ = net.broadcast(rec).await;
                }
                Some(net)
            }
            Err(e) => {
                say(&Out::Error {
                    message: &format!("offline: {e}"),
                }
                .line());
                None
            }
        }
    };

    say(&Out::Ready {
        room: &alias,
        nick: &nick,
        author: &author,
        online: net.is_some(),
    }
    .line());

    // Everything already in the log is history the bot did not see happen.
    // Skipping it stops a restart from replaying months of conversation as if
    // it had just arrived -- and from answering all of it.
    let mut consumed = shared.lock().await.log.records().len();
    report(&shared, &mut consumed, &nick).await;

    // Messages sent while the room had no gossip neighbours, and whether it
    // has any now.
    let mut pending: Vec<Record> = Vec::new();
    let mut has_peers = false;

    // stdin is blocking, so it gets a thread and a channel like the TUI's
    // keyboard does.
    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
    std::thread::Builder::new()
        .name("bot-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        })
        .context("start the stdin reader")?;

    loop {
        tokio::select! {
            line = lines.recv() => {
                // stdin closed: the bot is done, and so are we.
                let Some(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match text_of(&line) {
                    Ok(text) if text.trim().is_empty() => {}
                    Ok(text) => {
                        let composed = { shared.lock().await.compose(text, None) };
                        match composed {
                            Ok(rec) => {
                                let text = rec.body().unwrap_or_default().to_string();
                                let mut delivered = false;
                                if let Some(net) = &net {
                                    match net.broadcast(&rec).await {
                                        Ok(()) => delivered = true,
                                        Err(e) => say(&Out::Error {
                                            message: &format!("not delivered: {e}"),
                                        }
                                        .line()),
                                    }
                                }
                                // Marks our own message as seen rather than
                                // echoing it back as if somebody said it.
                                consumed = shared.lock().await.log.records().len();
                                // Said after the send, so `delivered` means
                                // what it says. Saved either way: offline, the
                                // message is in the log and syncs later.
                                // Gossip delivers to the neighbours we have
                                // *now*. Seconds after starting there are
                                // none, so this went nowhere -- and the sync
                                // loop does not save it, because that is a
                                // pull: peers ask us for what they are
                                // missing, and one that asks after we exit
                                // finds nobody home. Held so it can go again
                                // once somebody is actually there.
                                if !has_peers {
                                    pending.push(rec);
                                }
                                say(&Out::Sent { text: &text, delivered }.line());
                            }
                            Err(e) => say(&Out::Error { message: &format!("could not send: {e}") }.line()),
                        }
                    }
                    Err(e) => say(&Out::Error { message: &e.to_string() }.line()),
                }
            }
            event = events.recv() => {
                match event {
                    Some(NetEvent::Record) => report(&shared, &mut consumed, &nick).await,
                    Some(NetEvent::Peers(peers)) => {
                        has_peers = !peers.is_empty();
                        if has_peers {
                            flush_pending(&net, &mut pending).await;
                        }
                    }
                    // The rest is presence, diagnostics and status meant for a
                    // person watching a screen.
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    // `echo '{"text":"deploy ok"}' | local-llm bot ...` is the obvious way to
    // fire off one notification, and it was the one that did not work. A
    // message goes out over gossip, to the neighbours that exist at that
    // moment -- and a process that started a second ago has none. It left
    // having reported the message as delivered, and nobody ever got it.
    //
    // So on the way out, wait for somebody to show up and send it again. The
    // long-running case never gets here: neighbours are long since found.
    if net.is_some() && !pending.is_empty() {
        let deadline = tokio::time::Instant::now() + HANDOVER_WAIT;
        while !pending.is_empty() && tokio::time::Instant::now() < deadline {
            let left = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(left, events.recv()).await {
                Ok(Some(NetEvent::Peers(peers))) if !peers.is_empty() => {
                    flush_pending(&net, &mut pending).await;
                    // A moment for the send to actually leave the socket.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(Some(_)) => {}
                // Channel closed or the clock ran out.
                Ok(None) | Err(_) => break,
            }
        }
        if !pending.is_empty() {
            say(&Out::Error {
                message: "nobody was reachable before exiting; the message is                           saved here and syncs when this room is open again",
            }
            .line());
        }
    }
    Ok(())
}

/// Sends again everything that went out to an empty room.
async fn flush_pending(net: &Option<NetSession>, pending: &mut Vec<Record>) {
    let Some(net) = net else { return };
    for rec in pending.drain(..) {
        if let Err(e) = net.broadcast(&rec).await {
            say(&Out::Error {
                message: &format!("not delivered: {e}"),
            }
            .line());
        }
    }
}

/// How long to wait, on the way out, for somebody to appear to send to.
const HANDOVER_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

/// Emits every record that arrived since the last look.
async fn report(shared: &Arc<Mutex<OpenRoom>>, consumed: &mut usize, nick: &str) {
    let room = shared.lock().await;
    let records = room.log.records();
    if *consumed > records.len() {
        *consumed = 0;
    }
    for rec in records.iter().skip(*consumed) {
        let line = match rec {
            Record::Chat { author, ts, .. }
            | Record::ChatNamed { author, ts, .. }
            | Record::Post { author, ts, .. } => {
                let body = rec.body().unwrap_or_default().to_string();
                Some(
                    Out::Message {
                        from: &room.label_of(rec),
                        author: &hex32(author),
                        text: &body,
                        ts: *ts,
                        mine: room.is_mine(rec),
                        mentioned: crate::tui::mentions(&body, nick),
                        whisper: None,
                    }
                    .line(),
                )
            }
            Record::Image {
                author, ts, caption, ..
            } => Some(
                // A picture is announced by its caption, which is what a bot
                // can act on. The pixels are not its business.
                Out::Message {
                    from: &room.label_of(rec),
                    author: &hex32(author),
                    text: caption,
                    ts: *ts,
                    mine: room.is_mine(rec),
                    mentioned: crate::tui::mentions(caption, nick),
                    whisper: None,
                }
                .line(),
            ),
            Record::Quiet(_) | Record::Whisper { .. } => {
                room.open_whisper(rec).map(|opened| {
                    let them = room
                        .log
                        .records()
                        .iter()
                        .find_map(|r| match r {
                            Record::ChatNamed { author, name, .. } if *author == opened.them => {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| hex32(&opened.them));
                    Out::Message {
                        from: &opened.name,
                        author: &hex32(&opened.from),
                        text: &opened.body,
                        ts: crate::store::now_ts(),
                        mine: opened.mine,
                        // A whisper is addressed to us by construction.
                        mentioned: !opened.mine,
                        whisper: Some(&them),
                    }
                    .line()
                })
            }
            Record::Meta { .. } | Record::Identity { .. } | Record::Presence { .. } => None,
        };
        if let Some(line) = line {
            say(&line);
        }
    }
    *consumed = records.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_line_is_valid_json_with_the_awkward_characters_escaped() {
        let line = Out::Message {
            from: "Grok 4.5",
            author: "ab12",
            text: "olha isso: \"aspas\", uma \\ barra\ne uma quebra",
            ts: 1_700_000_000,
            mine: false,
            mentioned: true,
            whisper: None,
        }
        .line();

        assert!(line.starts_with('{') && line.ends_with('}'));
        // The escapes have to be in the output, not the raw characters: a raw
        // newline would split one message into two lines and desynchronise
        // whatever is parsing them.
        assert!(line.contains("\\\"aspas\\\""), "{line}");
        assert!(line.contains("\\\\ barra"), "{line}");
        assert!(line.contains("\\n"), "{line}");
        assert!(!line.contains('\n'), "a line must stay one line: {line}");
        assert!(line.contains("\"mentioned\":true"));
        assert!(!line.contains("whisper"), "absent when it is not private");
    }

    #[test]
    fn a_whisper_line_names_the_other_end() {
        let line = Out::Message {
            from: "Dale",
            author: "ab12",
            text: "so entre nos",
            ts: 1,
            mine: false,
            mentioned: true,
            whisper: Some("Grok 4.5"),
        }
        .line();
        assert!(line.contains("\"whisper\":\"Grok 4.5\""), "{line}");
    }

    #[test]
    fn control_characters_do_not_break_the_line() {
        let line = Out::Error {
            message: "tab\there, bell\u{7}there",
        }
        .line();
        assert!(line.contains("\\t"), "{line}");
        assert!(line.contains("\\u0007"), "{line}");
        assert!(!line.contains('\u{7}'));
    }

    #[test]
    fn reads_the_text_field_back_out() {
        assert_eq!(text_of(r#"{"text":"bom dia"}"#).unwrap(), "bom dia");
        // Escapes survive the round trip, which is what lets a bot send a
        // message containing quotes.
        assert_eq!(
            text_of(r#"{"text":"ele disse \"oi\" e saiu"}"#).unwrap(),
            "ele disse \"oi\" e saiu"
        );
        assert_eq!(text_of(r#"{"text":"linha\nquebrada"}"#).unwrap(), "linha\nquebrada");
        assert_eq!(text_of(r#"{ "text" : "com espacos" }"#).unwrap(), "com espacos");
        // Other fields are allowed and ignored, so a bot can carry its own
        // bookkeeping in the same object.
        assert_eq!(
            text_of(r#"{"id":7,"text":"depois de outro campo"}"#).unwrap(),
            "depois de outro campo"
        );
    }

    #[test]
    fn a_malformed_line_is_refused_rather_than_guessed() {
        // Plain text is the mistake somebody will actually make, and sending
        // it as a message would be a silent surprise in the room.
        assert!(text_of("bom dia").is_err());
        assert!(text_of(r#"{"txt":"errado"}"#).is_err());
        assert!(text_of(r#"{"text":123}"#).is_err());
        assert!(text_of(r#"{"text":"sem fechar}"#).is_err());
    }
}
