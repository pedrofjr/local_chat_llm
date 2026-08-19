use crate::crypto::{color_for, role_for, topic_id, Pin, topic_hex};
use crate::net::{parse_ticket, NetEvent, NetSession, Presence};
use crate::room::OpenRoom;
use crate::store::{now_ts, DataDir, ImageKind, ImageProto, Notify, Record, Settings};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use iroh::{EndpointAddr, EndpointId};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, Mutex};

type Term = Terminal<CrosstermBackend<Stdout>>;

const HOME_HINT: &str = "/new <name>   /join <key>   /nick <name>   /help   /quit";
const CHAT_HINT: &str = "/help for commands   f12 hides names   esc clears the line";
/// What the status bar says while the disguise is on. Never mentions peers.
const MASKED_STATUS: &str = "loaded · q4_k_m · 8192 ctx · 24 layers on gpu";
/// A peer that has not said anything for this long is treated as gone. Three
/// missed heartbeats, so a hiccup does not make people blink in and out.
const PRESENCE_TTL: Duration = Duration::from_secs(20);
/// Two keystrokes closer together than this came from the terminal, not from
/// fingers: the fastest typists in the world leave some 60 ms between keys.
const PASTE_GAP: Duration = Duration::from_millis(5);
/// How many back-to-back keystrokes it takes before we believe it is a paste.
/// One close pair proves nothing; a run of them cannot be typed.
const BURST_RUN: usize = 3;
/// Most recent transcript entries laid out per frame. Older ones stay in the
/// log and come back when the room is reopened.
const RENDER_CAP: usize = 800;

#[derive(Clone)]
enum Screen {
    Home,
    Unlock { alias: String, topic: [u8; 32] },
    Chat,
    /// Overlays. They own the screen so nothing can scroll them away — the
    /// earlier version wrote help into the transcript, where incoming traffic
    /// pushed it out of view within seconds.
    Help,
    Confirm { alias: String, topic: [u8; 32] },
    /// A new build was found and is waiting for a yes. Holds the verified
    /// manifest so the answer does not have to go looking again.
    Upgrade {
        version: String,
        manifest: Box<crate::update::Manifest>,
        required: bool,
    },
}

/// One rendered entry. Notices are local-only (never written to the log) and
/// live in the transcript instead of the status bar, so a printed key cannot
/// be wiped out two seconds later by a network event.
enum Feed {
    Notice {
        body: String,
    },
    System {
        body: String,
    },
    Msg {
        author: [u8; 32],
        seq: u64,
        name: String,
        mine: bool,
        body: String,
        ts: u64,
        reply_to: Option<([u8; 32], u64)>,
        /// Set when this is a whisper; holds the other side of it.
        whisper: Option<[u8; 32]>,
        /// Set when this message is a picture. Deliberately a field on `Msg`
        /// rather than a variant of its own: reply, copy, hide, hover, the
        /// per-person colour and the left/right alignment all keep working
        /// without a second copy of that code. Boxed because a picture is
        /// the rare case, and inline it made every plain notice in the feed
        /// carry its weight.
        image: Option<Box<ImageRef>>,
    },
}

/// Enough to draw the collapsed `image (+)` line without holding any pixels.
/// The bytes live in a blob that may not even have arrived yet.
#[derive(Clone)]
struct ImageRef {
    blob: [u8; 32],
    w: u32,
    h: u32,
    kind: ImageKind,
    bytes: u32,
}

/// Someone heard from recently, and when.
struct Live {
    name: String,
    at: Instant,
}

struct SessionRow {
    alias: String,
    topic: [u8; 32],
    remembered: bool,
}

#[derive(Default)]
struct Input {
    text: String,
    cursor: usize,
    history: Vec<String>,
    browsing: Option<usize>,
    stash: String,
}

