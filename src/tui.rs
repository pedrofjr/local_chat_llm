use crate::crypto::Pin;
use crate::net::{parse_ticket, NetEvent, NetSession};
use crate::room::OpenRoom;
use crate::store::{DataDir, IndexEntry, Record};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use iroh::EndpointAddr;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

enum Screen {
    Home,
    Unlock { alias: String },
    Chat,
}

pub struct App {
    dir: DataDir,
    screen: Screen,
    sessions: Vec<IndexEntry>,
    selected: usize,
    input: String,
    status: String,
    room: Option<Arc<Mutex<OpenRoom>>>,
    net: Option<NetSession>,
    events_rx: Option<mpsc::UnboundedReceiver<NetEvent>>,
    peer_count: usize,
    ticket: Option<String>,
    scroll: u16,
}

impl App {
    pub fn new() -> Result<Self> {
        let dir = DataDir::open()?;
        let sessions = dir.list_sessions().unwrap_or_default();
        Ok(Self {
            dir,
            screen: Screen::Home,
            sessions,
            selected: 0,
            input: String::new(),
            status: "/new <name>    /join <pin> [ticket]    /quit".into(),
            room: None,
            net: None,
            events_rx: None,
            peer_count: 0,
            ticket: None,
            scroll: 0,
        })
    }

    fn refresh_sessions(&mut self) {
        self.sessions = self.dir.list_sessions().unwrap_or_default();
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }
    }

    async fn shutdown_net(&mut self) {
        if let Some(net) = self.net.take() {
            let _ = net.shutdown().await;
        }
        self.events_rx = None;
        self.peer_count = 0;
        self.ticket = None;
        self.room = None;
    }
}

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_inner(&mut terminal).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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

fn is_typing(key: &KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
}

