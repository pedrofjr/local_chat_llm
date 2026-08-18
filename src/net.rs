use crate::crypto::{topic_hex, topic_id};
use crate::room::OpenRoom;
use crate::store::Record;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use iroh_gossip::api::{Event, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Bumped for the v2 record set. An older build simply fails to connect,
/// which is a better failure than half-understood traffic.
pub const SYNC_ALPN: &[u8] = b"local-llm/2";

/// Peer addresses from previous sessions, sealed with the room key inside the
/// room folder. Without this, coming back to a room means asking a colleague
/// for a ticket even though the room itself is already on disk.
const PEERS_FILE: &str = "peers.bin";
/// A room holds four people; the extra room is for machines that changed
/// address. Keeping every address ever seen would only slow the retries down.
const REMEMBERED_MAX: usize = 16;

pub fn load_peers(room: &OpenRoom) -> Vec<EndpointAddr> {
    room.log
        .read_side(PEERS_FILE)
        .and_then(|bytes| postcard::from_bytes::<Vec<EndpointAddr>>(&bytes).ok())
        .unwrap_or_default()
}

fn save_peers(room: &OpenRoom, peers: &[EndpointAddr]) {
    let keep: Vec<EndpointAddr> = peers.iter().take(REMEMBERED_MAX).cloned().collect();
    if let Ok(bytes) = postcard::to_stdvec(&keep) {
        let _ = room.log.write_side(PEERS_FILE, &bytes);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SyncMsg {
    Have {
        heads: BTreeMap<[u8; 32], u64>,
    },
    /// Records travel as opaque bytes so one entry this build cannot parse
    /// costs that entry, not the whole batch.
    Give {
        records: Vec<Vec<u8>>,
    },
}

#[derive(Debug, Clone)]
pub enum NetEvent {
    Status(String),
    /// Full roster of live neighbours, so the UI can name who joined or left.
    Peers(Vec<EndpointId>),
    Record,
    Ticket(String),
}

#[derive(Clone)]
struct SyncState {
    room: Arc<Mutex<OpenRoom>>,
}

impl fmt::Debug for SyncState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncState").finish_non_exhaustive()
    }
}

fn io_err(e: impl fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl ProtocolHandler for SyncState {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let req = recv.read_to_end(64 * 1024).await.map_err(io_err)?;
        let msg: SyncMsg = postcard::from_bytes(&req).map_err(io_err)?;
        if let SyncMsg::Have { heads } = msg {
            let missing: Vec<Vec<u8>> = {
                let room = self.room.lock().await;
                room.log
                    .missing_for(&heads)
                    .iter()
                    .filter_map(|rec| rec.encode().ok())
                    .collect()
            };
            let reply = postcard::to_stdvec(&SyncMsg::Give { records: missing }).map_err(io_err)?;
            send.write_all(&reply).await.map_err(io_err)?;
            send.finish()?;
        }
        connection.closed().await;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Presence {
    pub dir: PathBuf,
    pub instance: u8,
}

pub struct NetSession {
    router: Router,
    gossip_tx: GossipSender,
    endpoint: Endpoint,
    presence: Presence,
    known: Arc<Mutex<Vec<EndpointAddr>>>,
}

impl NetSession {
    pub async fn start(
        secret: SecretKey,
        room: Arc<Mutex<OpenRoom>>,
        events: mpsc::UnboundedSender<NetEvent>,
        bootstrap: Vec<EndpointAddr>,
        presence: Presence,
    ) -> Result<Self> {
        let pin = { room.lock().await.pin.clone() };
        let topic_bytes = topic_id(&pin);
        let topic = TopicId::from_bytes(topic_bytes);
        let topic_hex = topic_hex(&topic_bytes);

        let memory = MemoryLookup::new();
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .secret_key(secret)
            .address_lookup(MdnsAddressLookup::builder())
            .address_lookup(memory.clone())
            .bind()
            .await
            .context("bind iroh endpoint")?;

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let sync = SyncState { room: room.clone() };
        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(SYNC_ALPN, sync)
            .spawn();

        wait_for_addrs(&endpoint).await;
        let me = endpoint.id();
        let my_addr = localize_addr(endpoint.addr());
        let ticket = encode_ticket(&my_addr);
        let _ = events.send(NetEvent::Ticket(ticket.clone()));
        write_presence(&presence, &topic_hex, &ticket);

        let mut bootstrap: Vec<EndpointAddr> = bootstrap
            .into_iter()
            .filter(|a| a.id != me)
            .map(localize_addr)
            .collect();
        bootstrap.extend(scan_presence(&presence, &topic_hex, me));
        // Everyone this room has met before. Their addresses may well be stale
        // -- DHCP moves machines around -- but the endpoint id still lets mDNS
        // resolve them, and a dead address just fails fast.
        let remembered = { load_peers(&*room.lock().await) };
        for addr in remembered {
            if addr.id != me && !bootstrap.iter().any(|a| a.id == addr.id) {
                bootstrap.push(localize_addr(addr));
            }
        }
        for addr in &bootstrap {
            memory.add_endpoint_info(addr.clone());
        }
        let peer_ids: Vec<EndpointId> = bootstrap.iter().map(|a| a.id).collect();

        if peer_ids.is_empty() {
            let _ = events.send(NetEvent::Status(
                "waiting for peers (same pc + lan). /ticket if they can't see you.".into(),
            ));
        } else {
            let _ = events.send(NetEvent::Status(format!(
                "reaching {} known peer(s)…",
                peer_ids.len()
            )));
        }

        let topic_handle = gossip.subscribe(topic, peer_ids).await?;
        let (sender, receiver) = topic_handle.split();

        let known = Arc::new(Mutex::new(bootstrap));
        tokio::spawn(gossip_loop(receiver, room.clone(), events.clone()));
        tokio::spawn(sync_loop(
            endpoint.clone(),
            room.clone(),
            events.clone(),
            known.clone(),
        ));
        tokio::spawn(presence_loop(
            endpoint.clone(),
            sender.clone(),
            memory,
            presence.clone(),
            topic_hex,
            known.clone(),
            events,
            room,
        ));

        Ok(Self {
            router,
            gossip_tx: sender,
            endpoint,
            presence,
            known,
        })
    }

    /// Addresses we have a route to, whether or not gossip has them live.
    /// A non-zero count here with zero live peers means discovery worked and
    /// the connection did not — worth telling the user apart.
    pub async fn known_count(&self) -> usize {
        self.known.lock().await.len()
    }

    pub async fn broadcast(&self, rec: &Record) -> Result<()> {
        let bytes = postcard::to_stdvec(rec)?;
        self.gossip_tx.broadcast(Bytes::from(bytes)).await?;
        Ok(())
    }

    pub fn addr(&self) -> String {
        encode_ticket(&self.endpoint.addr())
    }

    pub async fn shutdown(self) -> Result<()> {
        remove_presence(&self.presence);
        self.router.shutdown().await?;
        Ok(())
    }
}

async fn gossip_loop(
    mut receiver: iroh_gossip::api::GossipReceiver,
    room: Arc<Mutex<OpenRoom>>,
    events: mpsc::UnboundedSender<NetEvent>,
) {
    let mut peers: HashSet<EndpointId> = HashSet::new();
    while let Some(event) = receiver.next().await {
        let Ok(event) = event else { continue };
        match event {
            Event::Received(msg) => {
                if let Ok(rec) = postcard::from_bytes::<Record>(&msg.content) {
                    let mut room = room.lock().await;
                    if let Ok(true) = room.ingest(rec) {
                        let _ = events.send(NetEvent::Record);
                    }
                }
            }
            Event::NeighborUp(id) => {
                peers.insert(id);
                let _ = events.send(NetEvent::Peers(peers.iter().copied().collect()));
            }
            Event::NeighborDown(id) => {
                peers.remove(&id);
                let _ = events.send(NetEvent::Peers(peers.iter().copied().collect()));
            }
            _ => {}
        }
    }
}

async fn sync_loop(
    endpoint: Endpoint,
    room: Arc<Mutex<OpenRoom>>,
    events: mpsc::UnboundedSender<NetEvent>,
    known: Arc<Mutex<Vec<EndpointAddr>>>,
) {
    let mut ticks = tokio::time::interval(Duration::from_secs(3));
    loop {
        ticks.tick().await;
        let peers = { known.lock().await.clone() };
        if peers.is_empty() {
            continue;
        }
        let heads = { room.lock().await.log.heads() };
        let req = match postcard::to_stdvec(&SyncMsg::Have { heads }) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let me = endpoint.id();
        for addr in peers.into_iter().filter(|a| a.id != me) {
            if let Ok(conn) = endpoint.connect(addr, SYNC_ALPN).await {
                if let Ok((mut send, mut recv)) = conn.open_bi().await {
                    if send.write_all(&req).await.is_ok() && send.finish().is_ok() {
                        if let Ok(buf) = recv.read_to_end(2 * 1024 * 1024).await {
                            if let Ok(SyncMsg::Give { records }) = postcard::from_bytes(&buf) {
                                let mut room = room.lock().await;
                                for raw in records {
                                    let Ok(rec) = Record::decode(&raw) else {
                                        continue;
                                    };
                                    if let Ok(true) = room.ingest(rec) {
                                        let _ = events.send(NetEvent::Record);
                                    }
                                }
                            }
                        }
                    }
                }
                conn.close(0u32.into(), b"ok");
            }
        }
    }
}

async fn wait_for_addrs(endpoint: &Endpoint) {
    for _ in 0..30 {
        if !endpoint.addr().addrs.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn localize_addr(mut addr: EndpointAddr) -> EndpointAddr {
    let mut extra = Vec::new();
    for item in &addr.addrs {
        if let TransportAddr::Ip(sa) = item {
            if sa.ip().is_unspecified() {
                extra.push(TransportAddr::Ip(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    sa.port(),
                )));
            }
        }
    }
    if !addr
        .addrs
        .iter()
        .any(|a| matches!(a, TransportAddr::Ip(sa) if sa.ip().is_loopback()))
    {
        if let Some(port) = addr.addrs.iter().find_map(|a| match a {
            TransportAddr::Ip(sa) => Some(sa.port()),
            _ => None,
        }) {
            extra.push(TransportAddr::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
            )));
        }
    }
    addr.addrs.extend(extra);
    addr
}

fn presence_path(presence: &Presence) -> PathBuf {
    presence.dir.join(format!("{}.txt", presence.instance))
}

fn write_presence(presence: &Presence, topic: &str, ticket: &str) {
    if fs::create_dir_all(&presence.dir).is_err() {
        return;
    }
    let _ = fs::write(presence_path(presence), format!("{topic}\n{ticket}\n"));
}

fn remove_presence(presence: &Presence) {
    let _ = fs::remove_file(presence_path(presence));
}

fn scan_presence(presence: &Presence, topic: &str, me: EndpointId) -> Vec<EndpointAddr> {
    let Ok(entries) = fs::read_dir(&presence.dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        if path.file_stem().and_then(|s| s.to_str()) == Some(&presence.instance.to_string()) {
            continue;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if modified
                    .elapsed()
                    .map(|d| d > Duration::from_secs(30))
                    .unwrap_or(true)
                {
                    continue;
                }
            }
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut lines = text.lines();
        let Some(file_topic) = lines.next().map(str::trim) else {
            continue;
        };
        let Some(ticket) = lines.next().map(str::trim) else {
            continue;
        };
        if file_topic != topic || ticket.is_empty() {
            continue;
        }
        if let Ok(addr) = parse_ticket(ticket) {
            if addr.id != me {
                out.push(localize_addr(addr));
            }
        }
    }
    out
}

/// Ticks (of two seconds) to wait before knocking on remembered addresses
/// again. Grows while nobody answers so an empty room does not hammer the
/// network all day, and resets the moment somebody turns up.
const RETRY_BACKOFF: [u32; 5] = [1, 3, 8, 15, 30];

/// Next backoff step and how many ticks to wait for it. Saturates at the top
/// so an empty room settles into one attempt a minute instead of stopping.
fn next_backoff(step: usize) -> (usize, u32) {
    let step = (step + 1).min(RETRY_BACKOFF.len() - 1);
    (step, RETRY_BACKOFF[step])
}

#[allow(clippy::too_many_arguments)]
async fn presence_loop(
    endpoint: Endpoint,
    gossip_tx: GossipSender,
    memory: MemoryLookup,
    presence: Presence,
    topic_hex: String,
    known: Arc<Mutex<Vec<EndpointAddr>>>,
    events: mpsc::UnboundedSender<NetEvent>,
    room: Arc<Mutex<OpenRoom>>,
) {
    let me = endpoint.id();
    let mut ticks = tokio::time::interval(Duration::from_secs(2));
    let mut step = 0usize;
    let mut countdown = 0u32;
    let mut saved = { load_peers(&*room.lock().await).len() };

    loop {
        ticks.tick().await;
        let ticket = encode_ticket(&localize_addr(endpoint.addr()));
        write_presence(&presence, &topic_hex, &ticket);

        // Other windows on this machine, which mDNS cannot see.
        let mut added = Vec::new();
        {
            let mut known = known.lock().await;
            for addr in scan_presence(&presence, &topic_hex, me) {
                memory.add_endpoint_info(addr.clone());
                if !known.iter().any(|a| a.id == addr.id) {
                    known.push(addr.clone());
                    added.push(addr.id);
                }
            }
        }
        if !added.is_empty() {
            let _ = gossip_tx.join_peers(added).await;
            let n = known.lock().await.len();
            let _ = events.send(NetEvent::Status(format!(
                "found peer on this pc, connecting… ({n} known)"
            )));
            step = 0;
            countdown = 0;
        }

        let roster = { known.lock().await.clone() };

        // Remember whoever we have a route to, so the next session can come
        // back without anyone reading a ticket out loud.
        if roster.len() != saved {
            saved = roster.len();
            save_peers(&*room.lock().await, &roster);
        }

        if countdown > 0 {
            countdown -= 1;
            continue;
        }
        let targets: Vec<EndpointId> = roster.iter().map(|a| a.id).filter(|id| *id != me).collect();
        if !targets.is_empty() {
            for addr in &roster {
                memory.add_endpoint_info(addr.clone());
            }
            // Already-connected peers are simply ignored by gossip, so this is
            // safe to repeat.
            let _ = gossip_tx.join_peers(targets.clone()).await;
            let _ = events.send(NetEvent::Status(format!(
                "reaching {} known peer(s)…",
                targets.len()
            )));
        }
        (step, countdown) = next_backoff(step);
    }
}

pub fn short_id(id: &EndpointId) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

pub fn encode_ticket(addr: &EndpointAddr) -> String {
    let bytes = postcard::to_stdvec(addr).unwrap_or_default();
    data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase()
}

pub fn parse_ticket(s: &str) -> Result<EndpointAddr> {
    let bytes = data_encoding::BASE32_NOPAD
        .decode(s.trim().to_ascii_uppercase().as_bytes())
        .context("invalid peer ticket")?;
    postcard::from_bytes(&bytes).context("invalid peer ticket")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Pin;
    use crate::store::DataDir;
    use tempfile::TempDir;

    fn any_addr() -> EndpointAddr {
        EndpointAddr {
            id: SecretKey::generate().public(),
            addrs: Default::default(),
        }
    }

    #[test]
    fn remembered_peers_come_back_and_are_capped() {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let pin = Pin::parse("7K2M-9QXP").unwrap();
        let room = OpenRoom::join(&dir, pin, Some("sala")).unwrap();

        assert!(load_peers(&room).is_empty(), "a fresh room knows nobody");

        let seen: Vec<EndpointAddr> = (0..REMEMBERED_MAX + 9).map(|_| any_addr()).collect();
        save_peers(&room, &seen);

        let back = load_peers(&room);
        assert_eq!(back.len(), REMEMBERED_MAX, "the list must not grow forever");
        assert_eq!(back[0].id, seen[0].id, "the oldest known peers are kept");
        assert_eq!(back[REMEMBERED_MAX - 1].id, seen[REMEMBERED_MAX - 1].id);
    }

    #[test]
    fn backoff_grows_and_then_holds() {
        let (mut step, mut wait) = next_backoff(0);
        assert_eq!(wait, RETRY_BACKOFF[1]);
        let mut seen = vec![wait];
        for _ in 0..10 {
            (step, wait) = next_backoff(step);
            seen.push(wait);
        }
        assert!(
            seen.windows(2).all(|pair| pair[1] >= pair[0]),
            "waits must never shrink on their own, got {seen:?}"
        );
        assert_eq!(
            *seen.last().unwrap(),
            *RETRY_BACKOFF.last().unwrap(),
            "and settle at the ceiling rather than growing without bound"
        );
    }

    #[test]
    fn presence_scan_finds_other_instance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = Presence {
            dir: tmp.path().to_path_buf(),
            instance: 1,
        };
        let b = Presence {
            dir: tmp.path().to_path_buf(),
            instance: 2,
        };
        let key = SecretKey::generate();
        let addr = EndpointAddr {
            id: key.public(),
            addrs: Default::default(),
        };
        let ticket = encode_ticket(&addr);
        write_presence(&a, "abc", &ticket);
        let found = scan_presence(&b, "abc", SecretKey::generate().public());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, addr.id);
        assert!(scan_presence(&b, "other", SecretKey::generate().public()).is_empty());
    }
}