impl Input {
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(idx)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn char_before(&self, idx: usize) -> Option<char> {
        idx.checked_sub(1).and_then(|i| self.text.chars().nth(i))
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_at(self.cursor - 1);
        self.text.remove(at);
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    fn kill_word(&mut self) {
        while self.char_before(self.cursor).is_some_and(char::is_whitespace) {
            self.backspace();
        }
        while self
            .char_before(self.cursor)
            .is_some_and(|c| !c.is_whitespace())
        {
            self.backspace();
        }
    }

    fn kill_to_start(&mut self) {
        let at = self.byte_at(self.cursor);
        self.text.replace_range(..at, "");
        self.cursor = 0;
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    fn take(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        if !out.trim().is_empty() && self.history.last() != Some(&out) {
            self.history.push(out.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        out
    }

    fn recall_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.browsing {
            None => {
                self.stash = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(idx);
        self.text = self.history[idx].clone();
        self.cursor = self.len();
    }

    fn recall_next(&mut self) {
        let Some(idx) = self.browsing else { return };
        if idx + 1 >= self.history.len() {
            self.browsing = None;
            self.text = std::mem::take(&mut self.stash);
        } else {
            self.browsing = Some(idx + 1);
            self.text = self.history[idx + 1].clone();
        }
        self.cursor = self.len();
    }
}

pub struct App {
    dir: DataDir,
    screen: Screen,
    sessions: Vec<SessionRow>,
    selected: usize,
    input: Input,
    status: String,
    room: Option<Arc<Mutex<OpenRoom>>>,
    net: Option<NetSession>,
    events_rx: Option<mpsc::UnboundedReceiver<NetEvent>>,
    /// Direct gossip neighbours. Topology, shown only by /diag.
    peers: Vec<EndpointId>,
    /// Who is actually in the room, by their own account.
    present: HashMap<[u8; 32], Live>,
    names: HashMap<[u8; 32], String>,
    /// Where each message sits in `feed`, so a reply can find what it answers.
    by_key: HashMap<([u8; 32], u64), usize>,
    ticket: Option<String>,
    feed: Vec<Feed>,
    /// How many log records are already mirrored into `feed`. The transcript
    /// is built incrementally so drawing never has to touch the room lock.
    consumed: usize,
    me: [u8; 32],
    alias: String,
    nick: String,
    scroll: u16,
    max_scroll: u16,
    follow: bool,
    unread: usize,
    masked: bool,
    mask_stash: String,
    offset: UtcOffset,
    last_bell: Option<Instant>,
    settings: Settings,
    /// Laid-out transcript, rebuilt only when something that affects it moves.
    rendered: Option<Rendered>,
    /// Where the transcript was painted last frame, so a mouse row can be
    /// turned back into a message.
    chat_area: Rect,
    /// Message under the pointer, and the one picked with the keyboard.
    hover: Option<usize>,
    picked: Option<usize>,
    /// Message the next thing you send will answer.
    replying: Option<([u8; 32], u64)>,
    /// Messages blurred on this screen. Local preference: nothing about it
    /// goes into the log or out to the peers.
    hidden: HashSet<([u8; 32], u64)>,
    /// Bumped on every change, so the layout cache notices. A plain count
    /// would miss hide-then-show.
    hidden_rev: u64,
    /// Set by a click on the hide icon and consumed by the event loop, which
    /// can await the write that the mouse handler cannot.
    pending_hide: Option<usize>,
    /// Pictures opened on this screen. Deliberately **not** persisted: every
    /// session starts with everything closed, so a screenshot never paints
    /// itself onto the terminal just because the room was reopened.
    expanded: HashSet<([u8; 32], u64)>,
    /// Bumped on every open or close, so the layout cache notices.
    expanded_rev: u64,
    /// Pixels decoded and encoded for the terminal, keyed by blob. Built when
    /// a picture is opened and dropped when it is closed.
    shots: HashMap<[u8; 32], Shot>,
    /// How this terminal draws. Settled once at startup, never re-queried.
    proto: ImageProto,
    /// Set by a click on the picture line, consumed by the event loop.
    pending_expand: Option<usize>,
    /// Who the next message goes to privately. Set by `/w` and held until
    /// Esc, because the alternative -- retyping `/w` every line -- is how a
    /// private sentence ends up in the room.
    whispering: Option<[u8; 32]>,
    /// Peers that refused a history sync last round, and how many were tried.
    /// A refusal means a different build: gossip keeps delivering live
    /// messages, so nothing looks wrong until somebody notices the history
    /// never arrives.
    sync_reach: (usize, usize),
    /// Said once per session, not once every three seconds.
    warned_mismatch: bool,
    /// Blobs actually painted last frame. An animation nobody can see must
    /// not keep waking the loop, and scrolling a gif off the top is the
    /// commonest way for that to happen.
    on_screen: Vec<[u8; 32]>,
}

/// A picture decoded and encoded for the terminal, ready to be drawn.
struct Shot {
    /// One entry per frame; a still picture has exactly one. Sliced so a
    /// picture half off the top or bottom of the viewport is drawn cut,
    /// rather than vanishing whole the moment it stops fitting.
    frames: Vec<SlicedProtocol>,
    /// How long each frame stays up, in milliseconds.
    delays: Vec<u32>,
    /// Which frame is showing.
    at: usize,
    /// When the next frame is due. `None` for a still picture, which never
    /// advances and so never wakes the loop.
    next: Option<Instant>,
    /// The cell area these frames were encoded for. Encoding is the expensive
    /// part, so it is redone only when the terminal is actually resized.
    for_area: ratatui::layout::Size,
}

impl Shot {
    fn rows(&self) -> u16 {
        self.for_area.height
    }

    fn animated(&self) -> bool {
        self.frames.len() > 1
    }
}

/// Advances every visible animation whose frame is due.
///
/// Nothing here is expensive: the frames were encoded when the picture was
/// opened, so advancing is picking a different one out of the list.
fn tick_animations(app: &mut App) {
    // Under the disguise nothing is drawn, so nothing should be moving
    // either -- an app that keeps working while it claims to be idle is the
    // sort of detail that gives it away.
    if app.masked {
        return;
    }
    let now = Instant::now();
    let on_screen = std::mem::take(&mut app.on_screen);
    for (blob, shot) in app.shots.iter_mut() {
        if !shot.animated() || !on_screen.contains(blob) {
            continue;
        }
        let Some(due) = shot.next else { continue };
        if due <= now {
            shot.at = (shot.at + 1) % shot.frames.len();
            let delay = shot.delays.get(shot.at).copied().unwrap_or(100).max(20);
            // From `now` rather than from `due`: a frame that came late must
            // not make the next one land immediately and cascade.
            shot.next = Some(now + Duration::from_millis(u64::from(delay)));
        }
    }
    app.on_screen = on_screen;
}

/// When the next frame of anything currently on screen is due. `None` means
/// nothing is moving and the loop can go back to the presence clock.
fn next_frame_due(app: &App) -> Option<Instant> {
    if app.masked {
        return None;
    }
    app.shots
        .iter()
        .filter(|(blob, shot)| shot.animated() && app.on_screen.contains(blob))
        .filter_map(|(_, shot)| shot.next)
        .min()
}

impl App {
    pub fn new() -> Result<Self> {
        // A build started by an update has to wait for the one it replaced
        // to let go of the first window slot. Grabbing the second one means a
        // different data directory -- different rooms, and the two of them
        // meeting through the presence file as if two windows were open on
        // purpose. That is exactly what went wrong on the first real update.
        let dir = if crate::update::was_just_updated() {
            DataDir::open_after_update()?
        } else {
            DataDir::open()?
        };
        let nick = dir.load_nick();
        let settings = dir.load_settings();
        let proto = settings.image_proto;
        let mut app = Self {
            dir,
            screen: Screen::Home,
            sessions: Vec::new(),
            selected: 0,
            input: Input::default(),
            status: HOME_HINT.into(),
            room: None,
            net: None,
            events_rx: None,
            peers: Vec::new(),
            present: HashMap::new(),
            names: HashMap::new(),
            by_key: HashMap::new(),
            ticket: None,
            feed: Vec::new(),
            consumed: 0,
            me: [0u8; 32],
            alias: String::new(),
            nick,
            scroll: 0,
            max_scroll: 0,
            follow: true,
            unread: 0,
            masked: false,
            mask_stash: String::new(),
            offset: UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
            last_bell: None,
            settings,
            rendered: None,
            chat_area: Rect::default(),
            hover: None,
            picked: None,
            replying: None,
            hidden: HashSet::new(),
            hidden_rev: 0,
            pending_hide: None,
            expanded: HashSet::new(),
            expanded_rev: 0,
            shots: HashMap::new(),
            proto,
            pending_expand: None,
            on_screen: Vec::new(),
            whispering: None,
            sync_reach: (0, 0),
            warned_mismatch: false,
        };
        app.refresh_sessions();
        Ok(app)
    }

    fn refresh_sessions(&mut self) {
        let mut rows = Vec::new();
        for entry in self.dir.list_sessions().unwrap_or_default() {
            let Ok(bytes) = data_encoding::HEXLOWER.decode(entry.topic.as_bytes()) else {
                continue;
            };
            let Ok(topic) = <[u8; 32]>::try_from(bytes.as_slice()) else {
                continue;
            };
            rows.push(SessionRow {
                remembered: self.dir.has_pin(&topic),
                alias: entry.alias,
                topic,
            });
        }
        self.sessions = rows;
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    /// Adds a line to the transcript. Deliberately does not jump to the
    /// bottom: someone reading back must not be yanked away because a peer
    /// showed up.
    fn notice(&mut self, body: impl Into<String>) {
        self.feed.push(Feed::Notice { body: body.into() });
        if self.follow {
            self.unread = 0;
        } else {
            self.unread += 1;
        }
    }

    async fn shutdown_net(&mut self) {
        if let Some(net) = self.net.take() {
            let _ = net.shutdown().await;
        }
        self.events_rx = None;
        self.peers.clear();
        self.present.clear();
        self.ticket = None;
        self.room = None;
        self.feed.clear();
        self.by_key.clear();
        self.consumed = 0;
        self.rendered = None;
        self.hover = None;
        self.picked = None;
        self.replying = None;
        self.hidden.clear();
        self.hidden_rev += 1;
        // Nobody from the last room can be whispered to from this one, and a
        // prompt still pointing at them would be a lie.
        self.whispering = None;
        self.sync_reach = (0, 0);
        self.warned_mismatch = false;
        self.expanded.clear();
        self.expanded_rev += 1;
        self.shots.clear();
        self.on_screen.clear();
        self.unread = 0;
        self.follow = true;
        self.scroll = 0;
    }

    fn show_home(&mut self) {
        self.refresh_sessions();
        self.screen = Screen::Home;
        self.status = HOME_HINT.into();
        self.input.clear();
    }

    /// Closes an overlay back onto whatever was underneath it.
    fn close_overlay(&mut self) {
        if self.room.is_some() {
            self.screen = Screen::Chat;
            self.status = CHAT_HINT.into();
        } else {
            self.show_home();
        }
    }

    fn selected_session(&self) -> Option<(String, [u8; 32])> {
        self.sessions
            .get(self.selected)
            .map(|row| (row.alias.clone(), row.topic))
    }

    /// Which icon, if any, sits under a click. Checked before the plain
    /// "select this message" fallback.
    fn action_at(&self, row: u16, column: u16) -> Option<Action> {
        let layout = &self.rendered.as_ref()?.layout;
        let line = row.checked_sub(self.chat_area.y)? as usize + self.scroll as usize;
        if layout.toggles.iter().any(|(at, _)| *at == line) {
            return Some(Action::Expand);
        }
        let anchor = layout.actions.as_ref()?;
        if line != anchor.line {
            return None;
        }
        let column = column.checked_sub(self.chat_area.x)?;
        if anchor.reply.contains(&column) {
            return Some(Action::Reply);
        }
        if anchor.copy.contains(&column) {
            return Some(Action::Copy);
        }
        if anchor.hide.contains(&column) {
            return Some(Action::Hide);
        }
        None
    }

    /// Which message is painted on a given screen row, if any. Blank spacer
    /// rows belong to the message above them, so the pointer does not flicker
    /// in the gaps.
    fn message_at(&self, row: u16) -> Option<usize> {
        let area = self.chat_area;
        if row < area.y || row >= area.y.saturating_add(area.height) {
            return None;
        }
        let rendered = self.rendered.as_ref()?;
        let idx = (row - area.y) as usize + self.scroll as usize;
        (*rendered.layout.owners.get(idx)?)
            .filter(|i| matches!(self.feed.get(*i), Some(Feed::Msg { .. })))
    }

    /// Splits "<name> <message>" where the name may itself contain spaces --
    /// people call themselves things like "Grok 4.5". Matches the longest
    /// known nick the line starts with, so no quoting is needed.
    fn split_whisper<'a>(&self, after: &'a str) -> Option<([u8; 32], &'a str)> {
        let mut best: Option<([u8; 32], usize)> = None;
        for (id, name) in &self.names {
            if *id == self.me || name.is_empty() {
                continue;
            }
            let Some(head) = after.get(..name.len()) else {
                continue;
            };
            if !head.eq_ignore_ascii_case(name) {
                continue;
            }
            let rest = &after[name.len()..];
            // The name has to end where a word ends, or "Ana" would match
            // inside "Anabela".
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                continue;
            }
            if best.is_none_or(|(_, best_len)| name.len() > best_len) {
                best = Some((*id, name.len()));
            }
        }
        let (id, len) = best?;
        Some((id, after[len..].trim_start()))
    }

    /// Who has beaten recently, by name. Whoever went quiet simply ages out,
    /// so nobody has to announce leaving.
    fn live_now(&self) -> Vec<String> {
        let now = Instant::now();
        let mut names: Vec<String> = self
            .present
            .values()
            .filter(|live| now.duration_since(live.at) < PRESENCE_TTL)
            .map(|live| live.name.clone())
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names
    }

    /// Whether this batch of incoming text deserves a bell.
    fn wants_bell(&self, text: &str) -> bool {
        if now_ts() < self.settings.snooze_until {
            return false;
        }
        match self.settings.notify {
            Notify::Off => false,
            Notify::All => true,
            Notify::Mention => mentions(text, &self.nick),
        }
    }

    /// Best-effort audible ping, rate limited so a history sync that ingests
    /// fifty records does not machine-gun the terminal bell.
    fn ring(&mut self, text: &str) {
        if !self.wants_bell(text) {
            return;
        }
        let now = Instant::now();
        if self
            .last_bell
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(3))
        {
            return;
        }
        self.last_bell = Some(now);
        let mut out = io::stdout();
        let _ = out.write_all(b"\x07");
        let _ = out.flush();
    }
}

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture is what makes the wheel scroll the transcript. It also
    // takes over drag-select; hold shift to get the terminal's own selection
    // back.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // The one safe moment to ask the terminal what it can draw: raw mode is on
    // (the query needs it) and the input thread does not exist yet. Asking
    // later means two readers racing for the same bytes on stdin -- the answer
    // arrives as keystrokes and the query waits forever. Inside the alternate
    // screen, so anything the terminal echoes back dies with it.
    settle_image_proto();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_inner(&mut terminal).await;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

/// How long we wait for the terminal to say what it can draw. Windows Terminal
/// answers in milliseconds; anything that has not replied by now either cannot
/// or is not a terminal at all.
const PROTO_QUERY_WAIT: Duration = Duration::from_secs(3);

/// Works out how this terminal draws pictures, once per install, and writes the
/// answer down.
///
/// Deliberately never retried: the query is the one thing in this program that
/// reads stdin outside the input thread, and a second attempt while the app is
/// running would steal keystrokes. A wrong guess is fixable with `/img proto`;
/// a stolen keystroke looks like the app is broken.
fn settle_image_proto() {
    let Ok(dir) = DataDir::open() else { return };
    let mut settings = dir.load_settings();
    if settings.image_proto != ImageProto::Unknown {
        return;
    }
    // On its own thread with a deadline: the query can block on a terminal
    // that never answers, and hanging on startup is worse than halfblocks.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("proto-query".into())
        .spawn(move || {
            let found = match Picker::from_query_stdio() {
                Ok(picker) if picker.protocol_type() == ProtocolType::Sixel => ImageProto::Sixel,
                // Kitty and iTerm2 do not exist on Windows, and we would not
                // know what to do with them here anyway.
                _ => ImageProto::Halfblocks,
            };
            let _ = tx.send(found);
        })
        .ok();
    settings.image_proto = rx
        .recv_timeout(PROTO_QUERY_WAIT)
        .unwrap_or(ImageProto::Halfblocks);
    let _ = dir.save_settings(&settings);
}

/// Builds the picker from what we already decided, without touching stdin.
fn picker_for(proto: ImageProto) -> Picker {
    match proto {
        ImageProto::Sixel => {
            // `halfblocks()` is just "a picker with a sane assumed cell size"
            // -- 10x20, the same numbers CELL_W/CELL_H lay out against -- and
            // unlike the querying constructors it never touches stdin.
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(ProtocolType::Sixel);
            picker
        }
        _ => Picker::halfblocks(),
    }
}

/// Reads the terminal, tagging each event with whether it arrived as part of a
/// burst. crossterm has no bracketed paste on Windows -- `Event::Paste` is
/// parsed only on unix -- so a pasted block shows up as ordinary key events,
/// and every newline in it looks exactly like the user pressing Enter. A burst
/// is the one thing that tells them apart: either another event is already
/// queued, or the previous one landed microseconds ago. Human typing does
/// neither.
/// Counts how many events arrived back-to-back. A paste is a long run of
/// them; typing never is.
#[derive(Default)]
struct Burst {
    previous: Option<Instant>,
    run: usize,
}

impl Burst {
    /// Records an event and answers whether we are inside a paste. Deliberately
    /// needs a *run* of close events: asking "is another event queued?" was the
    /// first attempt and it misfired on every keystroke, because Windows queues
    /// a release right behind each press.
    fn observe(&mut self, now: Instant) -> bool {
        let close = self
            .previous
            .is_some_and(|at| now.duration_since(at) < PASTE_GAP);
        self.run = if close { self.run + 1 } else { 0 };
        self.previous = Some(now);
        self.run >= BURST_RUN
    }

    fn idle(&mut self) {
        self.previous = None;
        self.run = 0;
    }
}

fn spawn_input_thread() -> mpsc::UnboundedReceiver<(Event, bool)> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            let mut burst = Burst::default();
            loop {
                match event::poll(Duration::from_millis(200)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            // Every key on Windows arrives twice, pressed and
                            // released. Timing both would make each keystroke
                            // look like two events a microsecond apart.
                            if matches!(&ev, Event::Key(key) if key.kind == KeyEventKind::Release)
                            {
                                continue;
                            }
                            let pasting = burst.observe(Instant::now());
                            if tx.send((ev, pasting)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {
                        burst.idle();
                        if tx.is_closed() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("input thread");
    rx
}

async fn run_inner(terminal: &mut Term) -> Result<()> {
    let mut app = App::new()?;

    // Started by an update that just replaced us: the file we came from is
    // finally unlocked, so it can go.
    if crate::update::was_just_updated() {
        crate::update::sweep_previous();
        let now = env!("CARGO_PKG_VERSION");
        let said = match crate::update::updated_from() {
            Some(before) if before != now => format!("updated — {before} to {now}"),
            _ => format!("updated — now on {now}"),
        };
        // The room has to be reopened first: doing that clears the feed, so a
        // notice written before it would be wiped exactly in the case that
        // matters most.
        resume_room(&mut app, terminal).await;
        // In the transcript rather than only the status bar, which the next
        // network event overwrites within seconds -- and "did it actually
        // update?" is the first thing anyone wants to know.
        app.notice(said);
    }

    let mut events = spawn_input_thread();
    loop {
        // Frames advance before the draw, so what is painted is what is due.
        tick_animations(&mut app);
        terminal.draw(|f| draw(f, &mut app))?;
        // Asked *after* drawing: the draw is what establishes which pictures
        // are actually on screen, and asking before it would miss the frame
        // that was just opened and sleep on the presence clock instead.
        let next_wake = next_frame_due(&app)
            .map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or(PRESENCE_TTL / 4)
            .min(PRESENCE_TTL / 4);
        tokio::select! {
            maybe = events.recv() => {
                let Some((ev, pasting)) = maybe else { continue };
                match ev {
                    Event::Key(key) => {
                        // Windows reports press and release; only one is input.
                        if key.kind == KeyEventKind::Release {
                            continue;
                        }
                        if handle_key(&mut app, key, terminal, pasting).await? {
                            break;
                        }
                    }
                    Event::Paste(text) => paste(&mut app, &text),
                    Event::Mouse(m) => {
                        handle_mouse(&mut app, m);
                        if let Some(idx) = app.pending_hide.take() {
                            toggle_hidden(&mut app, idx).await;
                        }
                        if let Some(idx) = app.pending_expand.take() {
                            toggle_expanded(&mut app, idx).await;
                        }
                    }
                    _ => {}
                }
            }
            ev = recv_net(&mut app) => {
                if let Some(ev) = ev {
                    apply_net(&mut app, ev).await;
                }
            }
            // Nothing arrives when a room falls silent, so the clock has to
            // come from somewhere for people to age out of the roster. With a
            // gif open it also has to fire on the next frame, so the wait is
            // whichever comes first. With nothing moving -- the normal case,
            // since pictures start closed -- it is the presence clock and the
            // app goes right back to idle.
            _ = tokio::time::sleep(next_wake) => sweep_presence(&mut app),
        }
    }
    app.shutdown_net().await;
    Ok(())
}

/// Drops whoever stopped beating, and says so once.
fn sweep_presence(app: &mut App) {
    let now = Instant::now();
    let gone: Vec<([u8; 32], String)> = app
        .present
        .iter()
        .filter(|(_, live)| now.duration_since(live.at) >= PRESENCE_TTL)
        .map(|(id, live)| (*id, live.name.clone()))
        .collect();
    for (id, name) in gone {
        app.present.remove(&id);
        app.notice(format!("{name} left"));
    }
}

async fn recv_net(app: &mut App) -> Option<NetEvent> {
    match app.events_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn paste(app: &mut App, text: &str) {
    let cleaned = text.replace('\r', "");
    // A pasted key or ticket is one token; a pasted paragraph should stay one
    // message rather than firing a send per line.
    let cleaned = if matches!(app.screen, Screen::Unlock { .. }) {
        cleaned.replace('\n', "")
    } else {
        cleaned
    };
    app.input.insert_str(&cleaned);
}

async fn apply_net(app: &mut App, ev: NetEvent) {
    match ev {
        NetEvent::Status(s) => app.status = s,
        NetEvent::Ticket(t) => app.ticket = Some(t),
        // Neighbours come and go as the overlay rearranges itself; that is
        // not somebody entering or leaving the room, so it says nothing.
        NetEvent::Peers(list) => app.peers = list,
        NetEvent::Searching(count) => {
            if app.live_now().is_empty() {
                app.status = format!("looking for {count} known peer(s)…");
            }
        }
        NetEvent::Live { author, name } => {
            app.names.insert(author, name.clone());
            let arriving = app
                .present
                .insert(
                    author,
                    Live {
                        name: name.clone(),
                        at: Instant::now(),
                    },
                )
                .is_none_or(|was| Instant::now().duration_since(was.at) >= PRESENCE_TTL);
            if arriving {
                app.notice(format!("{name} is here"));
            }
        }
        NetEvent::SyncReach { tried, reached } => {
            app.sync_reach = (tried, reached);
            // Only worth saying when somebody is demonstrably in the room:
            // nobody reachable and nobody present is just an empty room.
            let people = app.live_now().len();
            if reached == 0 && tried > 0 && people > 0 && !app.warned_mismatch {
                app.warned_mismatch = true;
                app.notice(format!(
                    "{people} here, but none of them syncs history — they are on an older build.                      everyone has to update, or you each keep a partial log"
                ));
            }
            if reached > 0 {
                // Rearm, so a later mismatch is reported again.
                app.warned_mismatch = false;
            }
        }
        // Pixels arrived for a line already on screen. Nothing to say and
        // nothing to read from the log -- only the layout cache has to let go,
        // so a picture waiting on its bytes can now be opened.
        NetEvent::Blob => app.rendered = None,
        NetEvent::Record => {
            let before = app.consumed;
            let seen = app.feed.len();
            sync_feed(app).await;
            if app.consumed > before {
                if app.follow {
                    app.unread = 0;
                } else {
                    app.unread += app.consumed - before;
                }
                // Only what other people said counts towards a bell, and the
                // text is needed to tell a mention from ordinary traffic.
                let arrived: Vec<&str> = app.feed[seen..]
                    .iter()
                    .filter_map(|item| match item {
                        Feed::Msg {
                            body, mine: false, ..
                        } => Some(body.as_str()),
                        _ => None,
                    })
                    .collect();
                if !arrived.is_empty() {
                    let text = arrived.join("\n");
                    app.ring(&text);
                }
            }
        }
    }
}

/// Mirrors newly appended log records into the transcript. Called from the
/// event loop (never from `draw`), so rendering is lock-free and cannot flash
/// an empty chat while a sync holds the room.
async fn sync_feed(app: &mut App) {
    let Some(room) = app.room.clone() else { return };
    let room = room.lock().await;
    let records = room.log.records();
    if app.consumed > records.len() {
        app.consumed = 0;
    }
    for rec in records.iter().skip(app.consumed) {
        match rec {
            Record::Meta { alias } => app.feed.push(Feed::System {
                body: format!("session {alias}"),
            }),
            // Neither carries a message: one is a key, the other a heartbeat
            // that was never stored to begin with.
            Record::Identity { .. } | Record::Presence { .. } => {}
            // A whisper with nobody's name on it. Only the two ends can open
            // it; for everybody else this produces nothing at all -- not even
            // a hint that it happened, which is the whole point.
            Record::Quiet(sealed) => {
                let Some(opened) = room.open_whisper(rec) else {
                    continue;
                };
                // It has no (author, seq) to be indexed by -- that pair is
                // precisely what it refuses to publish. The random id stands
                // in: the leading bytes are unique enough to key on, and every
                // machine derives the same one, so a reply to it resolves
                // everywhere it can be read.
                let seq = u64::from_le_bytes(sealed.id[..8].try_into().unwrap_or([0; 8]));
                app.by_key.insert((opened.from, seq), app.feed.len());
                app.feed.push(Feed::Msg {
                    author: opened.from,
                    seq,
                    name: opened.name,
                    mine: opened.mine,
                    body: opened.body,
                    ts: sealed.ts,
                    reply_to: opened.reply_to,
                    whisper: Some(opened.them),
                    image: None,
                });
            }
            // Legacy whisper: same on screen, but its record announced the two
            // ends to the whole room.
            Record::Whisper {
                author, seq, ts, ..
            } => {
                let Some(opened) = room.open_whisper(rec) else {
                    continue;
                };
                app.by_key.insert((*author, *seq), app.feed.len());
                app.feed.push(Feed::Msg {
                    author: *author,
                    seq: *seq,
                    name: opened.name,
                    mine: room.is_mine(rec),
                    body: opened.body,
                    ts: *ts,
                    reply_to: opened.reply_to,
                    whisper: Some(opened.them),
                    image: None,
                });
            }
            Record::Chat { author, seq, ts, .. }
            | Record::ChatNamed {
                author, seq, ts, ..
            }
            | Record::Post {
                author, seq, ts, ..
            } => {
                let name = room.label_of(rec);
                if !matches!(rec, Record::Chat { .. }) {
                    app.names.insert(*author, name.clone());
                }
                app.by_key.insert((*author, *seq), app.feed.len());
                app.feed.push(Feed::Msg {
                    author: *author,
                    seq: *seq,
                    name,
                    mine: room.is_mine(rec),
                    body: rec.body().unwrap_or_default().to_string(),
                    ts: *ts,
                    reply_to: rec.reply_to(),
                    whisper: None,
                    image: None,
                });
            }
            Record::Image {
                author,
                seq,
                ts,
                blob,
                w,
                h,
                kind,
                bytes,
                caption,
                reply_to,
                ..
            } => {
                let name = room.label_of(rec);
                app.names.insert(*author, name.clone());
                app.by_key.insert((*author, *seq), app.feed.len());
                app.feed.push(Feed::Msg {
                    author: *author,
                    seq: *seq,
                    name,
                    mine: room.is_mine(rec),
                    // The caption doubles as the body, so a picture sent with
                    // a sentence still mentions people and rings the bell.
                    body: caption.clone(),
                    ts: *ts,
                    reply_to: *reply_to,
                    whisper: None,
                    image: Some(Box::new(ImageRef {
                        blob: *blob,
                        w: *w,
                        h: *h,
                        kind: *kind,
                        bytes: *bytes,
                    })),
                });
            }
        }
    }
    app.consumed = records.len();
    app.alias = room.alias();
    app.nick = room.nick.clone();
    app.me = room.author;
}

/// True for a real control shortcut. On an ABNT keyboard AltGr arrives as
/// CONTROL+ALT, so treating that as a shortcut would swallow `²`, `¬` and the
/// dead-key combinations this app already had to fix once.
fn is_shortcut(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

async fn handle_key<B: Backend>(
    app: &mut App,
    key: KeyEvent,
    term: &mut Terminal<B>,
    pasting: bool,
) -> Result<bool> {
    if is_shortcut(&key) && matches!(key.code, KeyCode::Char('c')) {
        return Ok(true);
    }
    if app.masked {
        // Only a deliberate keypress lifts the disguise. Brushing the keyboard
        // while someone is reading over your shoulder must not undo it.
        if matches!(key.code, KeyCode::F(12) | KeyCode::Esc) {
            toggle_mask(app);
        }
        return Ok(false);
    }
    if key.code == KeyCode::F(12) {
        toggle_mask(app);
        return Ok(false);
    }
    // Overlays swallow everything: one key in, one key out.
    match app.screen.clone() {
        Screen::Help => {
            app.close_overlay();
            return Ok(false);
        }
        Screen::Confirm { alias, topic } => {
            if key.code == KeyCode::Enter {
                wipe(app, &alias, &topic).await?;
            } else {
                app.close_overlay();
                app.status = format!("{alias} kept");
            }
            return Ok(false);
        }
        Screen::Upgrade {
            version, manifest, ..
        } => {
            if key.code == KeyCode::Enter {
                // `true` means the replacement is running and this process
                // should stand down.
                return install_update(app, &version, &manifest, term).await;
            }
            app.close_overlay();
            app.status = format!(
                "staying on {} — /update when you want it",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(false);
        }
        _ => {}
    }
    if key.code == KeyCode::F(1) {
        app.screen = Screen::Help;
        return Ok(false);
    }
    if edit_key(app, key) {
        return Ok(false);
    }
    match app.screen.clone() {
        Screen::Home => handle_home(app, key, term).await,
        Screen::Unlock { alias, topic } => handle_unlock(app, key, term, &alias, topic).await,
        Screen::Chat => handle_chat(app, key, term, pasting).await,
        Screen::Help | Screen::Confirm { .. } | Screen::Upgrade { .. } => Ok(false),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Reply,
    Copy,
    Hide,
    /// The `image (+)` / `image (-)` line itself is the button; clicking the
    /// picture's own line is more obvious than hunting for a fourth icon in
    /// the hover ruler.
    Expand,
}

const HIDDEN_FILE: &str = "hidden.bin";

async fn load_hidden(app: &mut App) {
    let Some(room) = app.room.clone() else { return };
    let stored = room.lock().await.log.read_side(HIDDEN_FILE);
    app.hidden = stored
        .and_then(|bytes| postcard::from_bytes::<Vec<([u8; 32], u64)>>(&bytes).ok())
        .map(|list| list.into_iter().collect())
        .unwrap_or_default();
    app.hidden_rev += 1;
}

async fn save_hidden(app: &App) {
    let Some(room) = app.room.clone() else { return };
    let list: Vec<([u8; 32], u64)> = app.hidden.iter().copied().collect();
    if let Ok(bytes) = postcard::to_stdvec(&list) {
        let _ = room.lock().await.log.write_side(HIDDEN_FILE, &bytes);
    }
}

/// Blurs a message, or brings it back. Toggling leaves it revealed until you
/// hide it again -- no timer, nothing surprising.
async fn toggle_hidden(app: &mut App, idx: usize) {
    let Some(Feed::Msg { author, seq, .. }) = app.feed.get(idx) else {
        return;
    };
    let key = (*author, *seq);
    let blurred = if app.hidden.remove(&key) {
        false
    } else {
        app.hidden.insert(key);
        true
    };
    app.hidden_rev += 1;
    app.status = if blurred {
        "message hidden on this screen only".into()
    } else {
        "message shown again".into()
    };
    save_hidden(app).await;
}

/// Opens or closes the picture on message `idx`.
///
/// Closing throws the decoded frames away rather than keeping them warm: a
/// closed picture must not be sitting in memory ready to paint, and a room
/// scrolled through end to end would otherwise hold every screenshot ever sent.
async fn toggle_expanded(app: &mut App, idx: usize) {
    let Some(Feed::Msg {
        author,
        seq,
        image: Some(img),
        ..
    }) = app.feed.get(idx)
    else {
        app.status = "that message has no picture".into();
        return;
    };
    let (key, img) = ((*author, *seq), (**img).clone());

    if app.expanded.remove(&key) {
        app.shots.remove(&img.blob);
        app.expanded_rev += 1;
        app.status = "picture closed".into();
        return;
    }
    if app.masked {
        app.status = "not while the disguise is on".into();
        return;
    }
    app.expanded.insert(key);
    app.expanded_rev += 1;
    match build_shot(app, &img).await {
        Ok(shot) => {
            let frames = shot.frames.len();
            app.shots.insert(img.blob, shot);
            app.status = if frames > 1 {
                format!("picture open — {frames} frames")
            } else {
                "picture open".into()
            };
        }
        Err(why) => {
            // Stays "expanded" so the line reads as waiting rather than
            // silently snapping shut under the pointer.
            app.status = why;
        }
    }
}

/// Decodes a blob and encodes it for this terminal, at the size it will be
/// drawn. The encoding is the expensive half, so it happens here -- once, on a
/// deliberate keypress -- and not while drawing frames.
async fn build_shot(app: &App, img: &ImageRef) -> Result<Shot, String> {
    let Some(room) = &app.room else {
        return Err("no room open".into());
    };
    let bytes = { room.lock().await.log.read_blob(&img.blob) };
    let Some(bytes) = bytes else {
        return Err("waiting for the pixels to arrive…".into());
    };

    let width = app.chat_area.width;
    let max_cols = if width <= 34 {
        width.saturating_sub(4)
    } else {
        (width * 7) / 10
    };
    let (cols, rows) = fit_cells(img.w, img.h, max_cols, IMAGE_MAX_ROWS);
    if cols == 0 || rows == 0 {
        return Err("terminal too small for that picture".into());
    }

    let frames = crate::media::frames(&bytes, img.kind, MAX_GIF_FRAMES)
        .map_err(|e| format!("cannot read that picture: {e}"))?;
    let picker = picker_for(app.proto);
    let area = ratatui::layout::Size::new(cols, rows);
    let mut encoded = Vec::with_capacity(frames.len());
    let mut delays = Vec::with_capacity(frames.len());
    for frame in frames {
        let proto = SlicedProtocol::new_with_resize(
            &picker,
            frame.image,
            area,
            ratatui_image::Resize::Fit(None),
        )
        .map_err(|e| format!("cannot draw that picture: {e}"))?;
        encoded.push(proto);
        delays.push(frame.delay_ms);
    }
    let next = (encoded.len() > 1)
        .then(|| Instant::now() + Duration::from_millis(u64::from(delays[0].max(20))));
    Ok(Shot {
        frames: encoded,
        delays,
        at: 0,
        next,
        for_area: area,
    })
}

/// Points the next message at `idx`.
fn arm_reply(app: &mut App, idx: usize) {
    let Some(Feed::Msg {
        author, seq, name, ..
    }) = app.feed.get(idx)
    else {
        return;
    };
    app.replying = Some((*author, *seq));
    app.picked = Some(idx);
    app.status = format!("replying to {name}");
}

fn copy_message(app: &mut App, idx: usize) {
    let Some(Feed::Msg { body, .. }) = app.feed.get(idx) else {
        return;
    };
    let body = body.clone();
    app.status = if crate::sys::copy(&body) {
        "message copied".into()
    } else {
        "could not reach the clipboard".into()
    };
}

/// Completes the name in `/w <name> ...`, so whispering is a couple of keys
/// rather than an exact spelling.
fn complete_nick(app: &mut App) {
    let text = app.input.text.clone();
    // Everything after the command is the candidate, so a name with spaces in
    // it can still be completed.
    let Some((cmd, partial)) = text.split_once(' ') else {
        return;
    };
    if !matches!(cmd, "/w" | "/whisper") {
        return;
    }
    let partial = partial.to_lowercase();
    let hit = app
        .names
        .iter()
        .filter(|(id, _)| **id != app.me)
        .map(|(_, name)| name.clone())
        .find(|name| name.to_lowercase().starts_with(&partial));
    if let Some(name) = hit {
        app.input.text = format!("{cmd} {name} ");
        app.input.cursor = app.input.len();
    }
}

/// Walks the selection through messages, skipping notices.
fn pick_step(app: &mut App, back: bool) {
    let messages: Vec<usize> = app
        .feed
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, Feed::Msg { .. }))
        .map(|(at, _)| at)
        .collect();
    let Some(last) = messages.len().checked_sub(1) else {
        return;
    };
    let at = app
        .picked
        .and_then(|picked| messages.iter().position(|m| *m == picked));
    // Starting fresh lands on the newest message, which is what you almost
    // always want to answer.
    let next = match (at, back) {
        (None, _) => last,
        (Some(i), true) => i.saturating_sub(1),
        (Some(i), false) => (i + 1).min(last),
    };
    app.picked = Some(messages[next]);
}

fn handle_mouse(app: &mut App, ev: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !matches!(app.screen, Screen::Chat) {
        return;
    }
    match ev.kind {
        MouseEventKind::ScrollUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => scroll_down(app, 3),
        // Movement tracking fires constantly; only a change of target is worth
        // reacting to.
        MouseEventKind::Moved => {
            let at = app.message_at(ev.row);
            if at != app.hover {
                app.hover = at;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(idx) = app.message_at(ev.row) else {
                return;
            };
            match app.action_at(ev.row, ev.column) {
                Some(Action::Reply) => arm_reply(app, idx),
                Some(Action::Copy) => copy_message(app, idx),
                // Hiding writes to disk, which the mouse handler cannot await;
                // the click marks the message and the key does the rest.
                Some(Action::Hide) => app.pending_hide = Some(idx),
                // Same reason as Hide: opening reads a blob and decodes it,
                // which the mouse handler has no way to await.
                Some(Action::Expand) => app.pending_expand = Some(idx),
                None => app.picked = Some(idx),
            }
        }
        _ => {}
    }
}

fn scroll_down(app: &mut App, by: u16) {
    app.scroll = app.scroll.saturating_add(by).min(app.max_scroll);
    if app.scroll >= app.max_scroll {
        app.follow = true;
        app.unread = 0;
    }
}

async fn wipe(app: &mut App, alias: &str, topic: &[u8; 32]) -> Result<()> {
    if app.room.is_some() {
        app.shutdown_net().await;
    }
    app.dir.forget(topic)?;
    app.show_home();
    app.status = format!("{alias} wiped from this pc");
    Ok(())
}

fn toggle_mask(app: &mut App) {
    app.masked = !app.masked;
    if app.masked {
        app.mask_stash = std::mem::take(&mut app.input.text);
        app.input.cursor = 0;
    } else {
        app.input.text = std::mem::take(&mut app.mask_stash);
        app.input.cursor = app.input.len();
    }
}

/// Line editing shared by every screen. Returns true when the key was consumed.
fn edit_key(app: &mut App, key: KeyEvent) -> bool {
    let ctrl = is_shortcut(&key);
    match key.code {
        KeyCode::Left => app.input.cursor = app.input.cursor.saturating_sub(1),
        KeyCode::Right => {
            if app.input.cursor < app.input.len() {
                app.input.cursor += 1;
            }
        }
        // Ctrl+Home / Ctrl+End belong to the transcript, not the line.
        KeyCode::Home if !ctrl => app.input.cursor = 0,
        KeyCode::End if !ctrl => app.input.cursor = app.input.len(),
        // Only claim Delete when there is something to its right, so an empty
        // line leaves it free to mean "delete this session".
        KeyCode::Delete if app.input.cursor < app.input.len() => app.input.delete(),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Char('w') if ctrl => app.input.kill_word(),
        KeyCode::Char('u') if ctrl => app.input.kill_to_start(),
        KeyCode::Char('a') if ctrl => app.input.cursor = 0,
        KeyCode::Char('e') if ctrl => app.input.cursor = app.input.len(),
        KeyCode::Char(c) if !ctrl => app.input.insert(c),
        _ => return false,
    }
    true
}

async fn handle_home<B: Backend>(
    app: &mut App,
    key: KeyEvent,
    term: &mut Terminal<B>,
) -> Result<bool> {
    match key.code {
        KeyCode::Up if app.selected > 0 => app.selected -= 1,
        KeyCode::Down if app.selected + 1 < app.sessions.len() => app.selected += 1,
        KeyCode::Delete => match app.selected_session() {
            Some((alias, topic)) => app.screen = Screen::Confirm { alias, topic },
            None => app.status = "nothing to delete".into(),
        },
        KeyCode::Esc => {
            app.input.clear();
            app.status = HOME_HINT.into();
        }
        KeyCode::Enter => {
            let line = app.input.take();
            let line = line.trim().to_string();
            if line.starts_with('/') {
                return handle_command(app, line, term).await;
            }
            if !line.is_empty() {
                app.status = "commands start with a slash — try /help".into();
                return Ok(false);
            }
            let Some(row) = app.sessions.get(app.selected) else {
                app.status = "no sessions yet — /new gpt-oss-20b".into();
                return Ok(false);
            };
            let (alias, topic, remembered) = (row.alias.clone(), row.topic, row.remembered);
            if remembered {
                if let Some(pin) = app.dir.recall_pin(&topic) {
                    app.status = "opening…".into();
                    let _ = term.draw(|f| draw(f, app));
                    open_room(app, pin, Some(&alias), Vec::new(), false).await;
                    return Ok(false);
                }
                app.status = "saved key no longer opens — enter it again".into();
            }
            app.screen = Screen::Unlock { alias, topic };
            app.status = "enter the key   esc = back".into();
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_unlock<B: Backend>(
    app: &mut App,
    key: KeyEvent,
    term: &mut Terminal<B>,
    alias: &str,
    topic: [u8; 32],
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => app.show_home(),
        KeyCode::Enter => {
            let raw = app.input.take();
            match Pin::parse(&raw) {
                Ok(pin) => {
                    if topic_id(&pin) != topic {
                        app.status = "that key does not match this session".into();
                        return Ok(false);
                    }
                    app.status = "unlocking…".into();
                    let _ = term.draw(|f| draw(f, app));
                    open_room(app, pin, Some(alias), Vec::new(), true).await;
                }
                Err(e) => app.status = e.to_string(),
            }
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_chat<B: Backend>(
    app: &mut App,
    key: KeyEvent,
    term: &mut Terminal<B>,
    pasting: bool,
) -> Result<bool> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let page = 6u16;
    match key.code {
        KeyCode::Esc => {
            // Unwinds one step at a time: the armed reply, then the line, then
            // the whisper the prompt is pointed at. Leaving the whisper for
            // last means a half-typed private line is never sent to the room
            // by one keystroke too many -- clearing it comes first.
            if app.replying.is_some() {
                app.replying = None;
                app.picked = None;
                app.status = CHAT_HINT.into();
            } else if app.picked.is_some() {
                app.picked = None;
            } else if !app.input.text.is_empty() {
                app.input.clear();
            } else if app.whispering.take().is_some() {
                app.status = "back to the room".into();
            } else {
                app.status = "/leave goes back to the session list".into();
            }
        }
        KeyCode::Tab => complete_nick(app),
        KeyCode::Up if alt => pick_step(app, true),
        KeyCode::Down if alt => pick_step(app, false),
        KeyCode::Char('r') if is_shortcut(&key) => match app.picked.or(app.hover) {
            Some(idx) => arm_reply(app, idx),
            None => app.status = "alt+up picks a message to answer".into(),
        },
        KeyCode::Char('h') if is_shortcut(&key) => match app.picked.or(app.hover) {
            Some(idx) => toggle_hidden(app, idx).await,
            None => app.status = "alt+up picks a message to hide".into(),
        },
        KeyCode::Char('y') if is_shortcut(&key) => match app.picked.or(app.hover) {
            Some(idx) => copy_message(app, idx),
            None => app.status = "alt+up picks a message to copy".into(),
        },
        KeyCode::Char('g') if is_shortcut(&key) => match app.picked.or(app.hover) {
            Some(idx) => toggle_expanded(app, idx).await,
            None => app.status = "alt+up picks a picture to open".into(),
        },
        // Ctrl+Shift+V always means "paste the picture", because Ctrl+V may
        // never reach us: on Windows the terminal usually swallows it and
        // injects the clipboard text itself.
        KeyCode::Char('v' | 'V') if is_shortcut(&key) && shift => paste_image(app).await,
        // Plain Ctrl+V, for the terminals that do pass it through. Guarded on
        // there actually being a picture, so a text paste that also arrived
        // as injected keystrokes is never doubled up.
        KeyCode::Char('v') if is_shortcut(&key) && crate::sys::has_image() => {
            paste_image(app).await
        }
        KeyCode::PageUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_sub(page);
        }
        KeyCode::PageDown => scroll_down(app, page),
        KeyCode::Home if is_shortcut(&key) => {
            app.follow = false;
            app.scroll = 0;
        }
        KeyCode::End if is_shortcut(&key) => {
            app.follow = true;
            app.unread = 0;
        }
        KeyCode::Up => app.input.recall_prev(),
        KeyCode::Down => app.input.recall_next(),
        // Shift+Enter is the usual one, but some terminals swallow it, so alt
        // and ctrl do the same. A newline inside a paste belongs to the
        // message rather than to the send key.
        KeyCode::Enter
            if shift || alt || is_shortcut(&key) || (pasting && app.settings.paste_detect) =>
        {
            app.input.insert('\n')
        }
        KeyCode::Enter => {
            let line = app.input.take();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Ok(false);
            }
            if trimmed.starts_with('/') {
                return handle_command(app, trimmed.to_string(), term).await;
            }
            send(app, line).await;
        }
        _ => {}
    }
    Ok(false)
}

/// The other end of the whisper the pending reply points at, if it points at
/// a whisper at all.
fn whisper_being_answered(app: &App) -> Option<[u8; 32]> {
    let key = app.replying?;
    let at = *app.by_key.get(&key)?;
    match app.feed.get(at) {
        Some(Feed::Msg {
            whisper: Some(them),
            ..
        }) => Some(*them),
        _ => None,
    }
}

async fn send(app: &mut App, body: String) {
    // The prompt says where this is going, and it is the prompt that decides.
    if let Some(them) = app.whispering {
        send_whisper(app, them, body).await;
        return;
    }
    // Answering a whisper out loud is refused, and this is the one place that
    // can tell. The danger is not that the room reads the whisper -- it
    // cannot, the quote degrades to "not here yet" for anyone who cannot open
    // it. It is that *we* see the quote in full, attached to a public message,
    // and write the next sentence as if everyone shared that context.
    if let Some(them) = whisper_being_answered(app) {
        let who = app
            .names
            .get(&them)
            .cloned()
            .unwrap_or_else(|| "them".into());
        // Handed back as a ready command rather than thrown away: one Enter
        // sends it privately, Esc drops the quote and it goes to the room.
        app.input.clear();
        app.input.insert_str(&format!("/w {who} {}", body.trim()));
        app.status = format!("that is a whisper — enter sends it to {who}, esc drops the quote");
        return;
    }
    let Some(room) = app.room.clone() else { return };
    let reply = app.replying;
    let rec = {
        let mut room = room.lock().await;
        match room.compose(body, reply) {
            Ok(rec) => rec,
            Err(e) => {
                app.status = format!("could not save message: {e}");
                return;
            }
        }
    };
    app.replying = None;
    app.picked = None;
    app.follow = true;
    app.unread = 0;
    sync_feed(app).await;
    match &app.net {
        Some(net) => {
            if let Err(e) = net.broadcast(&rec).await {
                app.status = format!("not delivered: {e}");
            } else if app.peers.is_empty() {
                app.status = "nobody is here yet — they will get it when they join".into();
            } else {
                app.status = CHAT_HINT.into();
            }
        }
        None => app.status = "offline — saved here, not sent".into(),
    }
}

async fn handle_command<B: Backend>(
    app: &mut App,
    line: String,
    term: &mut Terminal<B>,
) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest = parts.clone().collect::<Vec<_>>().join(" ");
    match cmd {
        "/quit" | "/exit" => return Ok(true),
        "/new" => {
            if rest.is_empty() {
                app.status = "usage: /new gpt-oss-20b".into();
                return Ok(false);
            }
            app.status = "creating…".into();
            let _ = term.draw(|f| draw(f, app));
            match OpenRoom::create(&app.dir, &rest) {
                Ok(room) => {
                    let pin = room.pin.clone();
                    start_session(app, room, Vec::new()).await;
                    remember(app, &pin);
                    let shown = pin.display();
                    let copied = crate::sys::copy(&shown);
                    app.notice(format!(
                        "session created. key {shown}{}",
                        if copied { " (copied)" } else { "" }
                    ));
                    app.notice("say the key out loud, do not paste it in teams. /pin shows it again.");
                }
                Err(e) => app.status = format!("could not create: {e}"),
            }
        }
        "/join" => {
            let mut args = line.split_whitespace().skip(1);
            let raw = args.next().unwrap_or("");
            let ticket = args.next();
            match Pin::parse(raw) {
                Ok(pin) => {
                    let mut boots = Vec::new();
                    if let Some(t) = ticket {
                        match parse_ticket(t) {
                            Ok(addr) => boots.push(addr),
                            Err(e) => {
                                app.status = format!("bad ticket: {e}");
                                return Ok(false);
                            }
                        }
                    }
                    app.status = "joining…".into();
                    let _ = term.draw(|f| draw(f, app));
                    open_room(app, pin, None, boots, true).await;
                }
                Err(e) => app.status = e.to_string(),
            }
        }
        "/pin" | "/key" => match &app.room {
            Some(room) => {
                let shown = room.lock().await.pin.display();
                let copied = crate::sys::copy(&shown);
                app.notice(format!(
                    "key {shown}{}",
                    if copied { " (copied)" } else { "" }
                ));
            }
            None => app.status = "open a session first".into(),
        },
        "/ticket" => match app.ticket.clone() {
            Some(t) => {
                let copied = crate::sys::copy(&t);
                app.notice(if copied {
                    format!("ticket copied to the clipboard ({} chars)", t.len())
                } else {
                    format!("ticket {t}")
                });
                app.notice("they run: /join <key> <ticket>");
            }
            None => app.status = "no ticket yet — the network is still starting".into(),
        },
        "/peers" => {
            let names = app.live_now();
            if names.is_empty() {
                app.notice("nobody else is live right now");
            } else {
                app.notice(format!("live now: {}", names.join(", ")));
            }
        }
        "/nick" | "/name" => {
            if rest.is_empty() {
                let current = app.nick.clone();
                app.status = format!("you are {current} — /nick <name> to change");
                return Ok(false);
            }
            let result = match &app.room {
                Some(room) => room.lock().await.set_nick(&app.dir, rest.clone()),
                None => crate::room::normalize_nick(&rest).and_then(|n| {
                    app.dir.save_nick(&n)?;
                    Ok(())
                }),
            };
            match result {
                Ok(()) => {
                    app.nick = app.dir.load_nick();
                    let nick = app.nick.clone();
                    app.notice(format!(
                        "you are now {nick}. older messages keep the old name."
                    ));
                }
                Err(e) => app.status = format!("{e}"),
            }
        }
        "/leave" => {
            if app.room.is_some() {
                app.shutdown_net().await;
                app.show_home();
            } else {
                app.status = "already on the session list".into();
            }
        }
        "/lock" => {
            let topic = match &app.room {
                Some(room) => Some(topic_id(&room.lock().await.pin)),
                None => app.sessions.get(app.selected).map(|r| r.topic),
            };
            match topic {
                Some(topic) => {
                    app.dir.forget_pin(&topic)?;
                    app.refresh_sessions();
                    app.notice("key no longer saved on this pc — it will be asked next time");
                }
                None => app.status = "nothing to lock".into(),
            }
        }
        "/forget" | "/delete" => return forget(app, &rest).await,
        "/w" | "/whisper" => whisper_cmd(app, &line).await,
        "/paste" => paste_cmd(app, &rest)?,
        "/img" | "/image" => image_cmd(app, &line).await,
        "/update" => check_for_update(app, term).await,
        "/notify" | "/mute" => notify_cmd(app, &rest)?,
        "/diag" => diag(app).await,
        "/help" | "/?" => app.screen = Screen::Help,
        other => app.status = format!("no such command: {other} — /help"),
    }
    Ok(false)
}

/// `/img <path>` sends a picture, `/img proto ...` overrides how they are
/// drawn. The path is taken from the raw line rather than the whitespace-split
/// `rest`, so `C:\Users\Pedro Ailton\erro.png` survives.
async fn image_cmd(app: &mut App, line: &str) {
    let arg = line
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");

    if let Some(which) = arg.strip_prefix("proto").map(str::trim) {
        let chosen = match which {
            "sixel" => ImageProto::Sixel,
            "halfblocks" | "blocks" => ImageProto::Halfblocks,
            // Clears the stored answer; the next start asks the terminal again.
            "auto" | "" => ImageProto::Unknown,
            other => {
                app.status = format!("unknown protocol {other} — sixel, halfblocks or auto");
                return;
            }
        };
        let mut settings = app.dir.load_settings();
        settings.image_proto = chosen;
        let _ = app.dir.save_settings(&settings);
        app.settings.image_proto = chosen;
        app.proto = chosen;
        // Anything already encoded was drawn the old way.
        app.shots.clear();
        app.expanded.clear();
        app.expanded_rev += 1;
        app.status = match chosen {
            ImageProto::Sixel => "drawing pictures as sixels".into(),
            ImageProto::Halfblocks => "drawing pictures as half-blocks".into(),
            ImageProto::Unknown => "will ask the terminal again on next start".into(),
        };
        return;
    }

    if app.room.is_none() {
        app.status = "open a room first".into();
        return;
    }
    // Bare `/img` takes whatever is on the clipboard, which is the short way
    // round after win+shift+s.
    if arg.is_empty() {
        paste_image(app).await;
        return;
    }
    // Quotes are what the Explorer's "copy as path" puts around a path.
    let path = arg.trim_matches('"');
    let raw = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            app.status = format!("cannot read {path}: {e}");
            return;
        }
    };
    send_image(app, raw).await;
}

/// Sends whatever picture is on the clipboard, if there is one.
async fn paste_image(app: &mut App) {
    if app.room.is_none() {
        app.status = "open a room first".into();
        return;
    }
    match crate::sys::grab_image() {
        Some(grab) => match crate::media::from_clipboard(grab) {
            Ok(bytes) => send_image(app, bytes).await,
            Err(e) => app.status = format!("clipboard: {e}"),
        },
        None => app.status = "no picture on the clipboard — win+shift+s snips one".into(),
    }
}

/// Shrinks a picture to something worth putting on a LAN, files it, and
/// announces it.
async fn send_image(app: &mut App, raw: Vec<u8>) {
    let ready = match crate::media::prepare(&raw) {
        Ok(ready) => ready,
        Err(e) => {
            app.status = e.to_string();
            return;
        }
    };
    let Some(room) = app.room.clone() else { return };
    // A picture always goes to the room, so a quoted whisper cannot come with
    // it -- same reasoning as `send`. Here the quote is dropped rather than
    // the send refused: there is no way to hand a picture back to the input,
    // and refusing would mean snipping it again.
    let mut reply = app.replying;
    let mut dropped = None;
    if let Some(other) = whisper_being_answered(app) {
        dropped = Some(
            app.names
                .get(&other)
                .cloned()
                .unwrap_or_else(|| "them".into()),
        );
        reply = None;
    }
    let rec = {
        let mut room = room.lock().await;
        match room.compose_image(
            &ready.bytes,
            ready.w,
            ready.h,
            ready.kind,
            String::new(),
            reply,
        ) {
            Ok(rec) => rec,
            Err(e) => {
                app.status = format!("could not save picture: {e}");
                return;
            }
        }
    };
    app.replying = None;
    app.picked = None;
    app.follow = true;
    app.unread = 0;
    sync_feed(app).await;
    let size = format!("{}x{}", ready.w, ready.h);
    let note = dropped
        .map(|who| format!(" — quote dropped, only {who} can read that whisper"))
        .unwrap_or_default();
    match &app.net {
        Some(net) => {
            // Only the description goes out over gossip; the pixels wait for
            // whoever wants them to ask on their own stream.
            if let Err(e) = net.broadcast(&rec).await {
                app.status = format!("not delivered: {e}{note}");
            } else {
                app.status = format!(
                    "sent {size}, {}{note}",
                    human_bytes(ready.bytes.len() as u32)
                );
            }
        }
        None => app.status = format!("offline — {size} saved here, not sent{note}"),
    }
}

/// Asks the release channel whether there is something newer.
///
/// The only place in the program that talks to anything outside the LAN, and
/// only when somebody types `/update`. Anything that goes wrong -- no network,
/// github down, a manifest we do not trust -- is reported and dropped; the app
/// keeps working offline exactly as before.
async fn check_for_update<B: Backend>(app: &mut App, term: &mut Terminal<B>) {
    app.status = "checking for a new build…".into();
    let _ = term.draw(|f| draw(f, app));

    let manifest = match crate::update::fetch_manifest().await {
        Ok(manifest) => manifest,
        Err(e) => {
            app.status = format!("could not check: {e}");
            return;
        }
    };
    match manifest.against(crate::update::Version::current()) {
        Ok(crate::update::Check::UpToDate(running)) => {
            app.status = format!("already on the newest build ({running})");
        }
        Ok(crate::update::Check::Available {
            version,
            manifest,
            required,
        }) => {
            app.screen = Screen::Upgrade {
                version: version.to_string(),
                manifest,
                required,
            };
        }
        Err(e) => app.status = format!("could not read the release: {e}"),
    }
}

/// Downloads, verifies and installs, then hands over to the new build.
///
/// Answers `true` when the replacement is running and this process should
/// exit. The open room is noted first -- by topic only, never the key -- so
/// the new build comes back to it.
async fn install_update<B: Backend>(
    app: &mut App,
    version: &str,
    manifest: &crate::update::Manifest,
    term: &mut Terminal<B>,
) -> Result<bool> {
    app.status = format!("downloading {version}…");
    let _ = term.draw(|f| draw(f, app));

    // Nothing touches the disk until both the digest and the release
    // signature hold.
    let bytes = match crate::update::fetch_binary(manifest).await {
        Ok(bytes) => bytes,
        Err(e) => {
            app.close_overlay();
            app.status = format!("update refused: {e}");
            return Ok(false);
        }
    };

    // Written through the app's own DataDir. Opening a fresh one here would
    // claim a second window slot and put the note in `guest-2`, where the
    // build that has to read it will never look.
    if let Some(room) = &app.room {
        let topic = topic_hex(&topic_id(&room.lock().await.pin));
        let _ = crate::update::write_resume(&app.dir, &topic);
    }

    app.status = "installing…".into();
    let _ = term.draw(|f| draw(f, app));
    app.shutdown_net().await;
    // Let go of the window slot *before* the successor starts, or it will
    // take the next one and open a different data directory entirely.
    app.dir.release_slot();

    match crate::update::install_and_relaunch(&bytes) {
        Ok(()) => Ok(true),
        Err(e) => {
            app.close_overlay();
            app.status = format!("could not install: {e}");
            Ok(false)
        }
    }
}

/// Wiping history is irreversible and there is no second copy. Naming the
/// session goes straight through; anything else stops on a confirm screen.
async fn forget(app: &mut App, arg: &str) -> Result<bool> {
    let target = match app.room.clone() {
        Some(room) => Some((app.alias.clone(), topic_id(&room.lock().await.pin))),
        None => app.selected_session(),
    };
    let Some((alias, topic)) = target else {
        app.status = "no session selected".into();
        return Ok(false);
    };
    if arg.trim() == alias {
        wipe(app, &alias, &topic).await?;
    } else {
        app.screen = Screen::Confirm { alias, topic };
    }
    Ok(false)
}

async fn diag(app: &mut App) {
    let instance = app.dir.instance;
    let presence = app.dir.presence_dir();
    let files = std::fs::read_dir(&presence)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    let known = match &app.net {
        Some(net) => net.known_count().await,
        None => 0,
    };
    app.notice(format!(
        "window #{instance} · network {} · {} in the room · {} gossip neighbour(s) · {known} route(s)",
        if app.net.is_some() { "up" } else { "down" },
        app.live_now().len(),
        app.peers.len()
    ));
    // The one number that explains "messages arrive but history does not":
    // live traffic rides iroh's gossip ALPN, history rides ours, and only
    // ours changes between versions.
    let (tried, reached) = app.sync_reach;
    app.notice(format!(
        "history sync: {reached}/{tried} peer(s) answered on {} — this build is {}",
        String::from_utf8_lossy(crate::net::SYNC_ALPN),
        env!("CARGO_PKG_VERSION"),
    ));
    if tried > 0 && reached == 0 {
        app.notice(
            "nobody answered: they are running a different version. live messages              still come through gossip, which is why it looks like it works",
        );
    }
    let remembered = match app.room.clone() {
        Some(room) => crate::net::load_peers(&*room.lock().await).len(),
        None => 0,
    };
    app.notice(format!(
        "presence dir {} ({files} file(s)) · {remembered} peer(s) remembered from before",
        presence.display()
    ));
    if app.peers.is_empty() {
        app.notice(
            "no peers: check the windows firewall prompt was allowed on private networks, and that both machines are on the same subnet. /ticket works around mdns.",
        );
    }
}

/// Reopens the room the app was in before an update restarted it.
///
/// Only rooms whose key is remembered on this machine come back by themselves.
/// A locked one stops at its unlock screen rather than being skipped, so the
/// restart does not quietly drop somebody out of the conversation.
async fn resume_room<B: Backend>(app: &mut App, term: &mut Terminal<B>) {
    let Some(topic_hex_wanted) = crate::update::take_resume(&app.dir) else {
        return;
    };
    let Some(row) = app
        .sessions
        .iter()
        .find(|row| topic_hex(&row.topic) == topic_hex_wanted)
        .map(|row| (row.alias.clone(), row.topic, row.remembered))
    else {
        return;
    };
    let (alias, topic, remembered) = row;
    if remembered {
        if let Some(pin) = app.dir.recall_pin(&topic) {
            let _ = term.draw(|f| draw(f, app));
            open_room(app, pin, Some(&alias), Vec::new(), false).await;
            return;
        }
    }
    app.screen = Screen::Unlock { alias, topic };
    app.status = "back after the update — enter the key".into();
}

async fn open_room(
    app: &mut App,
    pin: Pin,
    alias: Option<&str>,
    bootstrap: Vec<EndpointAddr>,
    remember_key: bool,
) {
    match OpenRoom::join(&app.dir, pin.clone(), alias) {
        Ok(room) => {
            start_session(app, room, bootstrap).await;
            if remember_key {
                remember(app, &pin);
            }
        }
        Err(e) => app.status = format!("could not open: {e}"),
    }
}

fn remember(app: &mut App, pin: &Pin) {
    if let Err(e) = app.dir.remember_pin(&topic_id(pin), pin) {
        app.status = format!("key not saved on this pc: {e}");
    }
}

async fn start_session(app: &mut App, room: OpenRoom, bootstrap: Vec<EndpointAddr>) {
    app.shutdown_net().await;
    let secret = room.secret.clone();
    let shared = Arc::new(Mutex::new(room));
    let (tx, rx) = mpsc::unbounded_channel();
    app.events_rx = Some(rx);
    app.room = Some(shared.clone());
    app.screen = Screen::Chat;
    app.status = CHAT_HINT.into();
    sync_feed(app).await;
    load_hidden(app).await;
    // Escape hatch for a machine where the firewall says no: the history is
    // still readable and writable, it just does not go anywhere.
    if std::env::var_os("LOCAL_LLM_OFFLINE").is_some() {
        app.notice("offline mode: reading local history only, nothing is sent.");
        return;
    }
    let presence = Presence {
        dir: app.dir.presence_dir(),
        instance: app.dir.instance,
    };
    // Publish our whisper key before anything else, so peers can reach us
    // privately as soon as they see us.
    let announced = shared.lock().await.announce_identity().ok().flatten();
    match NetSession::start(secret, shared, tx, bootstrap, presence).await {
        Ok(net) => {
            app.ticket = Some(net.addr());
            if let Some(rec) = &announced {
                let _ = net.broadcast(rec).await;
            }
            app.net = Some(net);
        }
        Err(e) => {
            app.notice(format!("offline: {e}"));
            app.notice("messages are saved here and will sync when the network comes back.");
        }
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ').filter(|w| !w.is_empty()) {
            let wlen = word.chars().count();
            if !line.is_empty() && line.chars().count() + 1 + wlen > width {
                out.push(std::mem::take(&mut line));
            }
            if wlen > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                for ch in word.chars() {
                    if line.chars().count() == width {
                        out.push(std::mem::take(&mut line));
                    }
                    line.push(ch);
                }
                continue;
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

fn local_dt(ts: u64, offset: UtcOffset) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(ts as i64)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset)
}

fn clock(ts: u64, offset: UtcOffset) -> String {
    let dt = local_dt(ts, offset);
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

fn civil(ts: u64, offset: UtcOffset) -> (i32, u8, u8) {
    let dt = local_dt(ts, offset);
    (dt.year(), u8::from(dt.month()), dt.day())
}

fn day_label(ts: u64, offset: UtcOffset) -> String {
    let day = civil(ts, offset);
    let now = now_ts();
    if day == civil(now, offset) {
        return "today".into();
    }
    if day == civil(now.saturating_sub(86_400), offset) {
        return "yesterday".into();
    }
    format!("{:04}-{:02}-{:02}", day.0, day.1, day.2)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    match app.screen.clone() {
        Screen::Home => draw_home(f, chunks[1], app),
        Screen::Unlock { alias, .. } => draw_unlock(f, chunks[1], &alias),
        Screen::Chat => draw_chat(f, chunks[1], app),
        Screen::Help => draw_help(f, chunks[1]),
        Screen::Confirm { alias, .. } => draw_confirm(f, chunks[1], &alias),
        Screen::Upgrade {
            version, required, ..
        } => draw_upgrade(f, chunks[1], &version, required),
    }
    draw_status(f, chunks[2], app);
    draw_input(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let inst = if app.dir.instance > 1 {
        format!("  #{}", app.dir.instance)
    } else {
        String::new()
    };
    let version = env!("CARGO_PKG_VERSION");
    let title = match &app.screen {
        Screen::Home => format!("  local-llm  {version}{inst}"),
        Screen::Unlock { alias, .. } => format!("  local-llm  {alias}  locked{inst}"),
        Screen::Chat if app.masked => format!(
            "  local-llm  {}  ctx {}/4  8192 tok{inst}",
            app.alias,
            app.live_now().len() + 1
        ),
        Screen::Chat => format!(
            "  local-llm  {}  {}  {} online{inst}",
            app.alias,
            app.nick,
            app.live_now().len()
        ),
        Screen::Help => format!("  local-llm  {version}  keys and commands{inst}"),
        Screen::Confirm { alias, .. } => format!("  local-llm  delete {alias}{inst}"),
        Screen::Upgrade { version: to, .. } => {
            format!("  local-llm  {version} -> {to}{inst}")
        }
    };
    let p = Paragraph::new(title).style(
        Style::default()
            .fg(Color::Rgb(180, 200, 170))
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(p, area);
}

fn draw_home(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled("  sessions", dim()))];
    if app.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  nothing here yet.  /new gpt-oss-20b  creates one and prints its key.",
            dim(),
        )));
        lines.push(Line::from(Span::styled(
            "  someone gave you a key?  /join 7K2M-9QXP",
            dim(),
        )));
    }
    for (i, row) in app.sessions.iter().enumerate() {
        let picked = i == app.selected;
        let marker = if picked { ">" } else { " " };
        let state = if row.remembered { "ready" } else { "locked" };
        let style = if picked {
            Style::default().fg(Color::Rgb(200, 220, 190))
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker} {:<24}", row.alias), style),
            Span::styled(state.to_string(), dim()),
        ]));
    }
    if !app.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  enter opens the highlighted one    del deletes it    ready = key saved here",
            dim(),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Two columns, because the list outgrew a single screen and help that needs
/// scrolling is help nobody reads.
fn draw_help(f: &mut Frame, area: Rect) {
    const COMMANDS: &[(&str, &str)] = &[
        ("/new <name>", "create a session"),
        ("/join <key>", "join with a key"),
        ("/pin", "show + copy the key"),
        ("/ticket", "copy your address"),
        ("/peers", "who is here now"),
        ("/w <name> <text>", "private message"),
        ("/nick <name>", "change your name"),
        ("/notify", "bell settings"),
        ("/paste", "on or off"),
        ("/img [path]", "send a picture"),
        ("/update", "check for a new build"),
        ("/leave", "back to the list"),
        ("/lock", "forget the key here"),
        ("/forget", "delete it here"),
        ("/diag", "why nobody shows up"),
        ("/quit", "exit"),
    ];
    const KEYS: &[(&str, &str)] = &[
        ("f1", "this screen"),
        ("f12", "hide names"),
        ("alt+up / alt+down", "pick a message"),
        ("ctrl+r", "answer it"),
        ("ctrl+y", "copy it"),
        ("ctrl+h", "blur it here"),
        ("ctrl+g", "open a picture"),
        ("ctrl+shift+v", "paste a picture"),
        ("pgup / pgdn", "scroll (wheel too)"),
        ("ctrl+end", "jump to the newest"),
        ("up / down", "reuse what you typed"),
        ("shift+enter", "newline"),
        ("tab", "complete a name"),
        ("del", "on the list: delete"),
        ("esc", "clear the line"),
    ];

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_help_column(f, columns[0], "commands", COMMANDS);
    draw_help_column(f, columns[1], "keys", KEYS);
}

fn draw_help_column(f: &mut Frame, area: Rect, title: &str, rows: &[(&str, &str)]) {
    let mut lines = vec![
        Line::from(Span::styled(format!("  {title}"), dim())),
        Line::from(""),
    ];
    for (key, what) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<18}"),
                Style::default().fg(Color::Rgb(200, 220, 190)),
            ),
            Span::styled((*what).to_string(), dim()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_upgrade(f: &mut Frame, area: Rect, version: &str, required: bool) {
    let running = env!("CARGO_PKG_VERSION");
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  version {version} is out — you are on {running}"),
            Style::default()
                .fg(Color::Rgb(190, 210, 230))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if required {
        lines.push(Line::from(Span::styled(
            "  this one is not optional: your build can no longer reach the others.",
            Style::default().fg(Color::Rgb(230, 190, 120)),
        )));
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(Span::styled(
            "  it is downloaded here, checked against its signature, and only then",
            dim(),
        )),
        Line::from(Span::styled(
            "  put in place. the app restarts and comes back to this room.",
            dim(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  enter updates now      any other key leaves it for later",
            Style::default().fg(Color::Rgb(200, 220, 190)),
        )),
    ]);
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_confirm(f: &mut Frame, area: Rect, alias: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  delete {alias} from this pc?"),
            Style::default()
                .fg(Color::Rgb(230, 170, 150))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  the whole history on this machine goes away. there is no undo.",
            dim(),
        )),
        Line::from(Span::styled(
            "  whoever else has the session keeps their copy, and the key still works.",
            dim(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  enter deletes      any other key cancels",
            Style::default().fg(Color::Rgb(200, 220, 190)),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_unlock(f: &mut Frame, area: Rect, alias: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  unlock {alias}"),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled("  enter the key  (7K2M-9QXP)", dim())),
        Line::from(""),
        Line::from(Span::styled("  esc goes back", dim())),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_chat(f: &mut Frame, area: Rect, app: &mut App) {
    app.chat_area = area;
    let key = RenderKey {
        len: app.feed.len(),
        width: area.width,
        masked: app.masked,
        hover: app.hover,
        picked: app.picked,
        replying: app.replying,
        hidden_rev: app.hidden_rev,
        expanded_rev: app.expanded_rev,
    };
    if app.rendered.as_ref().map(|r| r.key) != Some(key) {
        let layout = build_lines(app, area.width);
        app.rendered = Some(Rendered { key, layout });
    }

    let viewport = area.height as usize;
    let total = app.rendered.as_ref().map_or(0, |r| r.layout.lines.len());
    let max_scroll = total.saturating_sub(viewport) as u16;
    app.max_scroll = max_scroll;
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow = true;
            app.unread = 0;
        }
    }

    // Walking the selection with the keyboard has to bring it into view, or
    // it moves somewhere you cannot see.
    if let Some(idx) = app.picked {
        let at = app
            .rendered
            .as_ref()
            .and_then(|r| r.layout.owners.iter().position(|o| *o == Some(idx)));
        if let Some(at) = at {
            let at = at as u16;
            if at < app.scroll {
                app.scroll = at;
                app.follow = false;
            } else if at >= app.scroll + area.height {
                app.scroll = at.saturating_sub(area.height.saturating_sub(1));
                app.follow = false;
            }
        }
    }

    // Only the visible slice is painted, so how far back the history goes stops
    // mattering per frame - which it now does, since pointer movement redraws.
    let scroll = app.scroll as usize;
    let Some(rendered) = app.rendered.as_ref() else {
        return;
    };
    let slots = rendered.layout.images.clone();
    let mut painted: Vec<[u8; 32]> = Vec::new();
    let buf = f.buffer_mut();
    for (row, line) in rendered
        .layout
        .lines
        .iter()
        .skip(scroll)
        .take(viewport)
        .enumerate()
    {
        buf.set_line(area.x, area.y + row as u16, line, area.width);
    }

    // Pixels go on last, over the blank lines `build_lines` reserved for them.
    // A picture cannot live in the cell grid the way a glyph does, so it is
    // painted rather than laid out -- but the room it needs was already
    // accounted for, which is what keeps scrolling honest.
    for slot in slots {
        let Some(shot) = app.shots.get(&slot.blob) else {
            continue;
        };
        let Some(proto) = shot.frames.get(shot.at) else {
            continue;
        };
        let top = slot.first_line as isize - scroll as isize;
        // Entirely above or below what is on screen.
        if top >= viewport as isize || top + slot.rows as isize <= 0 {
            continue;
        }
        let position = SignedPosition {
            x: i16::try_from(slot.col).unwrap_or(0),
            y: i16::try_from(top).unwrap_or(i16::MIN),
        };
        SlicedImage::new(proto, position).render(area, buf);
        painted.push(slot.blob);
    }
    app.on_screen = painted;
}

/// True when `nick` appears in `text` as a whole word, with or without a
/// leading @. Substring matching would make "ana" fire on "banana".
fn mentions(text: &str, nick: &str) -> bool {
    if nick.is_empty() {
        return false;
    }
    let hay = text.to_lowercase();
    let needle = nick.to_lowercase();
    hay.match_indices(&needle).any(|(at, _)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
    })
}

/// "30m", "2h", "45s" into seconds. A bare number is rejected on purpose, so
/// `/notify 30` cannot silently mean half a minute.
fn parse_snooze(raw: &str) -> Option<u64> {
    let split = raw.find(|c: char| !c.is_ascii_digit())?;
    let (count, unit) = raw.split_at(split);
    let count: u64 = count.parse().ok()?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    Some(count * seconds)
}

fn describe_notify(settings: &Settings) -> String {
    let left = settings.snooze_until.saturating_sub(now_ts());
    if left > 0 {
        return format!("muted for another {} min", left.div_ceil(60));
    }
    match settings.notify {
        Notify::All => "beeps on every message".into(),
        Notify::Mention => "beeps only when your name comes up".into(),
        Notify::Off => "never beeps".into(),
    }
}

/// Sends a message only one person can read. Everyone still sees that a
/// whisper happened and to whom -- the protocol hides the words, not the fact.
async fn whisper_cmd(app: &mut App, line: &str) {
    let after = line
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim_start())
        .unwrap_or("");
    let Some((target, text)) = app.split_whisper(after) else {
        if after.trim().is_empty() {
            app.status = "usage: /w <name> <message>   (tab completes the name)".into();
        } else {
            app.status = format!("nobody here answers to that: {after}");
        }
        return;
    };
    let text = text.to_string();
    // `/w <name>` with nothing after it just points the prompt at them. That
    // is the whole feature: you say who once, not once per sentence.
    if text.is_empty() {
        let who = app
            .names
            .get(&target)
            .cloned()
            .unwrap_or_else(|| "them".into());
        app.whispering = Some(target);
        app.status = format!("whispering to {who} — esc goes back to the room");
        return;
    }
    send_whisper(app, target, text).await;
}

/// Sends one whisper and leaves the prompt pointed at that person.
async fn send_whisper(app: &mut App, target: [u8; 32], text: String) {
    let Some(room) = app.room.clone() else { return };
    // A quoted whisper can only travel back to the person it was with. Sent
    // to anyone else it is the same trap as answering one out loud: they
    // cannot open the quoted whisper, so the context exists only on our
    // screen. Dropped rather than refused -- the message itself is fine, and
    // the sender is told so they can paste what matters by hand.
    let mut reply = app.replying;
    let mut dropped = None;
    if let Some(other) = whisper_being_answered(app) {
        if other != target {
            dropped = Some(
                app.names
                    .get(&other)
                    .cloned()
                    .unwrap_or_else(|| "them".into()),
            );
            reply = None;
        }
    }
    let rec = {
        let mut room = room.lock().await;
        match room.compose_sealed(target, text.to_string(), reply) {
            Ok(rec) => rec,
            Err(e) => {
                app.status = format!("whisper not sent: {e}");
                return;
            }
        }
    };
    // Every other way of sending clears this. Without it the reply stayed
    // armed after a whisper and quietly attached itself to the next ordinary
    // message instead.
    app.replying = None;
    app.picked = None;
    // Stay pointed at them, so the next line does not need the command again.
    app.whispering = Some(target);
    app.follow = true;
    app.unread = 0;
    sync_feed(app).await;
    // The dropped quote is worth saying however the send went: it changed
    // what the other side will see.
    let note = dropped
        .map(|who| format!(" — quote dropped, only {who} can read that whisper"))
        .unwrap_or_default();
    match &app.net {
        Some(net) => {
            if let Err(e) = net.broadcast(&rec).await {
                app.status = format!("whisper saved but not delivered: {e}{note}");
            } else if !note.is_empty() {
                app.status = format!("sent{note}");
            }
        }
        None => app.status = format!("offline - the whisper is saved, not sent{note}"),
    }
}

/// Escape hatch for the paste heuristic. With it off, Enter always sends and
/// a pasted block goes back to arriving one message per line -- which some
/// people will prefer to any amount of guessing.
fn paste_cmd(app: &mut App, arg: &str) -> Result<()> {
    match arg.trim().to_lowercase().as_str() {
        "" => {}
        "on" => app.settings.paste_detect = true,
        "off" => app.settings.paste_detect = false,
        _ => {
            app.status = "usage: /paste on | off".into();
            return Ok(());
        }
    }
    if !arg.trim().is_empty() {
        app.dir.save_settings(&app.settings)?;
    }
    app.notice(if app.settings.paste_detect {
        "pasted line breaks stay inside one message. shift+enter also breaks a line."
    } else {
        "enter always sends. a pasted block arrives one message per line."
    });
    Ok(())
}

fn notify_cmd(app: &mut App, arg: &str) -> Result<()> {
    let arg = arg.trim().to_lowercase();
    if arg.is_empty() {
        let state = describe_notify(&app.settings);
        app.notice(format!("{state}   —   /notify all | mention | off | 30m"));
        return Ok(());
    }
    match arg.as_str() {
        "all" | "on" => {
            app.settings.notify = Notify::All;
            app.settings.snooze_until = 0;
        }
        "mention" | "mentions" => {
            app.settings.notify = Notify::Mention;
            app.settings.snooze_until = 0;
        }
        "off" | "mute" | "none" => {
            app.settings.notify = Notify::Off;
            app.settings.snooze_until = 0;
        }
        other => match parse_snooze(other) {
            Some(seconds) => app.settings.snooze_until = now_ts() + seconds,
            None => {
                app.status = "usage: /notify all | mention | off | 30m".into();
                return Ok(());
            }
        },
    }
    app.dir.save_settings(&app.settings)?;
    let state = describe_notify(&app.settings);
    app.notice(format!("notifications: {state}"));
    Ok(())
}

/// Spaces needed to push `len` columns of text so it ends at column `edge`.
fn indent(edge: usize, len: usize) -> String {
    " ".repeat(edge.saturating_sub(len))
}

/// A laid-out transcript: the lines to paint plus, for each line, which feed
/// item produced it. That owner map is what lets the mouse and the keyboard
/// point at a *message* instead of at a row of text.
struct Rendered {
    key: RenderKey,
    layout: Transcript,
}

/// Where the clickable icons ended up on screen, so a mouse press can be told
/// apart from a press anywhere else on the message.
#[derive(Clone)]
struct ActionAnchor {
    line: usize,
    reply: std::ops::Range<u16>,
    copy: std::ops::Range<u16>,
    hide: std::ops::Range<u16>,
}

struct Transcript {
    lines: Vec<Line<'static>>,
    owners: Vec<Option<usize>>,
    actions: Option<ActionAnchor>,
    /// Where an opened picture reserved room for itself. The lines are left
    /// blank and the pixels are painted over them after the text, because a
    /// picture does not live in the cell grid the way a glyph does.
    images: Vec<ImageSlot>,
    /// Lines that open or close a picture when clicked, and which message
    /// each belongs to.
    toggles: Vec<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct ImageSlot {
    /// First reserved line, as an index into `lines`.
    first_line: usize,
    /// Which blob to paint there.
    blob: [u8; 32],
    /// How many lines were reserved.
    rows: u16,
    /// Left edge, relative to the transcript area. Mirrors the bubble, so a
    /// picture you sent sits on the right like your text does.
    col: u16,
}

/// Everything that changes the layout. While this holds still the previous
/// layout is reused, which is what keeps pointer movement cheap.
#[derive(Clone, Copy, PartialEq)]
struct RenderKey {
    len: usize,
    width: u16,
    masked: bool,
    hover: Option<usize>,
    picked: Option<usize>,
    replying: Option<([u8; 32], u64)>,
    hidden_rev: u64,
    expanded_rev: u64,
}

/// Collects lines together with their owning feed item, so the two can never
/// drift apart.
struct Sink {
    lines: Vec<Line<'static>>,
    owners: Vec<Option<usize>>,
    actions: Option<ActionAnchor>,
    images: Vec<ImageSlot>,
    toggles: Vec<(usize, usize)>,
}

impl Sink {
    fn push(&mut self, line: Line<'static>, owner: Option<usize>) {
        self.lines.push(line);
        self.owners.push(owner);
    }

    fn blank(&mut self, owner: Option<usize>) {
        self.push(Line::from(""), owner);
    }

    fn arm(&mut self, anchor: ActionAnchor) {
        self.actions = Some(anchor);
    }
}

/// Ceiling on how many frames of one gif we decode. A long animation is a
/// list of full-size images: cheap on disk, very much not in memory.
const MAX_GIF_FRAMES: usize = 120;

/// Tallest a picture may get. A screenshot that fills the window buries the
/// conversation it belongs to, and scrolling past it becomes the main activity.
const IMAGE_MAX_ROWS: u16 = 20;
/// Assumed cell shape. Terminals are roughly twice as tall as they are wide
/// per cell; being a little off makes a picture slightly squat, not broken.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

/// How many cells a `w`x`h` picture wants, kept inside the given bounds and
/// keeping its shape.
fn fit_cells(w: u32, h: u32, max_cols: u16, max_rows: u16) -> (u16, u16) {
    if w == 0 || h == 0 || max_cols == 0 || max_rows == 0 {
        return (0, 0);
    }
    // Never blow a small picture up: a 48x48 sticker stretched across the
    // window is worse than a small one.
    let natural_cols = (w / CELL_W).max(1) as u16;
    let mut cols = natural_cols.min(max_cols);
    let mut rows = cells_tall(w, h, cols);
    if rows > max_rows {
        // Too tall: fix the height and work the width back out.
        rows = max_rows;
        cols = ((u32::from(rows) * CELL_H * w) / (h * CELL_W)).max(1) as u16;
        cols = cols.min(max_cols);
    }
    (cols.max(1), rows.max(1))
}

fn cells_tall(w: u32, h: u32, cols: u16) -> u16 {
    ((u32::from(cols) * CELL_W * h) / (w * CELL_H)).max(1) as u16
}

fn human_bytes(n: u32) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{:.1} MB", n as f32 / (1024.0 * 1024.0))
    }
}

/// What a picture turns into on screen this frame.
enum Framed {
    /// A single line of text standing in for the picture.
    Line(String),
    /// Room reserved for pixels, painted after the text.
    Open { cols: u16, rows: u16 },
}

/// Decides how a picture shows up, which is where the disguise is enforced:
/// under `F12` a picture can only ever be a line of text, no matter what the
/// user opened before pressing it.
fn frame_image(app: &App, img: &ImageRef, key: ([u8; 32], u64)) -> Framed {
    let size = format!("{}x{}", img.w, img.h);
    if app.masked {
        // Reads as a multimodal prompt rather than as a chat attachment.
        return Framed::Line(format!("image input  {size}"));
    }
    if app.hidden.contains(&key) {
        return Framed::Line(format!("image (+)  {size}  hidden"));
    }
    if !app.expanded.contains(&key) {
        return Framed::Line(format!(
            "image (+)  {size}  {}  {}",
            human_bytes(img.bytes),
            img.kind.label()
        ));
    }
    match app.shots.get(&img.blob) {
        Some(shot) => Framed::Open {
            cols: shot.for_area.width,
            rows: shot.rows(),
        },
        // Opened, but the pixels are not here. Says so instead of leaving a
        // hole where a picture should be.
        None => Framed::Line(format!("image (-)  {size}  waiting for pixels…")),
    }
}

const REPLY_ICON: &str = "↩";
const COPY_ICON: &str = "⧉";
const HIDE_ICON: &str = "▨";
const QUOTE_MARK: &str = "┌";
const WHISPER_MARK: &str = "→";
const BLOCK: char = '█';
/// Below this the ruler drops its words and shows bare icons, so it never
/// pushes a message off the edge on a narrow terminal.
const ROOM_FOR_WORDS: usize = 70;

fn action_labels(hidden: bool, compact: bool) -> [&'static str; 3] {
    if compact {
        [REPLY_ICON, COPY_ICON, HIDE_ICON]
    } else if hidden {
        ["↩ reply", "⧉ copy", "▨ show"]
    } else {
        ["↩ reply", "⧉ copy", "▨ hide"]
    }
}

fn ruler_width(hidden: bool, compact: bool) -> usize {
    action_labels(hidden, compact)
        .iter()
        .map(|label| label.chars().count())
        .sum::<usize>()
        + 4
}

/// Builds the hover ruler and records the columns each icon landed on, so a
/// click can be attributed to the right action.
fn action_ruler(hidden: bool, compact: bool, at: u16, line: usize) -> (Vec<Span<'static>>, ActionAnchor) {
    let tint = Style::default().fg(Color::Rgb(150, 170, 200));
    let mut spans = Vec::new();
    let mut spans_at = Vec::new();
    let mut col = at;
    for (index, label) in action_labels(hidden, compact).iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
            col += 2;
        }
        let width = label.chars().count() as u16;
        spans_at.push(col..col + width);
        spans.push(Span::styled(label.to_string(), tint));
        col += width;
    }
    let anchor = ActionAnchor {
        line,
        reply: spans_at[0].clone(),
        copy: spans_at[1].clone(),
        hide: spans_at[2].clone(),
    };
    (spans, anchor)
}