async fn run_inner(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut app = App::new()?;
    let mut keys = spawn_input_thread();
    loop {
        terminal.draw(|f| draw(f, &app))?;
        tokio::select! {
            maybe = keys.recv() => {
                let Some(Event::Key(key)) = maybe else { continue };
                if !is_typing(&key) {
                    continue;
                }
                if handle_key(&mut app, key).await? {
                    break;
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
    if let Some(rx) = app.events_rx.as_mut() {
        rx.recv().await
    } else {
        std::future::pending::<Option<NetEvent>>().await
    }
}

async fn apply_net(app: &mut App, ev: NetEvent) {
    match ev {
        NetEvent::Status(s) => app.status = s,
        NetEvent::Peers(n) => app.peer_count = n,
        NetEvent::Record => {}
        NetEvent::Ticket(t) => {
            app.ticket = Some(t);
        }
    }
}

async fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }
    match &app.screen {
        Screen::Home => match key.code {
            KeyCode::Up if app.selected > 0 => {
                app.selected -= 1;
            }
            KeyCode::Down if app.selected + 1 < app.sessions.len() => {
                app.selected += 1;
            }
            KeyCode::Enter => {
                if app.input.starts_with('/') || !app.input.is_empty() {
                    if handle_command(app, app.input.clone()).await? {
                        return Ok(true);
                    }
                    app.input.clear();
                } else if let Some(s) = app.sessions.get(app.selected).cloned() {
                    app.screen = Screen::Unlock {
                        alias: s.alias.clone(),
                    };
                    app.input.clear();
                    app.status = format!("pin for {}", s.alias);
                }
            }
            KeyCode::Esc => app.input.clear(),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        Screen::Unlock { alias, .. } => match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Home;
                app.input.clear();
                app.status = "/new <name>    /join <pin> [ticket]    /quit".into();
            }
            KeyCode::Enter => {
                let pin_raw = app.input.clone();
                match Pin::parse(&pin_raw) {
                    Ok(pin) => match OpenRoom::join(&app.dir, pin, Some(alias)) {
                        Ok(room) => {
                            enter_chat(app, room, Vec::new()).await;
                        }
                        Err(e) => app.status = format!("unlock failed: {e}"),
                    },
                    Err(e) => app.status = e.to_string(),
                }
                app.input.clear();
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        Screen::Chat => match key.code {
            KeyCode::Esc => {
                app.shutdown_net().await;
                app.refresh_sessions();
                app.screen = Screen::Home;
                app.status = "/new <name>    /join <pin> [ticket]    /quit".into();
            }
            KeyCode::PageUp => app.scroll = app.scroll.saturating_add(4),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(4),
            KeyCode::Enter => {
                let line = app.input.trim().to_string();
                app.input.clear();
                if line.is_empty() {
                    return Ok(false);
                }
                if line.starts_with('/') {
                    if handle_command(app, line).await? {
                        return Ok(true);
                    }
                } else if let Some(room) = &app.room {
                    let rec = {
                        let mut room = room.lock().await;
                        room.compose(line)?
                    };
                    if let Some(net) = &app.net {
                        if let Err(e) = net.broadcast(&rec).await {
                            app.status = format!("send failed: {e}");
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
    }
    Ok(false)
}

async fn handle_command(app: &mut App, line: String) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "/quit" | "/exit" => return Ok(true),
        "/new" => {
            let name = parts.collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                app.status = "usage: /new gpt-oss-20b".into();
                return Ok(false);
            }
            match OpenRoom::create(&app.dir, &name) {
                Ok(room) => {
                    let pin = room.pin.display();
                    enter_chat(app, room, Vec::new()).await;
                    app.status =
                        format!("session created. pin {pin}  — say it in person, not on teams.");
                }
                Err(e) => app.status = format!("/new failed: {e}"),
            }
        }
        "/join" => {
            let pin_raw = parts.next().unwrap_or("");
            let ticket = parts.next();
            match Pin::parse(pin_raw) {
                Ok(pin) => match OpenRoom::join(&app.dir, pin, None) {
                    Ok(room) => {
                        let mut boots = Vec::new();
                        if let Some(t) = ticket {
                            match parse_ticket(t) {
                                Ok(addr) => boots.push(addr),
                                Err(e) => app.status = format!("ticket: {e}"),
                            }
                        }
                        enter_chat(app, room, boots).await;
                    }
                    Err(e) => app.status = format!("/join failed: {e}"),
                },
                Err(e) => app.status = e.to_string(),
            }
        }
        "/pin" => {
            if let Some(room) = &app.room {
                let pin = room.lock().await.pin.display();
                app.status = format!("pin {pin}  — not on teams.");
            } else {
                app.status = "no session open".into();
            }
        }
        "/ticket" => {
            if let Some(t) = &app.ticket {
                app.status = format!("ticket {t}");
            } else {
                app.status = "no ticket yet".into();
            }
        }
        "/peers" => {
            app.status = format!("{} peer(s) in overlay", app.peer_count);
        }
        "/forget" => {
            if let Some(room) = &app.room {
                let topic = crate::crypto::topic_id(&room.lock().await.pin);
                app.shutdown_net().await;
                app.dir.forget(&topic)?;
                app.refresh_sessions();
                app.screen = Screen::Home;
                app.status = "session wiped from this machine".into();
            } else if let Some(s) = app.sessions.get(app.selected) {
                if let Ok(bytes) = data_encoding::HEXLOWER.decode(s.topic.as_bytes()) {
                    if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                        app.dir.forget(&arr)?;
                        app.refresh_sessions();
                        app.status = "session wiped from this machine".into();
                    }
                }
            }
        }
        "/help" => {
            app.status =
                "/new /join /pin /ticket /peers /forget /quit   esc = leave session".into();
        }
        other if other.starts_with('/') => {
            app.status = format!("unknown command {other}  — /help");
        }
        _ => {}
    }
    Ok(false)
}

async fn enter_chat(app: &mut App, room: OpenRoom, bootstrap: Vec<EndpointAddr>) {
    let secret = room.secret.clone();
    let shared = Arc::new(Mutex::new(room));
    let (tx, rx) = mpsc::unbounded_channel();
    app.events_rx = Some(rx);
    app.room = Some(shared.clone());
    app.screen = Screen::Chat;
    app.scroll = 0;
    app.peer_count = 0;
    match NetSession::start(secret, shared, tx, bootstrap).await {
        Ok(net) => {
            app.ticket = Some(net.addr());
            app.net = Some(net);
            if app.status.starts_with("/new") || app.status.is_empty() {
                app.status = "lan session up".into();
            }
        }
        Err(e) => {
            app.status = format!("offline mode (net failed: {e})");
        }
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
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
    match &app.screen {
        Screen::Home => draw_home(f, chunks[1], app),
        Screen::Unlock { alias, .. } => draw_unlock(f, chunks[1], alias),
        Screen::Chat => draw_chat(f, chunks[1], app),
    }
    draw_status(f, chunks[2], app);
    draw_input(f, chunks[3], app);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = match &app.screen {
        Screen::Home => format!("  local-llm  {}", env!("CARGO_PKG_VERSION")),
        Screen::Unlock { alias, .. } => format!("  local-llm  {alias}  locked"),
        Screen::Chat => {
            let alias = app
                .room
                .as_ref()
                .and_then(|r| r.try_lock().ok().map(|g| g.alias()))
                .unwrap_or_else(|| "session".into());
            format!("  local-llm  {alias}  {} peers", app.peer_count)
        }
    };
    let p = Paragraph::new(title).style(
        Style::default()
            .fg(Color::Rgb(180, 200, 170))
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(p, area);
}

fn draw_home(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled(
        "  sessions",
        Style::default().fg(Color::DarkGray),
    ))];
    if app.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  no local sessions.  /new gpt-oss-20b",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (i, s) in app.sessions.iter().enumerate() {
        let marker = if i == app.selected { ">" } else { " " };
        let style = if i == app.selected {
            Style::default().fg(Color::Rgb(200, 220, 190))
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(
            format!("  {marker} {:<22} locked", s.alias),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  /new <name>    /join <pin> [ticket]    /forget    /quit",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_unlock(f: &mut ratatui::Frame, area: Rect, alias: &str) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  unlock {alias}"),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "  enter pin  (7K2M-9QXP)",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_chat(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(room) = &app.room {
        if let Ok(room) = room.try_lock() {
            for rec in room.log.records() {
                match rec {
                    Record::Meta { alias } => {
                        lines.push(Line::from(Span::styled(
                            "  system",
                            Style::default().fg(Color::DarkGray),
                        )));
                        lines.push(Line::from(Span::styled(
                            format!("  session {alias}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                        lines.push(Line::from(""));
                    }
                    Record::Chat { author, body, .. } => {
                        let role = room.role_of(author);
                        let color = if role == "user" {
                            Color::Rgb(160, 190, 220)
                        } else {
                            Color::Rgb(180, 210, 170)
                        };
                        lines.push(Line::from(Span::styled(
                            format!("  {role}"),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        )));
                        for para in body.split('\n') {
                            lines.push(Line::from(Span::styled(
                                format!("  {para}"),
                                Style::default().fg(Color::Rgb(220, 220, 220)),
                            )));
                        }
                        lines.push(Line::from(""));
                    }
                }
            }
        }
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    f.render_widget(p, area);
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, app: &App) {
    f.render_widget(
        Paragraph::new(format!("  {}", app.status)).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let mask = matches!(app.screen, Screen::Unlock { .. });
    let shown = if mask {
        "•".repeat(app.input.chars().count())
    } else {
        app.input.clone()
    };
    let p = Paragraph::new(format!("  > {shown}"))
        .style(Style::default().fg(Color::Rgb(200, 200, 190)))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Rgb(50, 55, 50))),
        );
    f.render_widget(p, area);
    let col = 4u16.saturating_add(shown.chars().count() as u16);
    f.set_cursor_position((area.x.saturating_add(col), area.y.saturating_add(1)));
}
