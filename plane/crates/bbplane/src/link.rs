//! UDP link to the robot, like the stock RC video feed: fire-and-forget datagrams, one frame
//! each. The robot announces itself with `hello` until it hears from us; we ping it every
//! second and consider it gone after a few seconds of silence. Lost datagrams are simply lost:
//! video frames are dropped, requests are retransmitted by the hub until acked.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bbproto::{Frame, ToRobot};
use bytes::BytesMut;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::hub::{Hub, now_ms};

const PING_INTERVAL: Duration = Duration::from_secs(1);
/// No datagram from the robot for this long means it is gone.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(8);

struct Peer {
    addr: SocketAddr,
    generation: u64,
    last_rx: Instant,
}

pub async fn serve(hub: Arc<Hub>, sock: UdpSocket) -> Result<()> {
    tracing::info!("robot link listening on udp {}", sock.local_addr()?);
    let sock = Arc::new(sock);
    let mut peer: Option<Peer> = None;
    let mut buf = vec![0u8; 65536];
    let mut ping = tokio::time::interval(PING_INTERVAL);

    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let (n, from) = match r {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("udp recv: {e}");
                        continue;
                    }
                };
                let mut b = BytesMut::from(&buf[..n]);
                let frame = match Frame::decode(&mut b) {
                    Ok(Some(f)) => f,
                    _ => continue, // not one of ours
                };
                if peer.as_ref().map(|p| p.addr) != Some(from) {
                    if let Some(old) = peer.take() {
                        tracing::info!("robot moved from {} to {from}", old.addr);
                        hub.detach(old.generation).await;
                    }
                    peer = Some(attach(&hub, &sock, from).await);
                    // Answer right away so the robot stops announcing itself, and ask who it is
                    // (it may have been connected to a previous plane process).
                    let id = hub.next_id();
                    hub.send(ToRobot::Ping { id, ts: now_ms() }).await.ok();
                    hub.send(ToRobot::Hello).await.ok();
                }
                if let Some(p) = peer.as_mut() {
                    p.last_rx = Instant::now();
                }
                hub.handle_frame(frame).await;
            }
            _ = ping.tick() => {
                if let Some(p) = &peer {
                    if p.last_rx.elapsed() > SILENCE_TIMEOUT {
                        tracing::info!("robot {} silent for {SILENCE_TIMEOUT:?}", p.addr);
                        hub.detach(p.generation).await;
                        peer = None;
                        continue;
                    }
                    let id = hub.next_id();
                    hub.send(ToRobot::Ping { id, ts: now_ms() }).await.ok();
                    // Until it has said who it is: the first request may have been lost, and
                    // without its mode the agent never starts.
                    if hub.status.lock().await.fw.is_none() {
                        hub.send(ToRobot::Hello).await.ok();
                    }
                }
            }
        }
    }
}

async fn attach(hub: &Arc<Hub>, sock: &Arc<UdpSocket>, addr: SocketAddr) -> Peer {
    tracing::info!("robot at {addr}");
    let (tx, mut rx) = mpsc::channel::<Frame>(256);
    let generation = hub.attach(addr.to_string(), tx).await;
    let sock = sock.clone();
    tokio::spawn(async move {
        // Ends when the hub drops its sender (detach or a newer robot).
        while let Some(f) = rx.recv().await {
            if let Err(e) = sock.send_to(&f.to_bytes(), addr).await {
                tracing::debug!("udp send to {addr}: {e}");
            }
        }
    });
    Peer {
        addr,
        generation,
        last_rx: Instant::now(),
    }
}
