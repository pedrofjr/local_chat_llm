use crate::crypto::topic_id;
use crate::room::OpenRoom;
use crate::store::Record;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_gossip::api::Event;
use iroh_gossip::{Gossip, TopicId};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub const SYNC_ALPN: &[u8] = b"local-llm/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SyncMsg {
    Have { heads: BTreeMap<[u8; 32], u64> },
    Give { records: Vec<Record> },
}

#[derive(Debug, Clone)]
pub enum NetEvent {
    Status(String),
    Peers(usize),
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
            let missing = {
                let room = self.room.lock().await;
                room.log.missing_for(&heads)
            };
            let reply = postcard::to_stdvec(&SyncMsg::Give { records: missing }).map_err(io_err)?;
            send.write_all(&reply).await.map_err(io_err)?;
            send.finish()?;
        }
        connection.closed().await;
        Ok(())
    }
}

pub struct NetSession {
    router: Router,
    gossip_tx: iroh_gossip::api::GossipSender,
    endpoint: Endpoint,
}

impl NetSession {
    pub async fn start(
        secret: SecretKey,
        room: Arc<Mutex<OpenRoom>>,
        events: mpsc::UnboundedSender<NetEvent>,
        bootstrap: Vec<EndpointAddr>,
    ) -> Result<Self> {
        let pin = { room.lock().await.pin.clone() };
        let topic = TopicId::from_bytes(topic_id(&pin));

        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .secret_key(secret)
            .address_lookup(MdnsAddressLookup::builder())
            .bind()
            .await
            .context("bind iroh endpoint")?;

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let sync = SyncState { room: room.clone() };
        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(SYNC_ALPN, sync)
            .spawn();

        let ticket = encode_ticket(&endpoint.addr());
        let _ = events.send(NetEvent::Ticket(ticket));

        let peer_ids: Vec<EndpointId> = bootstrap.iter().map(|a| a.id).collect();

        let topic_handle = if peer_ids.is_empty() {
            let _ = events.send(NetEvent::Status(
                "waiting for peers on lan (mdns). /ticket if they can't see you.".into(),
            ));
            gossip.subscribe(topic, vec![]).await?
        } else {
            let _ = events.send(NetEvent::Status(format!(
                "dialing {} bootstrap peer(s)…",
                peer_ids.len()
            )));
            gossip.subscribe_and_join(topic, peer_ids.clone()).await?
        };
        let (sender, receiver) = topic_handle.split();

        tokio::spawn(gossip_loop(receiver, room.clone(), events.clone()));
        tokio::spawn(sync_loop(
            endpoint.clone(),
            room.clone(),
            events.clone(),
            bootstrap,
        ));

        Ok(Self {
            router,
            gossip_tx: sender,
            endpoint,
        })
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
                let _ = events.send(NetEvent::Peers(peers.len()));
                let _ = events.send(NetEvent::Status(format!("peer up {}", short_id(&id))));
            }
            Event::NeighborDown(id) => {
                peers.remove(&id);
                let _ = events.send(NetEvent::Peers(peers.len()));
            }
            _ => {}
        }
    }
}

async fn sync_loop(
    endpoint: Endpoint,
    room: Arc<Mutex<OpenRoom>>,
    events: mpsc::UnboundedSender<NetEvent>,
    known: Vec<EndpointAddr>,
) {
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(4));
    loop {
        ticks.tick().await;
        if known.is_empty() {
            continue;
        }
        let heads = { room.lock().await.log.heads() };
        let req = match postcard::to_stdvec(&SyncMsg::Have { heads }) {
            Ok(b) => b,
            Err(_) => continue,
        };
        for addr in known.clone() {
            if let Ok(conn) = endpoint.connect(addr, SYNC_ALPN).await {
                if let Ok((mut send, mut recv)) = conn.open_bi().await {
                    if send.write_all(&req).await.is_ok() && send.finish().is_ok() {
                        if let Ok(buf) = recv.read_to_end(2 * 1024 * 1024).await {
                            if let Ok(SyncMsg::Give { records }) = postcard::from_bytes(&buf) {
                                let mut room = room.lock().await;
                                for rec in records {
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

fn short_id(id: &EndpointId) -> String {
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
