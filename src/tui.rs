use crate::crypto::{role_for, topic_id, Pin};
use crate::net::{parse_ticket, short_id, NetEvent, NetSession, Presence};
use crate::room::OpenRoom;
use crate::store::{now_ts, DataDir, Record};
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
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
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
        name: String,
        mine: bool,
        body: String,
        ts: u64,
    },
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
    peers: Vec<EndpointId>,
    names: HashMap<[u8; 32], String>,
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
}

impl App {
    pub fn new() -> Result<Self> {
        let dir = DataDir::open()?;
        let nick = dir.load_nick();
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
            names: HashMap::new(),
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

    fn notice(&mut self, body: impl Into<String>) {
        self.feed.push(Feed::Notice { body: body.into() });
        self.follow = true;
        self.unread = 0;
    }

    async fn shutdown_net(&mut self) {
        if let Some(net) = self.net.take() {
            let _ = net.shutdown().await;
        }
        self.events_rx = None;
        self.peers.clear();
        self.ticket = None;
        self.room = None;
        self.feed.clear();
        self.consumed = 0;
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

    fn peer_name(&self, id: &EndpointId) -> String {
        self.names
            .get(id.as_bytes())
            .cloned()
            .unwrap_or_else(|| short_id(id))
    }

    /// Best-effort audible ping, rate limited so a history sync that ingests
    /// fifty records does not machine-gun the terminal bell.
    fn ring(&mut self) {
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

fn spawn_input_thread() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        })
        .expect("input thread");
    rx
}

async fn run_inner(terminal: &mut Term) -> Result<()> {
    let mut app = App::new()?;
    let mut events = spawn_input_thread();
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        tokio::select! {
            maybe = events.recv() => {
                let Some(ev) = maybe else { continue };
                match ev {
                    Event::Key(key) => {
                        // Windows reports press and release; only one is input.
                        if key.kind == KeyEventKind::Release {
                            continue;
                        }
                        if handle_key(&mut app, key, terminal).await? {
                            break;
                        }
                    }
                    Event::Paste(text) => paste(&mut app, &text),
                    Event::Mouse(m) => handle_mouse(&mut app, m),
                    _ => {}
                }
            }
            ev = recv_net(&mut app) => {
                if let Some(ev) = ev {
                    apply_net(&mut app, ev).await;
                }
            }
        }
    }
    app.shutdown_net().await;
    Ok(())
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
        NetEvent::Peers(list) => {
            let before: Vec<EndpointId> = std::mem::take(&mut app.peers);
            for id in &list {
                if !before.contains(id) {
                    let who = app.peer_name(id);
                    app.notice(format!("{who} is here"));
                }
            }
            for id in &before {
                if !list.contains(id) {
                    let who = app.peer_name(id);
                    app.notice(format!("{who} left"));
                }
            }
            app.peers = list;
        }
        NetEvent::Record => {
            let before = app.consumed;
            sync_feed(app).await;
            if app.consumed > before {
                if app.follow {
                    app.unread = 0;
                } else {
                    app.unread += app.consumed - before;
                }
                app.ring();
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
            Record::Chat { author, ts, .. } | Record::ChatNamed { author, ts, .. } => {
                let name = room.label_of(rec);
                if let Record::ChatNamed { .. } = rec {
                    app.names.insert(*author, name.clone());
                }
                app.feed.push(Feed::Msg {
                    author: *author,
                    name,
                    mine: room.is_mine(rec),
                    body: rec.body().unwrap_or_default().to_string(),
                    ts: *ts,
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
        Screen::Chat => handle_chat(app, key, term).await,
        Screen::Help | Screen::Confirm { .. } => Ok(false),
    }
}

fn handle_mouse(app: &mut App, ev: crossterm::event::MouseEvent) {
    use crossterm::event::MouseEventKind;
    if !matches!(app.screen, Screen::Chat) {
        return;
    }
    match ev.kind {
        MouseEventKind::ScrollUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => scroll_down(app, 3),
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
) -> Result<bool> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let page = 6u16;
    match key.code {
        KeyCode::Esc => {
            if app.input.text.is_empty() {
                app.status = "/leave goes back to the session list".into();
            } else {
                app.input.clear();
            }
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
        KeyCode::Enter if shift => app.input.insert('\n'),
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

async fn send(app: &mut App, body: String) {
    let Some(room) = app.room.clone() else { return };
    let rec = {
        let mut room = room.lock().await;
        match room.compose(body) {
            Ok(rec) => rec,
            Err(e) => {
                app.status = format!("could not save message: {e}");
                return;
            }
        }
    };
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
            if app.peers.is_empty() {
                app.notice("nobody else is live right now");
            } else {
                let names: Vec<String> = app.peers.iter().map(|id| app.peer_name(id)).collect();
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
        "/diag" => diag(app).await,
        "/help" | "/?" => app.screen = Screen::Help,
        other => app.status = format!("no such command: {other} — /help"),
    }
    Ok(false)
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
        "window #{instance} · network {} · {} live · {known} route(s) known",
        if app.net.is_some() { "up" } else { "down" },
        app.peers.len()
    ));
    app.notice(format!(
        "presence dir {} ({files} file(s))",
        presence.display()
    ));
    if app.peers.is_empty() {
        app.notice(
            "no peers: check the windows firewall prompt was allowed on private networks, and that both machines are on the same subnet. /ticket works around mdns.",
        );
    }
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
    match NetSession::start(secret, shared, tx, bootstrap, presence).await {
        Ok(net) => {
            app.ticket = Some(net.addr());
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
            app.peers.len() + 1
        ),
        Screen::Chat => format!(
            "  local-llm  {}  {}  {} online{inst}",
            app.alias,
            app.nick,
            app.peers.len()
        ),
        Screen::Help => format!("  local-llm  {version}  keys and commands{inst}"),
        Screen::Confirm { alias, .. } => format!("  local-llm  delete {alias}{inst}"),
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

fn draw_help(f: &mut Frame, area: Rect) {
    const ROWS: &[(&str, &str)] = &[
        ("", ""),
        ("/new <name>", "create a session and show its key"),
        ("/join <key> [ticket]", "join one someone gave you"),
        ("/pin", "show the key again and copy it"),
        ("/ticket", "copy this window's address (when mdns fails)"),
        ("/peers", "who is online right now"),
        ("/nick <name>", "change the name others see"),
        ("/leave", "back to the session list"),
        ("/lock", "stop saving the key on this pc"),
        ("/forget", "delete this session from this pc"),
        ("/diag", "why nobody is showing up"),
        ("/quit", "exit"),
        ("", ""),
        ("f12", "hide names and notices instantly"),
        ("pgup / pgdn", "scroll — mouse wheel works too"),
        ("ctrl+end", "jump back to the newest message"),
        ("up / down", "reuse what you typed before"),
        ("shift+enter", "newline inside one message"),
        ("del", "on the session list: delete the selected one"),
        ("esc", "clear the line · close this help"),
    ];
    let lines: Vec<Line> = ROWS
        .iter()
        .map(|(key, what)| {
            if key.is_empty() {
                return Line::from("");
            }
            Line::from(vec![
                Span::styled(
                    format!("  {key:<22}"),
                    Style::default().fg(Color::Rgb(200, 220, 190)),
                ),
                Span::styled(what.to_string(), dim()),
            ])
        })
        .collect();
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
    let lines = build_lines(app, area.width);
    let viewport = area.height as usize;
    let max_scroll = lines.len().saturating_sub(viewport) as u16;
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
    f.render_widget(Paragraph::new(lines).scroll((app.scroll, 0)), area);
}

/// Spaces needed to push `len` columns of text so it ends at column `edge`.
fn indent(edge: usize, len: usize) -> String {
    " ".repeat(edge.saturating_sub(len))
}

fn build_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let inner = (width as usize).saturating_sub(4);
    // Column the right-aligned text ends on, and how wide a message may get
    // before it wraps. Narrower than the full width so the two sides read as
    // two columns instead of one ragged block.
    let edge = (width as usize).saturating_sub(2);
    let bubble = if inner <= 32 { inner } else { (inner * 7) / 10 };
    let mut out: Vec<Line> = Vec::new();
    let mut last_day: Option<(i32, u8, u8)> = None;
    // The transcript is re-laid out on every frame, so a year-old room must
    // not make each keystroke reformat tens of thousands of lines.
    let skipped = app.feed.len().saturating_sub(RENDER_CAP);
    if skipped > 0 && !app.masked {
        out.push(Line::from(Span::styled(
            format!("  ── {skipped} older entries kept in the log, not shown"),
            dim(),
        )));
        out.push(Line::from(""));
    }
    for item in app.feed.iter().skip(skipped) {
        match item {
            Feed::Notice { body } | Feed::System { body } => {
                // The disguise hides anything that talks about the network,
                // keys or people joining.
                if app.masked {
                    continue;
                }
                for piece in wrap_text(body, inner) {
                    out.push(Line::from(Span::styled(format!("  {piece}"), dim())));
                }
                out.push(Line::from(""));
            }
            Feed::Msg {
                author,
                name,
                mine,
                body,
                ts,
            } => {
                let day = civil(*ts, app.offset);
                if last_day != Some(day) && !app.masked {
                    out.push(Line::from(Span::styled(
                        format!("  ── {}", day_label(*ts, app.offset)),
                        dim(),
                    )));
                    out.push(Line::from(""));
                }
                last_day = Some(day);
                let label = if app.masked {
                    if *mine {
                        "user".to_string()
                    } else {
                        role_for(author).to_string()
                    }
                } else {
                    name.clone()
                };
                let head_style = Style::default()
                    .fg(if *mine {
                        Color::Rgb(160, 190, 220)
                    } else {
                        Color::Rgb(180, 210, 170)
                    })
                    .add_modifier(Modifier::BOLD);
                let body_style = Style::default().fg(Color::Rgb(220, 220, 220));

                // Your own messages hug the right edge, the way every chat
                // does it. The disguise turns that off: an inference log is
                // flush left, and staggered text would give it away at a
                // glance.
                if *mine && !app.masked {
                    let stamp = clock(*ts, app.offset);
                    let head_width = stamp.chars().count() + 2 + label.chars().count();
                    out.push(Line::from(vec![
                        Span::styled(indent(edge, head_width) + &stamp, dim()),
                        Span::styled(format!("  {label}"), head_style),
                    ]));
                    for piece in wrap_text(body, bubble) {
                        let pad = indent(edge, piece.chars().count());
                        out.push(Line::from(Span::styled(pad + &piece, body_style)));
                    }
                } else {
                    let mut head = vec![Span::styled(format!("  {label}"), head_style)];
                    if !app.masked {
                        head.push(Span::styled(format!("  {}", clock(*ts, app.offset)), dim()));
                    }
                    out.push(Line::from(head));
                    for piece in wrap_text(body, if app.masked { inner } else { bubble }) {
                        out.push(Line::from(Span::styled(format!("  {piece}"), body_style)));
                    }
                }
                out.push(Line::from(""));
            }
        }
    }
    out
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let text = if app.masked {
        MASKED_STATUS.to_string()
    } else if matches!(app.screen, Screen::Help) {
        "any key closes this".to_string()
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
    let room = (area.width as usize).saturating_sub(6).max(8);
    let start = cursor.saturating_sub(room);
    let shown: String = full.chars().skip(start).take(room).collect();
    let p = Paragraph::new(format!("  > {shown}"))
        .style(Style::default().fg(Color::Rgb(200, 200, 190)))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Rgb(50, 55, 50))),
        );
    f.render_widget(p, area);
    let col = 4u16.saturating_add((cursor - start) as u16);
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

        async fn press(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_key(&mut self.app, key, &mut self.term).await.unwrap();
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

    fn flatten(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[tokio::test]
    async fn my_messages_sit_right_theirs_sit_left() {
        let mut h = Harness::new();
        h.cmd("/nick Pedro").await;
        h.cmd("/new gpt-oss-20b").await;
        send(&mut h.app, "vou sair mais cedo".into()).await;
        h.app.feed.push(Feed::Msg {
            author: [9u8; 32],
            name: "Dale".into(),
            mine: false,
            body: "beleza".into(),
            ts: now_ts(),
        });

        let lines = build_lines(&h.app, 80);
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
        let lines = build_lines(&h.app, 80);
        let mine = lines
            .iter()
            .map(flatten)
            .find(|text| text.contains("vou sair mais cedo"))
            .unwrap();
        // Staggered text would read as a chat, not as an inference log.
        assert_eq!(mine, "  vou sair mais cedo");
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
        h.app.feed.push(Feed::Msg {
            author: [9u8; 32],
            name: "Dale".into(),
            mine: false,
            body: "e a daily, o que ficou?".into(),
            ts: now_ts(),
        });
        send(&mut h.app, "deploy quinta, eu pego o script".into()).await;
        h.app.feed.push(Feed::Msg {
            author: [9u8; 32],
            name: "Dale".into(),
            mine: false,
            body: "fechou. lembra que o banco de homologacao cai as 18h, entao tem que subir antes disso".into(),
            ts: now_ts(),
        });
        send(&mut h.app, "opa, boa".into()).await;
        h.app.input.insert_str("ate amanha");

        for masked in [false, true] {
            h.app.masked = masked;
            let label = if masked { "F12 (disguise)" } else { "normal" };
            println!("\n=== {label} ===");
            let painted: Vec<char> = h.painted().chars().collect();
            for row in painted.chunks(80) {
                println!("|{}|", row.iter().collect::<String>().trim_end());
            }
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
        handle_key(&mut app, enter, &mut term).await.unwrap();
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