/// Replaces every character with a block, keeping the shape so you can tell a
/// message is there. This is what gets rendered -- the original text never
/// reaches the screen buffer.
fn blur(body: &str) -> String {
    body.chars()
        .map(|c| if c == '\n' { '\n' } else { BLOCK })
        .collect()
}

/// First line of a body, cut to fit a quote.
fn one_line(body: &str, width: usize) -> String {
    let first = body.lines().next().unwrap_or("");
    if first.chars().count() <= width {
        return first.to_string();
    }
    let mut cut: String = first.chars().take(width.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

fn build_lines(app: &App, width: u16) -> Transcript {
    let inner = (width as usize).saturating_sub(4);
    // Column the right-aligned text ends on, and how wide a message may get
    // before it wraps. Narrower than the full width so the two sides read as
    // two columns instead of one ragged block.
    let edge = (width as usize).saturating_sub(2);
    let bubble = if inner <= 32 { inner } else { (inner * 7) / 10 };
    let compact = inner < ROOM_FOR_WORDS;
    let mut sink = Sink {
        lines: Vec::new(),
        owners: Vec::new(),
        actions: None,
        images: Vec::new(),
        toggles: Vec::new(),
    };
    let mut last_day: Option<(i32, u8, u8)> = None;
    // A year-old room must not make every keystroke reformat tens of thousands
    // of lines.
    let skipped = app.feed.len().saturating_sub(RENDER_CAP);
    if skipped > 0 && !app.masked {
        sink.push(
            Line::from(Span::styled(
                format!("  ── {skipped} older entries kept in the log, not shown"),
                dim(),
            )),
            None,
        );
        sink.blank(None);
    }
    for (offset, item) in app.feed.iter().skip(skipped).enumerate() {
        let idx = skipped + offset;
        match item {
            Feed::Notice { body } | Feed::System { body } => {
                // The disguise hides anything that talks about the network,
                // keys or people joining.
                if app.masked {
                    continue;
                }
                for piece in wrap_text(body, inner) {
                    sink.push(Line::from(Span::styled(format!("  {piece}"), dim())), None);
                }
                sink.blank(None);
            }
            Feed::Msg {
                author,
                seq,
                name,
                mine,
                body,
                ts,
                reply_to,
                whisper,
                image,
            } => {
                // A whisper is the most sensitive thing on screen, so the
                // disguise drops it entirely rather than relabelling it.
                if whisper.is_some() && app.masked {
                    continue;
                }
                let day = civil(*ts, app.offset);
                if last_day != Some(day) && !app.masked {
                    sink.push(
                        Line::from(Span::styled(
                            format!("  ── {}", day_label(*ts, app.offset)),
                            dim(),
                        )),
                        None,
                    );
                    sink.blank(None);
                }
                last_day = Some(day);
                // The quoted line only makes sense once the answered message
                // has arrived; until then say so rather than showing nothing.
                // Quotes carry a real name, and an inference log has no such
                // thing anyway, so the disguise drops them.
                let quote = reply_to.filter(|_| !app.masked).map(|key| match app.by_key.get(&key) {
                    Some(at) => match app.feed.get(*at) {
                        Some(Feed::Msg { name, body, .. }) => {
                            // A quote must obey the blur too, or answering a
                            // hidden message would put it right back on screen.
                            let text = if app.hidden.contains(&key) {
                                blur(body)
                            } else {
                                body.clone()
                            };
                            format!("{QUOTE_MARK} {name}: {}", one_line(&text, bubble / 2))
                        }
                        _ => format!("{QUOTE_MARK} (message not here yet)"),
                    },
                    None => format!("{QUOTE_MARK} (message not here yet)"),
                });
                let armed = !app.masked && (app.hover == Some(idx) || app.picked == Some(idx));
                let hidden_here = app.hidden.contains(&(*author, *seq));
                // The blur is built here, so the real text never reaches the
                // screen buffer at all.
                let shown = if hidden_here {
                    blur(body)
                } else {
                    body.clone()
                };
                let label = if app.masked {
                    if *mine {
                        "user".to_string()
                    } else {
                        role_for(author).to_string()
                    }
                } else {
                    match whisper {
                        Some(them) if *mine => {
                            let who = app
                                .names
                                .get(them)
                                .cloned()
                                .unwrap_or_else(|| "them".into());
                            format!("{name} {WHISPER_MARK} {who}")
                        }
                        Some(_) => format!("{name} {WHISPER_MARK} you"),
                        None => name.clone(),
                    }
                };
                // Everyone gets their own colour, derived from their id so all
                // machines agree without anyone configuring anything. The
                // disguise falls back to two neutral tones, because four
                // distinct colours read as a conversation from across the room.
                let head_style = Style::default()
                    .fg(if app.masked {
                        if *mine {
                            Color::Rgb(160, 190, 220)
                        } else {
                            Color::Rgb(180, 210, 170)
                        }
                    } else {
                        let (r, g, b) = color_for(author);
                        Color::Rgb(r, g, b)
                    })
                    .add_modifier(Modifier::BOLD);
                let body_style = if hidden_here {
                    dim()
                } else {
                    Style::default().fg(Color::Rgb(220, 220, 220))
                };

                // Your own messages hug the right edge, the way every chat does
                // it. The disguise turns that off: an inference log is flush
                // left, and staggered text would give it away at a glance.
                if *mine && !app.masked {
                    if let Some(quote) = &quote {
                        let pad = indent(edge, quote.chars().count());
                        sink.push(Line::from(Span::styled(pad + quote, dim())), Some(idx));
                    }
                    let stamp = clock(*ts, app.offset);
                    let mut head_width = stamp.chars().count() + 2 + label.chars().count();
                    // Icons sit to the left of a right-aligned header, so they
                    // widen the block instead of pushing it off the edge.
                    let icons = if armed {
                        ruler_width(hidden_here, compact) + 3
                    } else {
                        0
                    };
                    head_width += icons;
                    let lead = indent(edge, head_width);
                    let col = lead.chars().count() as u16;
                    let mut head: Vec<Span> = vec![Span::raw(lead)];
                    if armed {
                        let (spans, anchor) =
                            action_ruler(hidden_here, compact, col, sink.lines.len());
                        head.extend(spans);
                        head.push(Span::raw("   "));
                        sink.arm(anchor);
                    }
                    head.push(Span::styled(stamp, dim()));
                    head.push(Span::styled(format!("  {label}"), head_style));
                    sink.push(Line::from(head), Some(idx));
                    for piece in wrap_text(&shown, bubble) {
                        let pad = indent(edge, piece.chars().count());
                        sink.push(
                            Line::from(Span::styled(pad + &piece, body_style)),
                            Some(idx),
                        );
                    }
                    if let Some(img) = image {
                        match frame_image(app, img, (*author, *seq)) {
                            Framed::Line(text) => {
                                let pad = indent(edge, text.chars().count());
                                if !app.masked {
                                    sink.toggles.push((sink.lines.len(), idx));
                                }
                                sink.push(
                                    Line::from(Span::styled(pad + &text, dim())),
                                    Some(idx),
                                );
                            }
                            Framed::Open { cols, rows } => {
                                let shut = format!("image (-)  {}x{}", img.w, img.h);
                                let pad = indent(edge, shut.chars().count());
                                sink.toggles.push((sink.lines.len(), idx));
                                sink.push(Line::from(Span::styled(pad + &shut, dim())), Some(idx));
                                let first_line = sink.lines.len();
                                for _ in 0..rows {
                                    sink.blank(Some(idx));
                                }
                                sink.images.push(ImageSlot {
                                    first_line,
                                    blob: img.blob,
                                    rows,
                                    // Hugs the right edge, like the text above it.
                                    col: (edge as u16).saturating_sub(cols),
                                });
                            }
                        }
                    }
                } else {
                    if let Some(quote) = &quote {
                        sink.push(
                            Line::from(Span::styled(format!("  {quote}"), dim())),
                            Some(idx),
                        );
                    }
                    let mut head = vec![Span::styled(format!("  {label}"), head_style)];
                    let mut col = 2 + label.chars().count() as u16;
                    if !app.masked {
                        let stamp = clock(*ts, app.offset);
                        head.push(Span::styled(format!("  {stamp}"), dim()));
                        col += 2 + stamp.chars().count() as u16;
                    }
                    if armed {
                        head.push(Span::raw("   "));
                        col += 3;
                        let (spans, anchor) =
                            action_ruler(hidden_here, compact, col, sink.lines.len());
                        head.extend(spans);
                        sink.arm(anchor);
                    }
                    sink.push(Line::from(head), Some(idx));
                    for piece in wrap_text(&shown, if app.masked { inner } else { bubble }) {
                        sink.push(
                            Line::from(Span::styled(format!("  {piece}"), body_style)),
                            Some(idx),
                        );
                    }
                    if let Some(img) = image {
                        match frame_image(app, img, (*author, *seq)) {
                            Framed::Line(text) => {
                                if !app.masked {
                                    sink.toggles.push((sink.lines.len(), idx));
                                }
                                sink.push(
                                    Line::from(Span::styled(format!("  {text}"), dim())),
                                    Some(idx),
                                );
                            }
                            // Left edge, so the width does not come into it.
                            Framed::Open { cols: _, rows } => {
                                let shut = format!("image (-)  {}x{}", img.w, img.h);
                                sink.toggles.push((sink.lines.len(), idx));
                                sink.push(
                                    Line::from(Span::styled(format!("  {shut}"), dim())),
                                    Some(idx),
                                );
                                let first_line = sink.lines.len();
                                for _ in 0..rows {
                                    sink.blank(Some(idx));
                                }
                                sink.images.push(ImageSlot {
                                    first_line,
                                    blob: img.blob,
                                    rows,
                                    col: 2,
                                });
                            }
                        }
                    }
                }
                // The trailing blank belongs to the message above it, so the
                // pointer does not flicker in the gap between messages.
                sink.blank(Some(idx));
            }
        }
    }
    Transcript {
        lines: sink.lines,
        owners: sink.owners,
        actions: sink.actions,
        images: sink.images,
        toggles: sink.toggles,
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let text = if app.masked {
        MASKED_STATUS.to_string()
    } else if matches!(app.screen, Screen::Help) {
        "any key closes this".to_string()
    } else if let Some(key) = app.replying {
        let who = app
            .by_key
            .get(&key)
            .and_then(|at| app.feed.get(*at))
            .and_then(|item| match item {
                Feed::Msg { name, .. } => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "someone".into());
        format!("replying to {who}   esc cancels")
    } else if app.unread > 0 {
        format!("{}   ▼ {} new below   ctrl+end", app.status, app.unread)
    } else if matches!(app.screen, Screen::Chat) && !app.follow {
        // Without this you cannot tell a scrolled view from a stuck one.
        format!(
            "scrolled up — {} line(s) below   ctrl+end returns",
            app.max_scroll.saturating_sub(app.scroll)
        )
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(format!("  {text}")).style(dim()),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    // Overlays answer keys, they do not take text.
    if matches!(app.screen, Screen::Help | Screen::Confirm { .. }) {
        f.render_widget(
            Paragraph::new("").block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::Rgb(50, 55, 50))),
            ),
            area,
        );
        return;
    }
    let hide = matches!(app.screen, Screen::Unlock { .. });
    let full: String = if app.masked {
        String::new()
    } else if hide {
        "•".repeat(app.input.len())
    } else {
        app.input.text.replace('\n', "↵")
    };
    let cursor = if app.masked { 0 } else { app.input.cursor };
    // Whispering changes where every keystroke goes, so it changes the
    // prompt itself rather than announcing it somewhere off to the side. The
    // disguise drops it with the draft: a name at the prompt is a real name.
    let lead = match app.whispering.filter(|_| !app.masked && !hide) {
        Some(them) => {
            let who = app
                .names
                .get(&them)
                .cloned()
                .unwrap_or_else(|| "them".into());
            format!("  {who} {WHISPER_MARK} ")
        }
        None => "  > ".to_string(),
    };
    let room = (area.width as usize)
        .saturating_sub(lead.chars().count() + 2)
        .max(8);
    let start = cursor.saturating_sub(room);
    let shown: String = full.chars().skip(start).take(room).collect();
    let quiet = app.whispering.is_some() && !app.masked && !hide;
    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            lead.clone(),
            if quiet {
                Style::default()
                    .fg(Color::Rgb(230, 190, 120))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(200, 200, 190))
            },
        ),
        Span::styled(shown, Style::default().fg(Color::Rgb(200, 200, 190))),
    ]))
    .block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(if quiet {
                Color::Rgb(150, 120, 60)
            } else {
                Color::Rgb(50, 55, 50)
            })),
    );
    f.render_widget(p, area);
    let col = (lead.chars().count() as u16).saturating_add((cursor - start) as u16);
    f.set_cursor_position((area.x.saturating_add(col), area.y.saturating_add(1)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::TempDir;

    /// The data dir is chosen by process-wide env vars and guarded by a port
    /// slot, so these tests cannot overlap.
    fn serialize() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn isolate(tmp: &TempDir) {
        std::env::set_var("LOCAL_LLM_HOME", tmp.path());
        std::env::set_var("LOCAL_LLM_OFFLINE", "1");
    }

    struct Harness {
        app: App,
        term: Terminal<TestBackend>,
        _tmp: TempDir,
        _guard: MutexGuard<'static, ()>,
    }

    impl Harness {
        fn new() -> Self {
            let guard = serialize();
            let tmp = TempDir::new().unwrap();
            isolate(&tmp);
            Self {
                app: App::new().unwrap(),
                term: Terminal::new(TestBackend::new(80, 24)).unwrap(),
                _tmp: tmp,
                _guard: guard,
            }
        }

        async fn cmd(&mut self, line: &str) {
            handle_command(&mut self.app, line.to_string(), &mut self.term)
                .await
                .unwrap();
        }

        /// A keystroke the terminal delivered as part of a burst, which is
        /// what a paste looks like on Windows.
        async fn paste_key(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_key(&mut self.app, key, &mut self.term, true)
                .await
                .unwrap();
        }

        async fn press(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_key(&mut self.app, key, &mut self.term, false)
                .await
                .unwrap();
        }

        fn transcript(&self) -> String {
            self.app
                .feed
                .iter()
                .map(|item| match item {
                    Feed::Notice { body } | Feed::System { body } => body.clone(),
                    Feed::Msg { name, body, .. } => format!("{name}: {body}"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The painted buffer, one string per row.
        fn painted_lines(&mut self) -> Vec<String> {
            let app = &mut self.app;
            self.term.draw(|f| draw(f, app)).unwrap();
            let buf = self.term.backend().buffer();
            let w = buf.area.width as usize;
            buf.content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect::<Vec<_>>()
                .chunks(w)
                .map(|row| row.join("").trim_end().to_string())
                .collect()
        }

        fn painted(&mut self) -> String {
            let app = &mut self.app;
            self.term.draw(|f| draw(f, app)).unwrap();
            self.term
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }
    }

    #[tokio::test]
    async fn the_key_survives_later_network_chatter() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let key = h.app.room.as_ref().unwrap().lock().await.pin.display();
        assert!(h.transcript().contains(&key));

        // The old build printed the key into the one-line status bar, where
        // the first peer event two seconds later wiped it out.
        apply_net(&mut h.app, NetEvent::Status("peer up abcd1234".into())).await;
        apply_net(&mut h.app, NetEvent::Peers(Vec::new())).await;
        assert!(h.transcript().contains(&key));
        assert!(h.painted().contains(&key));
    }

    #[tokio::test]
    async fn deleting_a_session_stops_on_a_confirm_screen() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;

        h.cmd("/forget").await;
        assert!(matches!(h.app.screen, Screen::Confirm { .. }));
        assert!(h.app.room.is_some(), "nothing may be wiped before enter");
        assert!(h.painted().contains("no undo"));

        h.press(KeyCode::Char('x')).await;
        assert_eq!(h.app.dir.list_sessions().unwrap().len(), 1);
        assert!(h.app.room.is_some(), "any key but enter must cancel");

        h.cmd("/forget").await;
        h.press(KeyCode::Enter).await;
        assert!(h.app.room.is_none());
        assert!(h.app.dir.list_sessions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_key_on_the_session_list_asks_first() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.cmd("/leave").await;
        assert!(matches!(h.app.screen, Screen::Home));

        h.press(KeyCode::Delete).await;
        assert!(matches!(h.app.screen, Screen::Confirm { .. }));
        h.press(KeyCode::Enter).await;
        assert!(h.app.dir.list_sessions().unwrap().is_empty());
    }

    /// Appends somebody else's message the same way sync_feed would, index
    /// included -- without the index a quote cannot find what it answers.
    fn push_peer(app: &mut App, author: [u8; 32], seq: u64, name: &str, body: &str) {
        app.by_key.insert((author, seq), app.feed.len());
        app.feed.push(from_peer(author, seq, name, body));
    }

    /// A picture somebody sent, for tests that only care about layout. The
    /// blob is a name, not bytes -- nothing here needs to decode.
    fn push_picture(app: &mut App, author: [u8; 32], seq: u64, name: &str) -> ([u8; 32], u64) {
        app.by_key.insert((author, seq), app.feed.len());
        app.feed.push(Feed::Msg {
            author,
            seq,
            name: name.into(),
            mine: false,
            body: String::new(),
            ts: now_ts(),
            reply_to: None,
            whisper: None,
            image: Some(Box::new(ImageRef {
                blob: [0x5a; 32],
                w: 320,
                h: 240,
                kind: ImageKind::Png,
                bytes: 62_000,
            })),
        });
        (author, seq)
    }

    #[tokio::test]
    async fn a_picture_arrives_closed_and_only_opens_when_asked() {
        let mut h = Harness::new();
        h.cmd("/new sala").await;
        let key = push_picture(&mut h.app, [9u8; 32], 1, "Dale");

        let closed = h.painted();
        assert!(closed.contains("image (+)"), "should offer to open it");
        assert!(closed.contains("320x240"), "and say what it is");
        assert!(
            !closed.contains("image (-)"),
            "nothing should be open on arrival"
        );
        assert!(
            h.app.rendered.as_ref().unwrap().layout.images.is_empty(),
            "a closed picture must not reserve any room"
        );

        // Opening is what puts it on screen. There are no pixels behind this
        // blob, so it stops at "waiting" -- which is itself the behaviour we
        // want when the bytes have not caught up with the announcement.
        let last = h.app.feed.len() - 1;
        toggle_expanded(&mut h.app, last).await;
        assert!(h.app.expanded.contains(&key));
        assert!(
            h.painted().contains("waiting for pixels"),
            "an opened picture with no bytes has to say so, not leave a hole"
        );
    }

    #[tokio::test]
    async fn the_disguise_closes_every_picture() {
        let mut h = Harness::new();
        h.cmd("/new sala").await;
        let key = push_picture(&mut h.app, [9u8; 32], 1, "Dale");

        // Force it open, including the state that survives a redraw.
        h.app.expanded.insert(key);
        h.app.expanded_rev += 1;
        h.app.shots.insert(
            [0x5a; 32],
            Shot {
                frames: Vec::new(),
                delays: Vec::new(),
                at: 0,
                next: None,
                for_area: ratatui::layout::Size::new(20, 10),
            },
        );

        h.app.masked = true;
        let masked = h.painted();
        assert!(
            !masked.contains("image (-)"),
            "F12 has to close every picture, not just the ones nobody opened"
        );
        assert!(
            h.app.rendered.as_ref().unwrap().layout.images.is_empty(),
            "no room may be reserved for pixels while the disguise is on"
        );
        assert!(
            masked.contains("image input"),
            "it should read as a multimodal prompt, not as a chat attachment"
        );
        assert!(
            h.app.rendered.as_ref().unwrap().layout.toggles.is_empty(),
            "and there must be nothing to click that would reopen it"
        );

        // Turning the disguise off leaves the earlier choice intact.
        h.app.masked = false;
        assert!(h.app.expanded.contains(&key), "F12 hides, it does not forget");
    }

    /// A real shot with `n` encoded frames, already due to advance. Built
    /// through the same picker the app uses, so the test exercises the actual
    /// encode path rather than a stand-in.
    fn animated_shot(n: usize) -> Shot {
        let picker = picker_for(ImageProto::Halfblocks);
        let area = ratatui::layout::Size::new(8, 4);
        let frames: Vec<SlicedProtocol> = (0..n)
            .map(|i| {
                let mut img = image::RgbaImage::new(16, 16);
                for px in img.pixels_mut() {
                    *px = image::Rgba([(i * 40) as u8, 100, 150, 255]);
                }
                SlicedProtocol::new_with_resize(
                    &picker,
                    image::DynamicImage::ImageRgba8(img),
                    area,
                    ratatui_image::Resize::Fit(None),
                )
                .unwrap()
            })
            .collect();
        Shot {
            frames,
            delays: vec![40; n],
            at: 0,
            // Already overdue, so a single tick has to act.
            next: Some(Instant::now() - Duration::from_millis(1)),
            for_area: area,
        }
    }

    #[tokio::test]
    async fn an_animation_only_runs_while_it_can_be_seen() {
        let mut h = Harness::new();
        h.cmd("/new sala").await;
        let blob = [0x5a; 32];
        h.app.shots.insert(blob, animated_shot(3));

        // Off screen: a gif scrolled out of view must neither advance nor ask
        // to be woken, or it burns cpu for nobody.
        h.app.on_screen.clear();
        tick_animations(&mut h.app);
        assert_eq!(h.app.shots[&blob].at, 0, "must not advance unseen");
        assert!(next_frame_due(&h.app).is_none(), "must not schedule unseen");

        // On screen: it advances, wraps around, and keeps asking.
        h.app.on_screen = vec![blob];
        tick_animations(&mut h.app);
        assert_eq!(h.app.shots[&blob].at, 1, "a due frame should advance");
        assert!(next_frame_due(&h.app).is_some(), "and schedule the next");

        for expected in [2usize, 0, 1] {
            h.app.shots.get_mut(&blob).unwrap().next =
                Some(Instant::now() - Duration::from_millis(1));
            tick_animations(&mut h.app);
            assert_eq!(h.app.shots[&blob].at, expected, "should loop round");
        }

        // The disguise stops the clock, not just the drawing.
        h.app.masked = true;
        assert!(
            next_frame_due(&h.app).is_none(),
            "F12 must stop the clock, not merely hide the picture"
        );
        let before = h.app.shots[&blob].at;
        h.app.shots.get_mut(&blob).unwrap().next = Some(Instant::now() - Duration::from_millis(1));
        tick_animations(&mut h.app);
        assert_eq!(h.app.shots[&blob].at, before, "and must not advance either");
    }

    #[tokio::test]
    async fn a_late_frame_does_not_cascade_through_the_rest() {
        let mut h = Harness::new();
        h.cmd("/new sala").await;
        let blob = [0x5a; 32];
        let mut shot = animated_shot(5);
        // The loop was blocked for a second: four frames' worth of deadlines
        // went by while nothing ran.
        shot.next = Some(Instant::now() - Duration::from_secs(1));
        h.app.shots.insert(blob, shot);
        h.app.on_screen = vec![blob];

        tick_animations(&mut h.app);
        assert_eq!(
            h.app.shots[&blob].at, 1,
            "one tick advances exactly one frame, however late it was"
        );
        let next = h.app.shots[&blob].next.expect("should still be scheduled");
        assert!(
            next > Instant::now() + Duration::from_millis(25),
            "the next frame must be a full delay from now, not from the deadline it missed              -- otherwise a stalled loop replays the whole gif in one burst"
        );
    }

    /// A still picture has one frame and must never wake the loop at all.
    #[tokio::test]
    async fn a_still_picture_never_schedules_anything() {
        let mut h = Harness::new();
        h.cmd("/new sala").await;
        let blob = [0x5a; 32];
        h.app.shots.insert(blob, animated_shot(1));
        h.app.on_screen = vec![blob];

        assert!(!h.app.shots[&blob].animated());
        tick_animations(&mut h.app);
        assert_eq!(h.app.shots[&blob].at, 0);
        assert!(
            next_frame_due(&h.app).is_none(),
            "a still picture must leave the app idle"
        );
    }

    #[test]
    fn a_picture_keeps_its_shape_inside_the_space_it_gets() {
        // Wider than tall, plenty of room: bounded by width.
        let (cols, rows) = fit_cells(800, 400, 40, 20);
        assert!(cols <= 40 && rows <= 20, "{cols}x{rows}");
        assert!(rows >= 1);

        // Tall and narrow: the height limit is what binds, and the width has
        // to come down with it or the picture stretches.
        let (cols, rows) = fit_cells(400, 4000, 40, 20);
        assert_eq!(rows, 20);
        assert!(cols < 40, "a tall picture must not fill the width: {cols}");

        // A sticker is not blown up to fill the window.
        let (cols, _) = fit_cells(48, 48, 60, 20);
        assert!(cols <= 6, "small pictures stay small, got {cols}");

        // Degenerate input must not panic or divide by zero.
        assert_eq!(fit_cells(0, 0, 40, 20), (0, 0));
        assert_eq!(fit_cells(100, 100, 0, 20), (0, 0));
    }

    /// A message from somebody else, for tests that only care about layout.
    fn from_peer(author: [u8; 32], seq: u64, name: &str, body: &str) -> Feed {
        Feed::Msg {
            author,
            seq,
            name: name.into(),
            mine: false,
            body: body.into(),
            ts: now_ts(),
            reply_to: None,
            whisper: None,
            image: None,
        }
    }

    fn flatten(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    /// Colour of the bold author span on the line carrying `needle`.
    fn head_colour(lines: &[Line], needle: &str) -> Option<Color> {
        lines
            .iter()
            .find(|line| flatten(line).contains(needle))?
            .spans
            .iter()
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
    }

    #[test]
    fn a_mention_is_a_whole_word() {
        assert!(mentions("opa @Pedro, olha isso", "Pedro"));
        assert!(mentions("pedro viu?", "Pedro"), "case should not matter");
        assert!(mentions("fala ana", "Ana"));
        assert!(
            !mentions("comprei bananas hoje", "ana"),
            "substrings must not fire the bell"
        );
        assert!(!mentions("nada a ver", "Pedro"));
        assert!(!mentions("qualquer texto", ""));
    }

    #[test]
    fn a_snooze_needs_a_unit() {
        assert_eq!(parse_snooze("30m"), Some(1800));
        assert_eq!(parse_snooze("2h"), Some(7200));
        assert_eq!(parse_snooze("45s"), Some(45));
        // A bare number must not silently mean seconds or minutes.
        assert_eq!(parse_snooze("30"), None);
        assert_eq!(parse_snooze("m"), None);
        assert_eq!(parse_snooze("30d"), None);
    }

    #[tokio::test]
    async fn notification_modes_decide_when_the_bell_rings() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        assert!(
            h.app.wants_bell("qualquer coisa"),
            "every message by default"
        );

        h.cmd("/notify mention").await;
        assert!(!h.app.wants_bell("assunto que nao me cita"));
        assert!(h.app.wants_bell("@Pedro consegue olhar?"));

        h.cmd("/notify off").await;
        assert!(!h.app.wants_bell("@Pedro consegue olhar?"));

        h.cmd("/notify all").await;
        h.cmd("/notify 30m").await;
        assert!(h.app.settings.snooze_until > now_ts());
        assert!(!h.app.wants_bell("qualquer coisa"), "snoozed");

        // Preference survives a restart.
        assert_eq!(h.app.dir.load_settings().notify, Notify::All);
        assert!(h.app.dir.load_settings().snooze_until > now_ts());
    }

    fn mouse(app: &mut App, kind: crossterm::event::MouseEventKind, row: u16, column: u16) {
        handle_mouse(
            app,
            crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
        );
    }

    /// First screen row that carries a message.
    fn first_message_row(app: &App) -> u16 {
        let area = app.chat_area;
        (area.y..area.y + area.height)
            .find(|row| app.message_at(*row).is_some())
            .expect("a message should be on screen")
    }

    async fn record_count(app: &App) -> usize {
        app.room.as_ref().unwrap().lock().await.log.records().len()
    }

    #[tokio::test]
    async fn hiding_blurs_the_text_without_touching_the_log() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 3, "Dale", "coisa pesada demais");
        h.painted();

        let idx = h.app.feed.len() - 1;
        let before = record_count(&h.app).await;

        toggle_hidden(&mut h.app, idx).await;
        let painted = h.painted();
        assert!(
            !painted.contains("coisa pesada"),
            "hidden text must never reach the screen buffer"
        );
        assert!(painted.contains(BLOCK), "the shape of the message should stay");
        assert!(
            painted.contains("Dale"),
            "you should still see that Dale said something"
        );
        assert_eq!(
            record_count(&h.app).await,
            before,
            "hiding is a local preference, not an edit to the room"
        );

        // Toggling brings it back and leaves it back.
        toggle_hidden(&mut h.app, idx).await;
        assert!(h.painted().contains("coisa pesada"));
        assert!(h.painted().contains("coisa pesada"));
    }

    #[tokio::test]
    async fn hidden_messages_are_still_hidden_next_time() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 3, "Dale", "nao quero isso na tela");
        let last = h.app.feed.len() - 1;
        toggle_hidden(&mut h.app, last).await;
        let kept: Vec<_> = h.app.hidden.iter().copied().collect();
        assert_eq!(kept.len(), 1);

        // Reopening the room reloads the preference from the sealed side file.
        h.app.hidden.clear();
        load_hidden(&mut h.app).await;
        assert_eq!(h.app.hidden.iter().copied().collect::<Vec<_>>(), kept);
    }

    #[tokio::test]
    async fn the_hide_icon_is_clickable_and_says_what_it_will_do() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 3, "Dale", "segredo");
        h.painted();

        let row = first_message_row(&h.app);
        mouse(&mut h.app, MouseEventKind::Moved, row, 4);
        let shown = h.painted();
        assert!(shown.contains("hide"), "the ruler should offer hiding");

        let anchor = h
            .app
            .rendered
            .as_ref()
            .unwrap()
            .layout
            .actions
            .clone()
            .expect("ruler anchored");
        let icon_row = h.app.chat_area.y + (anchor.line as u16 - h.app.scroll);
        mouse(
            &mut h.app,
            MouseEventKind::Down(MouseButton::Left),
            icon_row,
            anchor.hide.start,
        );
        // The click only marks it; the loop performs the write.
        let idx = h.app.pending_hide.take().expect("click should ask to hide");
        toggle_hidden(&mut h.app, idx).await;

        assert!(!h.painted().contains("segredo"));
        // And the ruler now offers the way back.
        mouse(&mut h.app, MouseEventKind::Moved, row, 4);
        assert!(h.painted().contains("show"));
    }

    #[tokio::test]
    async fn the_roster_counts_the_room_not_the_overlay() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;

        // A gossip neighbour is topology; it says nothing about who is here.
        apply_net(
            &mut h.app,
            NetEvent::Peers(vec![iroh::SecretKey::generate().public()]),
        )
        .await;
        assert!(h.app.live_now().is_empty(), "a neighbour is not a person");
        assert!(h.painted().contains("0 online"));

        apply_net(
            &mut h.app,
            NetEvent::Live {
                author: [9u8; 32],
                name: "Dale".into(),
            },
        )
        .await;
        apply_net(
            &mut h.app,
            NetEvent::Live {
                author: [7u8; 32],
                name: "Ana".into(),
            },
        )
        .await;
        assert_eq!(h.app.live_now(), vec!["Ana", "Dale"]);
        assert!(h.painted().contains("2 online"));

        // Beating again is not arriving again.
        let notices = h.transcript().matches("Dale is here").count();
        apply_net(
            &mut h.app,
            NetEvent::Live {
                author: [9u8; 32],
                name: "Dale".into(),
            },
        )
        .await;
        assert_eq!(h.transcript().matches("Dale is here").count(), notices);

        // Going quiet drops them, once.
        h.app.present.get_mut(&[9u8; 32]).unwrap().at =
            Instant::now() - PRESENCE_TTL - Duration::from_secs(1);
        assert_eq!(h.app.live_now(), vec!["Ana"], "stale beats do not count");
        sweep_presence(&mut h.app);
        assert!(h.transcript().contains("Dale left"));
        sweep_presence(&mut h.app);
        assert_eq!(h.transcript().matches("Dale left").count(), 1);
    }

    #[test]
    fn typing_is_never_mistaken_for_a_paste() {
        let start = Instant::now();
        let mut burst = Burst::default();

        // Someone typing quickly: 40 ms between keys, well under a record
        // holder's pace and still nowhere near a paste.
        for step in 1..=20 {
            let typed = burst.observe(start + Duration::from_millis(step * 40));
            assert!(!typed, "keystroke {step} was taken for a paste");
        }

        // A paste: the terminal dumps everything at once.
        let dump = start + Duration::from_secs(1);
        assert!(!burst.observe(dump), "one close pair proves nothing");
        assert!(!burst.observe(dump + Duration::from_micros(200)));
        assert!(!burst.observe(dump + Duration::from_micros(400)));
        assert!(
            burst.observe(dump + Duration::from_micros(600)),
            "a run of instant events is a paste"
        );

        // And typing again right after settles it back down.
        assert!(!burst.observe(dump + Duration::from_millis(500)));
    }

    #[tokio::test]
    async fn a_pasted_block_arrives_as_one_message() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let before = h.app.feed.len();

        // What the console hands over when two lines are pasted: plain key
        // events, the newline among them indistinguishable from Enter except
        // for arriving in a burst.
        for ch in "linha um".chars() {
            h.paste_key(KeyCode::Char(ch)).await;
        }
        h.paste_key(KeyCode::Enter).await;
        for ch in "linha dois".chars() {
            h.paste_key(KeyCode::Char(ch)).await;
        }
        // The send is a real keypress, on its own.
        h.press(KeyCode::Enter).await;

        assert_eq!(
            h.app.feed.len() - before,
            1,
            "a paste must not be chopped into one message per line"
        );
        match h.app.feed.last().unwrap() {
            Feed::Msg { body, .. } => assert_eq!(body, "linha um\nlinha dois"),
            _ => panic!("expected a message"),
        }
    }

    #[tokio::test]
    async fn typing_enter_still_sends() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let before = h.app.feed.len();

        for ch in "primeira".chars() {
            h.press(KeyCode::Char(ch)).await;
        }
        h.press(KeyCode::Enter).await;
        for ch in "segunda".chars() {
            h.press(KeyCode::Char(ch)).await;
        }
        h.press(KeyCode::Enter).await;

        assert_eq!(
            h.app.feed.len() - before,
            2,
            "typed Enter must keep sending, burst detection is not a mode"
        );
    }

    #[tokio::test]
    async fn a_reply_carries_a_pointer_and_shows_a_quote() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "deploy quinta".into()).await;
        let answered = match h.app.feed.last().unwrap() {
            Feed::Msg { author, seq, .. } => (*author, *seq),
            _ => panic!("expected a message"),
        };

        h.app.replying = Some(answered);
        send(&mut h.app, "confirmo".into()).await;
        assert_eq!(h.app.replying, None, "sending must disarm the reply");

        match h.app.feed.last().unwrap() {
            Feed::Msg { reply_to, .. } => assert_eq!(*reply_to, Some(answered)),
            _ => panic!("expected a message"),
        }

        let quoted = build_lines(&h.app, 80)
            .lines
            .iter()
            .map(flatten)
            .any(|text| text.contains(QUOTE_MARK) && text.contains("deploy quinta"));
        assert!(quoted, "the answered message should be quoted above the reply");
    }

    #[tokio::test]
    async fn answering_something_we_never_received_says_so() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.app.replying = Some(([42u8; 32], 99));
        send(&mut h.app, "resposta orfa".into()).await;

        let text: String = build_lines(&h.app, 80).lines.iter().map(flatten).collect();
        assert!(
            text.contains("not here yet"),
            "a dangling quote must be admitted, not silently dropped"
        );
    }

    #[tokio::test]
    async fn the_keyboard_picks_a_message_and_answers_it() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "primeira".into()).await;
        send(&mut h.app, "segunda".into()).await;
        h.painted();

        // Nothing picked yet: the first step lands on the newest message.
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        handle_key(&mut h.app, alt_up, &mut h.term, false).await.unwrap();
        match &h.app.feed[h.app.picked.expect("something picked")] {
            Feed::Msg { body, .. } => assert_eq!(body, "segunda"),
            _ => panic!("expected a message"),
        }

        handle_key(&mut h.app, alt_up, &mut h.term, false).await.unwrap();
        match &h.app.feed[h.app.picked.unwrap()] {
            Feed::Msg { body, .. } => assert_eq!(body, "primeira"),
            _ => panic!("expected a message"),
        }

        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        handle_key(&mut h.app, ctrl_r, &mut h.term, false).await.unwrap();
        assert!(h.app.replying.is_some(), "ctrl+r should arm the reply");
        assert!(h.painted().contains("replying to"));

        // Esc unwinds the reply before touching anything else.
        h.press(KeyCode::Esc).await;
        assert!(h.app.replying.is_none());
    }

    #[tokio::test]
    async fn hovering_reveals_icons_and_clicking_one_acts_on_that_message() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 7, "Dale", "e a daily?");
        h.painted();

        let plain = h.painted();
        assert!(!plain.contains("reply"), "icons stay hidden until hovered");

        let row = first_message_row(&h.app);
        mouse(&mut h.app, MouseEventKind::Moved, row, 4);
        assert_eq!(h.app.hover, h.app.message_at(row));
        let shown = h.painted();
        assert!(shown.contains("reply") && shown.contains("copy"));

        let anchor = h
            .app
            .rendered
            .as_ref()
            .unwrap()
            .layout
            .actions
            .clone()
            .expect("icons should be anchored");
        let icon_row = h.app.chat_area.y + (anchor.line as u16 - h.app.scroll);

        mouse(
            &mut h.app,
            MouseEventKind::Down(MouseButton::Left),
            icon_row,
            anchor.reply.start,
        );
        assert_eq!(
            h.app.replying,
            Some(([9u8; 32], 7)),
            "clicking the reply icon should arm that very message"
        );

        // A click away from the icons just picks the message.
        h.app.replying = None;
        mouse(&mut h.app, MouseEventKind::Down(MouseButton::Left), row, 4);
        assert!(h.app.replying.is_none());
        assert!(h.app.picked.is_some());
    }

    #[tokio::test]
    async fn a_whisper_is_labelled_for_its_two_ends_and_gone_in_the_disguise() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        h.app.names.insert([9u8; 32], "Dale".into());

        let me = h.app.me;
        h.app.feed.push(Feed::Msg {
            author: [9u8; 32],
            seq: 1,
            name: "Dale".into(),
            mine: false,
            body: "isso fica entre nos".into(),
            ts: now_ts(),
            reply_to: None,
            whisper: Some(me),
            image: None,
        });

        let shown: String = build_lines(&h.app, 80).lines.iter().map(flatten).collect();
        assert!(shown.contains("isso fica entre nos"));
        assert!(
            shown.contains(&format!("Dale {WHISPER_MARK} you")),
            "a whisper should say who it is between"
        );

        h.app.masked = true;
        let hidden: String = build_lines(&h.app, 80).lines.iter().map(flatten).collect();
        assert!(
            !hidden.contains("isso fica entre nos"),
            "the disguise must drop whispers entirely, not relabel them"
        );
    }

    #[tokio::test]
    async fn whispering_needs_a_name_it_knows() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;

        h.cmd("/w").await;
        assert!(h.app.status.contains("usage"));

        h.cmd("/w Fulano oi tudo bem").await;
        assert!(
            h.app.status.contains("Fulano"),
            "an unknown name should be named back, got {:?}",
            h.app.status
        );

        // A name on its own is no longer a mistake: it aims the prompt, which
        // is what stops the *next* line needing the command again.
        h.app.names.insert([9u8; 32], "Diamante".into());
        h.cmd("/w Diamante").await;
        assert_eq!(h.app.whispering, Some([9u8; 32]));
        assert!(
            h.app.status.contains("Diamante"),
            "it has to say who is listening, got {:?}",
            h.app.status
        );
        h.app.whispering = None;

        // Tab completion fills the name in from who has spoken.
        h.app.input.clear();
        h.app.input.insert_str("/w dia");
        h.press(KeyCode::Tab).await;
        assert_eq!(h.app.input.text, "/w Diamante ");
    }

    /// Publishes a made-up peer's whisper key into the open room, so a
    /// whisper to them actually composes instead of failing for want of a key.
    async fn peer_who_can_be_whispered_to(app: &mut App, nick: &str) -> [u8; 32] {
        let secret = iroh::SecretKey::generate();
        let author = *secret.public().as_bytes();
        let x_pub = crate::crypto::whisper_public(&crate::crypto::whisper_secret(
            &secret.to_bytes(),
        ));
        let sig = secret.sign(&crate::room::identity_payload(&author, &x_pub));
        let identity = Record::Identity {
            author,
            x_pub,
            sig: sig.to_bytes().to_vec(),
        };
        let room = app.room.clone().expect("a room has to be open");
        room.lock().await.ingest(identity).unwrap();
        app.names.insert(author, nick.into());
        author
    }

    #[tokio::test]
    async fn a_whisper_carries_the_reply_and_does_not_leave_it_armed() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let them = peer_who_can_be_whispered_to(&mut h.app, "Diamante").await;
        push_peer(&mut h.app, them, 1, "Diamante", "alguem viu o relatorio?");
        let answered = (them, 1u64);

        // Point at the message, then answer it privately.
        let last = h.app.feed.len() - 1;
        arm_reply(&mut h.app, last);
        assert_eq!(h.app.replying, Some(answered));
        h.cmd("/w Diamante ta comigo").await;

        // The quote reaches the whisper that was actually sent.
        match h.app.feed.last() {
            Some(Feed::Msg {
                body,
                reply_to,
                whisper: Some(_),
                ..
            }) => {
                assert_eq!(body, "ta comigo");
                assert_eq!(
                    *reply_to,
                    Some(answered),
                    "the whisper should quote what it answered"
                );
            }
            _ => panic!("expected a whisper, status was {:?}", h.app.status),
        }

        // And it is cleared afterwards, like every other kind of send. Leaving
        // it armed made the *next* ordinary message quote something at random,
        // which is what made this look broken rather than merely missing.
        assert_eq!(h.app.replying, None, "the reply must not stay armed");

        send(&mut h.app, "assunto totalmente diferente".into()).await;
        match h.app.feed.last() {
            Some(Feed::Msg {
                reply_to, body, ..
            }) => {
                assert_eq!(body, "assunto totalmente diferente");
                assert_eq!(*reply_to, None, "a stale reply leaked into the next message");
            }
            _ => panic!("expected a message"),
        }
    }

    /// What a bystander sees when somebody answers a whisper **out loud**.
    ///
    /// Two other people whisper to each other; one of them then quotes that
    /// whisper in an ordinary message. We are the third person: we hold the
    /// whisper record and cannot open it, so the question is whether the quote
    /// hands us its contents anyway.
    #[tokio::test]
    async fn quoting_a_whisper_in_public_must_not_hand_it_to_the_room() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let room = h.app.room.clone().unwrap();

        // Two real people, on their own machines, with their own keys.
        let (ta, tb) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let pin = { room.lock().await.pin.clone() };
        let mut ana = OpenRoom::join(
            &crate::store::DataDir::from_path(ta.path().to_path_buf()).unwrap(),
            pin.clone(),
            Some("sala"),
        )
        .unwrap();
        let mut bia = OpenRoom::join(
            &crate::store::DataDir::from_path(tb.path().to_path_buf()).unwrap(),
            pin,
            Some("sala"),
        )
        .unwrap();
        let ia = ana.announce_identity().unwrap().unwrap();
        let ib = bia.announce_identity().unwrap().unwrap();
        bia.ingest(ia.clone()).unwrap();
        ana.ingest(ib.clone()).unwrap();

        // Ana whispers to Bia. We receive the record like everybody else.
        let secret = ana
            .compose_whisper(bia.author, "o chefe vai demitir o Carlos".into(), None)
            .unwrap();
        let answered = secret.chat_key().unwrap();

        // Bia answers it *out loud*, quoting the whisper.
        bia.ingest(secret.clone()).unwrap();
        let aloud = bia.compose("serio isso?".into(), Some(answered)).unwrap();

        {
            let mut room = room.lock().await;
            room.ingest(ia).unwrap();
            room.ingest(ib).unwrap();
            room.ingest(secret).unwrap();
            room.ingest(aloud).unwrap();
        }
        sync_feed(&mut h.app).await;

        let screen = h.painted();
        assert!(
            screen.contains("serio isso?"),
            "the public answer itself is public, and should show"
        );
        assert!(
            !screen.contains("demitir"),
            "the whisper's text must never reach a bystander's screen"
        );
        assert!(
            !screen.contains("chefe"),
            "not even part of it"
        );
        // The whisper is not in our feed at all, so there is nothing to quote
        // from -- the quote degrades to a placeholder.
        assert!(
            screen.contains("not here yet"),
            "the quote should read as missing, got:\n{screen}"
        );
    }

    /// The dangerous half of the same situation: what the *sender* sees.
    ///
    /// We can open the whisper, so on our screen the quote renders in full --
    /// attached to a message everybody else can read. That is how somebody
    /// writes "pois e, melhor nao contar pro Carlos" believing the context is
    /// there, and leaks the whisper in their own words.
    #[tokio::test]
    async fn answering_a_whisper_out_loud_shows_us_a_context_nobody_else_has() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let them = peer_who_can_be_whispered_to(&mut h.app, "Diamante").await;

        // A whisper we received and can read.
        let room = h.app.room.clone().unwrap();
        let seq = { room.lock().await.log.next_seq_for(&them) };
        h.app.by_key.insert((them, seq), h.app.feed.len());
        h.app.feed.push(Feed::Msg {
            author: them,
            seq,
            name: "Diamante".into(),
            mine: false,
            body: "o chefe vai demitir o Carlos".into(),
            ts: now_ts(),
            reply_to: None,
            // `whisper` holds the *other* end: for one we received, the sender.
            whisper: Some(them),
            image: None,
        });

        let last = h.app.feed.len() - 1;
        arm_reply(&mut h.app, last);
        send(&mut h.app, "serio isso?".into()).await;

        let screen = h.painted();
        assert!(
            !screen.contains("serio isso?") || h.app.input.text.contains("serio isso?"),
            "the public message must not have gone out with a whisper quoted              onto it:
{screen}"
        );

        // Nothing was sent; what we typed comes back as a ready whisper.
        assert_eq!(
            h.app.input.text, "/w Diamante serio isso?",
            "the text must not be thrown away -- it comes back one Enter from              going privately"
        );
        assert!(
            h.app.status.contains("whisper"),
            "and it has to say why, got {:?}",
            h.app.status
        );
        assert!(
            h.app.replying.is_some(),
            "the quote stays armed so the whisper can still carry it"
        );

        // Esc drops the quote, and the same text then goes to the room.
        h.press(KeyCode::Esc).await;
        assert!(h.app.replying.is_none(), "esc has to release the quote");
    }

    /// The same trap one step further in: quoting Ana's whisper into a whisper
    /// meant for Bia. Bia cannot open Ana's whisper either, so she gets the
    /// placeholder while we sit looking at the full text.
    #[tokio::test]
    async fn a_whisper_quote_cannot_be_carried_to_a_third_person() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let ana = peer_who_can_be_whispered_to(&mut h.app, "Ana").await;
        let bia = peer_who_can_be_whispered_to(&mut h.app, "Bia").await;

        // A whisper Ana sent us.
        let seq = 1u64;
        h.app.by_key.insert((ana, seq), h.app.feed.len());
        h.app.feed.push(Feed::Msg {
            author: ana,
            seq,
            name: "Ana".into(),
            mine: false,
            body: "o chefe vai demitir o Carlos".into(),
            ts: now_ts(),
            reply_to: None,
            whisper: Some(ana),
            image: None,
        });
        let last = h.app.feed.len() - 1;
        arm_reply(&mut h.app, last);

        // Passing it along to Bia must not carry the quote.
        h.cmd("/w Bia olha o que a Ana falou").await;
        match h.app.feed.last() {
            Some(Feed::Msg {
                reply_to,
                whisper: Some(other),
                ..
            }) => {
                assert_eq!(*other, bia, "should have gone to Bia");
                assert_eq!(
                    *reply_to, None,
                    "Bia cannot open Ana's whisper, so quoting it at her only \
                     shows *us* a context she does not have"
                );
            }
            _ => panic!("expected a whisper, status {:?}", h.app.status),
        }
        assert!(
            h.app.status.contains("Ana"),
            "and it has to say the quote was dropped and why, got {:?}",
            h.app.status
        );
    }

    /// A picture always goes to the room, so a whisper quote must not ride
    /// along with it either.
    #[tokio::test]
    async fn a_picture_cannot_carry_a_whisper_quote_to_the_room() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let ana = peer_who_can_be_whispered_to(&mut h.app, "Ana").await;

        h.app.by_key.insert((ana, 1), h.app.feed.len());
        h.app.feed.push(Feed::Msg {
            author: ana,
            seq: 1,
            name: "Ana".into(),
            mine: false,
            body: "o chefe vai demitir o Carlos".into(),
            ts: now_ts(),
            reply_to: None,
            whisper: Some(ana),
            image: None,
        });
        let last = h.app.feed.len() - 1;
        arm_reply(&mut h.app, last);

        // A tiny real PNG, through the same path a pasted screenshot takes.
        let mut img = image::RgbaImage::new(8, 8);
        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        send_image(&mut h.app, png.into_inner()).await;

        match h.app.feed.last() {
            Some(Feed::Msg {
                reply_to,
                image: Some(_),
                ..
            }) => assert_eq!(
                *reply_to, None,
                "a picture goes to the room; a whisper quote on it would show \
                 the context to us alone"
            ),
            _ => panic!("expected a picture, status {:?}", h.app.status),
        }
        assert!(
            h.app.status.contains("quote dropped"),
            "and it has to say so, got {:?}",
            h.app.status
        );
    }

    /// The whole point of the sticky mode: the second private sentence does
    /// not need the command, and therefore cannot be sent to the room by
    /// forgetting it.
    #[tokio::test]
    async fn whispering_stays_pointed_at_the_same_person() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let dale = peer_who_can_be_whispered_to(&mut h.app, "Dale").await;

        h.cmd("/w Dale ta rolando aquilo?").await;
        assert_eq!(h.app.whispering, Some(dale), "the prompt should follow");

        // The line that used to go to the whole room.
        send(&mut h.app, "pois e, o chefe soube".into()).await;
        match h.app.feed.last() {
            Some(Feed::Msg {
                body,
                whisper: Some(other),
                ..
            }) => {
                assert_eq!(body, "pois e, o chefe soube");
                assert_eq!(*other, dale, "it must have stayed private");
            }
            _ => panic!("the follow-up went to the room, status {:?}", h.app.status),
        }
    }

    #[tokio::test]
    async fn the_prompt_says_who_is_listening_and_esc_lets_go() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        peer_who_can_be_whispered_to(&mut h.app, "Dale").await;

        assert!(!h.painted().contains("Dale >"), "nothing pointed at yet");

        // A bare `/w <name>` just aims the prompt.
        h.cmd("/w Dale").await;
        let aimed = h.painted();
        assert!(
            aimed.contains(&format!("Dale {WHISPER_MARK}")),
            "the prompt has to name who is listening:\n{aimed}"
        );

        // Esc clears a half-typed line first, so one keystroke too many never
        // drops you into the room mid-sentence.
        h.app.input.insert_str("meia frase");
        h.press(KeyCode::Esc).await;
        assert_eq!(h.app.input.text, "", "the line goes first");
        assert_eq!(h.app.whispering, h.app.whispering, "still aimed");
        assert!(h.app.whispering.is_some(), "and only then the whisper");

        h.press(KeyCode::Esc).await;
        assert_eq!(h.app.whispering, None, "the second esc lets go");
        assert!(!h.painted().contains(&format!("Dale {WHISPER_MARK}")));
    }

    /// A real name at the prompt would walk straight through the disguise.
    #[tokio::test]
    async fn the_disguise_takes_the_name_off_the_prompt() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        peer_who_can_be_whispered_to(&mut h.app, "Dale").await;
        h.cmd("/w Dale").await;

        h.app.masked = true;
        let masked = h.painted();
        assert!(!masked.contains("Dale"), "the name leaked:\n{masked}");
        // Still aimed, though -- F12 hides, it does not decide who you talk to.
        assert!(h.app.whispering.is_some());
    }

    /// Nobody from the previous room can be whispered to from this one.
    #[tokio::test]
    async fn leaving_a_room_lets_go_of_the_whisper() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        peer_who_can_be_whispered_to(&mut h.app, "Dale").await;
        h.cmd("/w Dale").await;
        assert!(h.app.whispering.is_some());

        h.cmd("/leave").await;
        assert_eq!(
            h.app.whispering, None,
            "a prompt still pointing at someone from another room would be a lie"
        );
    }

    /// Prints the prompt in both states, to eyeball that "who is listening"
    /// is impossible to miss.
    ///
    ///   cargo test preview_the_prompt -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn preview_the_prompt() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        peer_who_can_be_whispered_to(&mut h.app, "Dale").await;

        h.app.input.insert_str("e a daily, o que ficou?");
        let room = h.painted_lines();
        println!("
--- falando com a sala ---");
        for line in room.iter().rev().take(3).rev() {
            println!("{line}");
        }

        h.cmd("/w Dale").await;
        h.app.input.clear();
        h.app.input.insert_str("pois e, o chefe soube");
        let quiet = h.painted_lines();
        println!("
--- sussurrando ---");
        for line in quiet.iter().rev().take(3).rev() {
            println!("{line}");
        }
        println!();
    }

    /// The quote lives inside the ciphertext, so the disguise -- which drops
    /// whispers entirely -- must not put it back on screen.
    #[tokio::test]
    async fn a_quoted_whisper_still_vanishes_under_the_disguise() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        let them = peer_who_can_be_whispered_to(&mut h.app, "Diamante").await;
        push_peer(&mut h.app, them, 1, "Diamante", "o relatorio do Pedro");
        let last = h.app.feed.len() - 1;
        arm_reply(&mut h.app, last);
        h.cmd("/w Diamante ta comigo").await;

        h.app.masked = true;
        let masked = h.painted();
        assert!(!masked.contains("ta comigo"), "whisper text leaked");
        assert!(!masked.contains("Diamante"), "whisper name leaked");
        assert!(
            !masked.contains(QUOTE_MARK),
            "the quote line leaked through the disguise"
        );
    }

    #[tokio::test]
    async fn a_whisper_target_may_have_spaces_in_its_name() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.app.names.insert([9u8; 32], "Grok 4.5".into());
        h.app.names.insert([7u8; 32], "Grok".into());

        // The longest matching name wins, otherwise "Grok 4.5 e ai" would be
        // read as a whisper to "Grok" saying "4.5 e ai".
        assert_eq!(
            h.app.split_whisper("Grok 4.5 e ai"),
            Some(([9u8; 32], "e ai"))
        );
        assert_eq!(h.app.split_whisper("Grok e ai"), Some(([7u8; 32], "e ai")));
        // Case does not matter, unknown names do.
        assert_eq!(h.app.split_whisper("grok 4.5 oi"), Some(([9u8; 32], "oi")));
        assert_eq!(h.app.split_whisper("Fulano oi"), None);
        // And a name must end on a word boundary.
        assert_eq!(h.app.split_whisper("Grokzinho oi"), None);

        // Tab completes across the space too.
        h.app.input.clear();
        h.app.input.insert_str("/w grok 4");
        h.press(KeyCode::Tab).await;
        assert_eq!(h.app.input.text, "/w Grok 4.5 ");
    }

    #[tokio::test]
    async fn answering_a_hidden_message_does_not_quote_it_back_onto_the_screen() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 3, "Dale", "aquilo que eu escondi");

        let idx = h.app.feed.len() - 1;
        toggle_hidden(&mut h.app, idx).await;
        arm_reply(&mut h.app, idx);
        send(&mut h.app, "respondendo".into()).await;

        let painted = h.painted();
        assert!(painted.contains("respondendo"));
        assert!(
            !painted.contains("aquilo que eu escondi"),
            "the quote put the hidden message straight back on screen"
        );
        assert!(painted.contains(BLOCK), "it should still show as blurred");
    }

    #[tokio::test]
    async fn a_screen_row_maps_back_to_its_message() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        for i in 0..5 {
            send(&mut h.app, format!("mensagem {i}")).await;
        }
        h.painted();

        let area = h.app.chat_area;
        let mut walked: Vec<usize> = Vec::new();
        for row in area.y..area.y + area.height {
            if let Some(idx) = h.app.message_at(row) {
                if walked.last() != Some(&idx) {
                    walked.push(idx);
                }
            }
        }

        assert!(walked.len() >= 2, "several messages should be on screen");
        assert!(
            walked.windows(2).all(|pair| pair[0] < pair[1]),
            "rows must map to messages in order, got {walked:?}"
        );
        match &h.app.feed[*walked.last().unwrap()] {
            Feed::Msg { body, .. } => assert_eq!(body, "mensagem 4"),
            _ => panic!("newest row should own a chat message"),
        }

        // The header row is above the transcript and owns no message.
        assert_eq!(h.app.message_at(0), None);
        assert_eq!(h.app.message_at(area.y + area.height), None);
    }

    #[tokio::test]
    async fn hovering_only_lands_on_messages_not_on_notices() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.painted();

        // Fresh room: the transcript is all notices (the key, the warning).
        let area = h.app.chat_area;
        let any = (area.y..area.y + area.height).any(|row| h.app.message_at(row).is_some());
        assert!(!any, "notices must not be hoverable targets");
    }

    #[tokio::test]
    async fn people_get_different_colours_until_the_disguise_is_on() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 1, "Dale", "primeiro");
        push_peer(&mut h.app, [200u8; 32], 2, "Ana", "segundo");

        let lines = build_lines(&h.app, 80).lines;
        let dale = head_colour(&lines, "Dale").expect("Dale on screen");
        let ana = head_colour(&lines, "Ana").expect("Ana on screen");
        assert_ne!(dale, ana, "two people must not share a colour");

        // Same person, same colour, with no configuration anywhere.
        let again = build_lines(&h.app, 100).lines;
        assert_eq!(head_colour(&again, "Dale"), Some(dale));

        h.app.masked = true;
        let masked = build_lines(&h.app, 80).lines;
        assert_eq!(
            head_colour(&masked, "gpt-oss"),
            head_colour(&masked, "assistant"),
            "the disguise must flatten everyone onto one neutral tone"
        );
    }

    #[tokio::test]
    async fn my_messages_sit_right_theirs_sit_left() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "vou sair mais cedo".into()).await;
        push_peer(&mut h.app, [9u8; 32], 3, "Dale", "beleza");

        let lines = build_lines(&h.app, 80).lines;
        let find = |needle: &str| {
            lines
                .iter()
                .map(flatten)
                .find(|text| text.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle}"))
        };

        let mine = find("vou sair mais cedo");
        assert!(mine.starts_with(' '), "my message should be pushed over");
        assert_eq!(mine.chars().count(), 78, "and end on the right edge");
        assert!(mine.trim_end().ends_with("vou sair mais cedo"));

        assert_eq!(find("beleza"), "  beleza", "theirs stays flush left");
        assert!(find("Pedro").trim_end().ends_with("Pedro"));
        assert!(find("Dale").starts_with("  Dale"));
    }

    #[tokio::test]
    async fn the_disguise_flattens_everything_back_to_the_left() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "vou sair mais cedo".into()).await;

        h.app.masked = true;
        let lines = build_lines(&h.app, 80).lines;
        let mine = lines
            .iter()
            .map(flatten)
            .find(|text| text.contains("vou sair mais cedo"))
            .unwrap();
        // Staggered text would read as a chat, not as an inference log.
        assert_eq!(mine, "  vou sair mais cedo");

        // A quoted line would carry a real name straight through the disguise.
        h.app.masked = false;
        let answered = match h.app.feed.last().unwrap() {
            Feed::Msg { author, seq, .. } => (*author, *seq),
            _ => panic!("expected a message"),
        };
        h.app.replying = Some(answered);
        send(&mut h.app, "e ai".into()).await;
        h.app.masked = true;
        let masked: String = build_lines(&h.app, 80).lines.iter().map(flatten).collect();
        assert!(
            !masked.contains(QUOTE_MARK) && !masked.contains("Pedro"),
            "quotes must not leak a name into the disguise"
        );
    }

    /// Not an assertion — a way to eyeball the chat layout without launching
    /// the app, which needs a real terminal.
    ///   cargo test preview_the_chat_layout -- --ignored --nocapture
    #[ignore]
    #[tokio::test]
    async fn preview_the_chat_layout() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        push_peer(&mut h.app, [9u8; 32], 4, "Dale", "e a daily, o que ficou?");
        send(&mut h.app, "deploy quinta, eu pego o script".into()).await;
        push_peer(&mut h.app, [9u8; 32], 5, "Dale", "fechou. lembra que o banco de homologacao cai as 18h, entao tem que subir antes disso");
        // Answer Dale, so the quote line shows up in the preview.
        let answered = match h.app.feed.last().unwrap() {
            Feed::Msg { author, seq, .. } => (*author, *seq),
            _ => panic!("expected a message"),
        };
        h.app.replying = Some(answered);
        send(&mut h.app, "opa, boa. subo 17h entao".into()).await;
        h.app.names.insert([9u8; 32], "Dale".into());
        let me = h.app.me;
        h.app.feed.push(Feed::Msg {
            author: [9u8; 32],
            seq: 9,
            name: "Dale".into(),
            mine: false,
            body: "psiu, o cliente ligou reclamando de novo".into(),
            ts: now_ts(),
            reply_to: None,
            whisper: Some(me),
            image: None,
        });
        push_peer(&mut h.app, [9u8; 32], 11, "Dale", "e aquele cliente chato de novo");
        let heavy = h.app.feed.len() - 1;
        toggle_hidden(&mut h.app, heavy).await;
        h.app.input.insert_str("ate amanha");
        h.painted();

        for label in ["normal", "hover", "F12 (disguise)"] {
            h.app.masked = label.starts_with("F12");
            h.app.hover = if label == "hover" {
                h.app.message_at(first_message_row(&h.app))
            } else {
                None
            };
            println!("\n=== {label} ===");
            let painted: Vec<char> = h.painted().chars().collect();
            for row in painted.chunks(80) {
                println!("|{}|", row.iter().collect::<String>().trim_end());
            }
        }
    }

    #[tokio::test]
    async fn the_whole_help_fits_a_small_terminal() {
        // The Harness paints 80x24, about the smallest window anyone uses.
        // Help that needs scrolling is help nobody reads, so every entry has
        // to be on screen at once.
        let mut h = Harness::new();
        h.cmd("/help").await;
        let painted = h.painted();
        for probe in [
            "/new", "/join", "/w ", "/paste", "/quit", "f1", "f12", "ctrl+h", "shift+enter", "esc",
        ] {
            assert!(painted.contains(probe), "{probe} fell off the help screen");
        }
    }

    #[tokio::test]
    async fn help_owns_the_screen_so_traffic_cannot_bury_it() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.cmd("/help").await;
        assert!(matches!(h.app.screen, Screen::Help));

        let painted = h.painted();
        assert!(painted.contains("/join"), "commands must be listed");
        assert!(painted.contains("f12"), "keys must be listed");

        // Help used to live in the transcript, where incoming records pushed
        // it off screen within seconds.
        for _ in 0..50 {
            apply_net(&mut h.app, NetEvent::Record).await;
        }
        assert!(h.painted().contains("/join"));

        h.press(KeyCode::Esc).await;
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[tokio::test]
    async fn a_room_shows_its_name_once_no_matter_how_often_sync_resends_it() {
        let mut h = Harness::new();
        h.cmd("/new teste").await;
        let room = h.app.room.clone().unwrap();
        for _ in 0..40 {
            let echoed = Record::Meta {
                alias: "teste".into(),
            };
            room.lock().await.ingest(echoed).unwrap();
        }
        sync_feed(&mut h.app).await;

        let banners = h
            .app
            .feed
            .iter()
            .filter(|item| matches!(item, Feed::System { body } if body == "session teste"))
            .count();
        assert_eq!(banners, 1, "sync echoes must not pile up on screen");
    }

    #[tokio::test]
    async fn escape_clears_the_line_instead_of_dropping_the_session() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        h.app.input.insert_str("half typed");

        h.press(KeyCode::Esc).await;
        assert!(h.app.input.text.is_empty());
        assert!(
            matches!(h.app.screen, Screen::Chat) && h.app.room.is_some(),
            "esc used to tear down the room and force a re-unlock"
        );

        h.cmd("/leave").await;
        assert!(matches!(h.app.screen, Screen::Home));
    }

    #[tokio::test]
    async fn f12_hides_names_notices_and_the_draft() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "o chefe pegou no pe hoje".into()).await;
        h.app.input.insert_str("nao presta mesmo");

        let plain = h.painted();
        assert!(plain.contains("Pedro"));

        h.press(KeyCode::F(12)).await;
        let masked = h.painted();
        assert!(!masked.contains("Pedro"), "real name leaked while masked");
        assert!(!masked.contains("nao presta"), "draft leaked while masked");
        assert!(!masked.contains("key "), "session key leaked while masked");
        assert!(masked.contains("ctx "), "should read like an inference client");

        // A stray keystroke must not undo the disguise; F12 must.
        h.press(KeyCode::Char('x')).await;
        assert!(h.app.masked);
        h.press(KeyCode::F(12)).await;
        assert!(!h.app.masked);
        assert_eq!(h.app.input.text, "nao presta mesmo");
    }

    // Held across awaits on purpose: these tests share process-wide env vars
    // and a port slot, and #[tokio::test] runs them on a single thread.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_remembered_key_opens_without_asking() {
        let _guard = serialize();
        let tmp = TempDir::new().unwrap();
        isolate(&tmp);

        let topic = {
            let mut app = App::new().unwrap();
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            handle_command(&mut app, "/new gpt-oss-20b".into(), &mut term)
                .await
                .unwrap();
            let topic = topic_id(&app.room.as_ref().unwrap().lock().await.pin);
            app.shutdown_net().await;
            topic
        };

        let mut app = App::new().unwrap();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].remembered, "dpapi blob should be on disk");

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter, &mut term, false).await.unwrap();
        assert!(
            matches!(app.screen, Screen::Chat),
            "a remembered session must not ask for the key again"
        );

        app.dir.forget_pin(&topic).unwrap();
        app.refresh_sessions();
        assert!(!app.sessions[0].remembered);
    }

    #[tokio::test]
    async fn the_view_sticks_to_the_newest_message() {
        let mut h = Harness::new();
        h.cmd("/new gpt-oss-20b").await;
        for i in 0..60 {
            send(&mut h.app, format!("message number {i}")).await;
        }
        h.painted();
        assert!(h.app.follow);
        assert_eq!(h.app.scroll, h.app.max_scroll);
        assert!(h.app.max_scroll > 0, "60 messages must overflow 24 rows");
        assert!(h.painted().contains("message number 59"));

        h.press(KeyCode::PageUp).await;
        h.painted();
        assert!(!h.app.follow);
        assert!(h.app.scroll < h.app.max_scroll, "pgup must move up, not down");

        // New traffic while scrolled back is counted, not silently jumped to.
        h.app.consumed = 0;
        h.app.feed.clear();
        apply_net(&mut h.app, NetEvent::Record).await;
        assert!(h.app.unread > 0);
    }

    #[test]
    fn wrap_breaks_on_words_and_keeps_blank_lines() {
        assert_eq!(wrap_text("a b c", 5), vec!["a b c"]);
        assert_eq!(wrap_text("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_text("one\n\ntwo", 10), vec!["one", "", "two"]);
    }

    #[test]
    fn wrap_splits_words_longer_than_the_line() {
        let ticket = "a".repeat(20);
        let lines = wrap_text(&ticket, 8);
        assert_eq!(lines, vec!["aaaaaaaa", "aaaaaaaa", "aaaa"]);
    }

    #[test]
    fn input_edits_by_char_not_byte() {
        let mut input = Input::default();
        input.insert_str("ação");
        input.cursor = 2;
        input.backspace();
        assert_eq!(input.text, "aão");
        input.kill_to_start();
        assert_eq!(input.text, "ão");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn kill_word_stops_at_the_previous_space() {
        let mut input = Input::default();
        input.insert_str("/join 7K2M-9QXP");
        input.kill_word();
        assert_eq!(input.text, "/join ");
    }

    #[test]
    fn history_recall_walks_back_and_restores_the_draft() {
        let mut input = Input::default();
        input.insert_str("first");
        input.take();
        input.insert_str("second");
        input.take();
        input.insert_str("draft");
        input.recall_prev();
        assert_eq!(input.text, "second");
        input.recall_prev();
        assert_eq!(input.text, "first");
        input.recall_next();
        assert_eq!(input.text, "second");
        input.recall_next();
        assert_eq!(input.text, "draft");
    }
}
