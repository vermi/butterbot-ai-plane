//! The link to the plane (`bbplane`): UDP, one frame per datagram (see `bbproto::frame`).
//!
//! The robot says hello until the plane answers (it pings every second); silence for 8 s means
//! the plane is gone. Requests with an id are retransmitted by the plane until answered: each id
//! runs once here, and its reply is sent again for a repeat. Control messages go out before
//! microphone audio, and both before video; a video frame's datagrams let control messages
//! through between them. Live video keeps only the newest frame.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bbproto::frame::{
    FrameType, FLAG_SNAPSHOT, HEADER_LEN, MAGIC, MAX_DATAGRAM_PAYLOAD, MIC_SAMPLES_PER_DATAGRAM,
};
use bbproto::{FromRobot, ToRobot};
use serde_json::Value;

use crate::net::Net;
use crate::store::LinkConfig;
use crate::util::{self, millis, sleep_ms, Stack};

const HELLO_MS: u64 = 1000;
/// The plane pings every second; this much silence means it is gone.
const SILENCE_MS: u64 = 8000;
const REPLIES: usize = 32;
const MIC_QUEUE: usize = 24;
const CTRL_QUEUE: usize = 64;
const VIDEO_HEADER: usize = 16;

pub enum LinkEvent {
    Online,
    Offline,
    /// The plane (re)started and asks who we are.
    PlaneHello,
    Request(ToRobot),
}

pub type SpeakerSink = Box<dyn Fn(&[u8]) + Send>;

struct Video {
    flags: u8,
    /// seq u32, ts u32, w u16, h u16: repeated in every datagram.
    head: [u8; 12],
    jpeg: Vec<u8>,
}

enum Item {
    Datagram(Vec<u8>),
    Video(Video),
}

#[derive(Default)]
struct Queues {
    ctrl: VecDeque<Item>,
    mic: VecDeque<Vec<u8>>,
    video: Option<Video>,
}

#[derive(Clone)]
struct Reply {
    id: u32,
    json: Option<Vec<u8>>,
}

pub struct Link {
    sock: Mutex<Option<Arc<UdpSocket>>>,
    target: Mutex<Option<SocketAddr>>,
    config: Mutex<Option<LinkConfig>>,
    wanted: AtomicBool,
    online: AtomicBool,
    queues: Mutex<Queues>,
    wake: Condvar,
    replies: Mutex<(Vec<Reply>, usize)>,
    hello: Vec<u8>,
    mic_seq: AtomicU32,
    speaker: Mutex<Option<SpeakerSink>>,
}

fn header(out: &mut Vec<u8>, ty: FrameType, flags: u8, len: usize) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(ty as u8);
    out.push(flags);
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

fn ctrl_datagram(json: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(HEADER_LEN + json.len());
    header(&mut d, FrameType::Ctrl, 0, json.len());
    d.extend_from_slice(json);
    d
}

impl Link {
    pub fn new(fw: &str, caps: &[&str]) -> Arc<Link> {
        let hello = serde_json::to_vec(&FromRobot::Hello {
            proto: bbproto::PROTO_VERSION,
            fw: fw.into(),
            build: util::build_id(),
            mac: util::mac(),
            caps: caps.iter().map(|c| c.to_string()).collect(),
        })
        .unwrap_or_default();
        Arc::new(Link {
            sock: Mutex::new(None),
            target: Mutex::new(None),
            config: Mutex::new(None),
            wanted: AtomicBool::new(false),
            online: AtomicBool::new(false),
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
            replies: Mutex::new((vec![Reply { id: 0, json: None }; REPLIES], 0)),
            hello,
            mic_seq: AtomicU32::new(0),
            speaker: Mutex::new(None),
        })
    }

    pub fn start<E: From<LinkEvent> + Send + 'static>(
        self: &Arc<Self>,
        net: Arc<Net>,
        tx: Sender<E>,
    ) {
        let l = self.clone();
        util::spawn(c"link_rx", 8192, 8, Stack::Internal, move || {
            l.rx_loop(&net, &tx)
        });
        let l = self.clone();
        util::spawn(c"link_tx", 6144, 7, Stack::Internal, move || l.tx_loop());
    }

    pub fn configure(&self, cfg: LinkConfig) {
        *self.config.lock().unwrap() = Some(cfg);
        // Reconnect to the (possibly new) plane address.
        *self.sock.lock().unwrap() = None;
    }

    /// Dial the plane (Wi-Fi must be up), or hang up.
    pub fn set_wanted(&self, on: bool) {
        self.wanted.store(on, Ordering::Relaxed);
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// The plane's IPv4 address (network byte order), once known.
    pub fn plane_ip(&self) -> Option<std::net::Ipv4Addr> {
        match *self.target.lock().unwrap() {
            Some(SocketAddr::V4(a)) => Some(*a.ip()),
            _ => None,
        }
    }

    pub fn set_speaker_sink(&self, sink: Option<SpeakerSink>) {
        *self.speaker.lock().unwrap() = sink;
    }

    // ---------------------------------------------------------------- receiving

    fn open(&self) -> Option<Arc<UdpSocket>> {
        let cfg = self.config.lock().unwrap().clone()?;
        let addr = match (cfg.host.as_str(), cfg.port).to_socket_addrs() {
            Ok(mut a) => a.find(|a| a.is_ipv4())?,
            Err(e) => {
                log::warn!("link: cannot resolve {}: {e}", cfg.host);
                return None;
            }
        };
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                log::error!("link: socket: {e}");
                return None;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
        let sock = Arc::new(sock);
        *self.target.lock().unwrap() = Some(addr);
        *self.sock.lock().unwrap() = Some(sock.clone());
        self.clear_replies();
        log::info!("link: dialing plane {addr}");
        Some(sock)
    }

    fn go_offline<E: From<LinkEvent>>(&self, tx: &Sender<E>) {
        if self.online.swap(false, Ordering::Relaxed) {
            log::warn!("link: plane lost");
            self.clear_replies();
            self.drain();
            let _ = tx.send(LinkEvent::Offline.into());
        }
    }

    fn rx_loop<E: From<LinkEvent>>(&self, net: &Net, tx: &Sender<E>) {
        let mut buf = vec![0u8; 2048];
        let mut next_hello = 0;
        let mut last_rx = 0;
        loop {
            if !self.wanted.load(Ordering::Relaxed) || !net.has_ip() {
                // Let queued replies (an exit_remote ack, say) go out first.
                if self.is_online() && !self.wanted.load(Ordering::Relaxed) {
                    for _ in 0..20 {
                        if self.queues.lock().unwrap().ctrl.is_empty() {
                            break;
                        }
                        sleep_ms(10);
                    }
                }
                self.go_offline(tx);
                *self.sock.lock().unwrap() = None;
                sleep_ms(50);
                continue;
            }
            let existing = self.sock.lock().unwrap().clone();
            let sock = match existing {
                Some(s) => s,
                None => match self.open() {
                    Some(s) => {
                        next_hello = 0;
                        s
                    }
                    None => {
                        sleep_ms(1000);
                        continue;
                    }
                },
            };
            let target = *self.target.lock().unwrap();
            let now = millis();
            if !self.is_online() && now >= next_hello {
                next_hello = now + HELLO_MS;
                if let Some(t) = target {
                    let _ = sock.send_to(&ctrl_datagram(&self.hello), t);
                }
            }
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if Some(from.ip()) != target.map(|t| t.ip()) || n < HEADER_LEN {
                        continue;
                    }
                    let d = &buf[..n];
                    if u16::from_le_bytes([d[0], d[1]]) != MAGIC {
                        continue;
                    }
                    let len = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
                    if len > n - HEADER_LEN {
                        continue;
                    }
                    last_rx = millis();
                    if !self.online.swap(true, Ordering::Relaxed) {
                        log::info!("link: plane answered");
                        let _ = tx.send(LinkEvent::Online.into());
                    }
                    let payload = &d[HEADER_LEN..HEADER_LEN + len];
                    match FrameType::try_from(d[2]) {
                        Ok(FrameType::Ctrl) => self.handle_ctrl(payload, tx),
                        Ok(FrameType::SpkPcm) => {
                            if let Some(sink) = self.speaker.lock().unwrap().as_ref() {
                                sink(payload);
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    // Timeout (or a broken socket: closed below).
                }
            }
            if self.is_online() && millis() - last_rx > SILENCE_MS {
                self.go_offline(tx);
            }
        }
    }

    fn handle_ctrl<E: From<LinkEvent>>(&self, payload: &[u8], tx: &Sender<E>) {
        let Ok(v) = serde_json::from_slice::<Value>(payload) else {
            log::warn!("link: unparseable control message");
            return;
        };
        let t = v["t"].as_str().unwrap_or("").to_string();
        let id = v["id"].as_u64().unwrap_or(0) as u32;
        match t.as_str() {
            "hello" => {
                let _ = tx.send(LinkEvent::PlaneHello.into());
                return;
            }
            "ping" => {
                self.send(&FromRobot::Pong {
                    id,
                    ts: v["ts"].as_u64().unwrap_or(0),
                });
                return;
            }
            _ => {}
        }
        if id != 0 {
            let cached = {
                let r = self.replies.lock().unwrap();
                r.0.iter().find(|r| r.id == id).map(|r| r.json.clone())
            };
            match cached {
                Some(Some(json)) => {
                    self.enqueue_ctrl(ctrl_datagram(&json));
                    return;
                }
                Some(None) => return, // still running
                None => {
                    let mut r = self.replies.lock().unwrap();
                    let at = r.1;
                    r.0[at] = Reply { id, json: None };
                    r.1 = (at + 1) % REPLIES;
                }
            }
        }
        match serde_json::from_value::<ToRobot>(v) {
            Ok(msg) => {
                let _ = tx.send(LinkEvent::Request(msg).into());
            }
            Err(e) => {
                log::warn!("link: request \"{t}\": {e}");
                if id != 0 {
                    self.error(id, "unknown request");
                }
            }
        }
    }

    fn clear_replies(&self) {
        let mut r = self.replies.lock().unwrap();
        for e in r.0.iter_mut() {
            *e = Reply { id: 0, json: None };
        }
    }

    // ---------------------------------------------------------------- sending

    fn enqueue_ctrl(&self, d: Vec<u8>) {
        if !self.is_online() {
            return;
        }
        let mut q = self.queues.lock().unwrap();
        if q.ctrl.len() >= CTRL_QUEUE {
            log::warn!("link: control queue full, dropping");
            return;
        }
        q.ctrl.push_back(Item::Datagram(d));
        self.wake.notify_one();
    }

    pub fn send_hello(&self) {
        self.enqueue_ctrl(ctrl_datagram(&self.hello));
    }

    pub fn send(&self, msg: &FromRobot) {
        if !self.is_online() {
            return;
        }
        match serde_json::to_vec(msg) {
            Ok(json) if json.len() <= MAX_DATAGRAM_PAYLOAD => {
                self.enqueue_ctrl(ctrl_datagram(&json))
            }
            Ok(json) => log::warn!(
                "link: control message too large ({} bytes), dropped",
                json.len()
            ),
            Err(e) => log::warn!("link: {e}"),
        }
    }

    fn remember(&self, id: u32, json: &[u8]) {
        let mut r = self.replies.lock().unwrap();
        if let Some(e) = r.0.iter_mut().find(|e| e.id == id) {
            e.json = Some(json.to_vec());
        }
    }

    pub fn ack(&self, id: u32, data: Value) {
        let json = serde_json::to_vec(&FromRobot::Ack { id, data }).unwrap_or_default();
        self.remember(id, &json);
        if json.len() <= MAX_DATAGRAM_PAYLOAD {
            self.enqueue_ctrl(ctrl_datagram(&json));
        }
    }

    pub fn error(&self, id: u32, msg: &str) {
        let json = serde_json::to_vec(&FromRobot::Error {
            id,
            msg: msg.into(),
        })
        .unwrap_or_default();
        self.remember(id, &json);
        self.enqueue_ctrl(ctrl_datagram(&json));
    }

    /// Queue a JPEG frame. Live frames replace one not yet sent; snapshots are never dropped.
    pub fn send_video(&self, jpeg: Vec<u8>, w: u16, h: u16, seq: u32, snapshot: bool) -> bool {
        if !self.is_online() {
            return false;
        }
        let mut head = [0u8; 12];
        head[0..4].copy_from_slice(&seq.to_le_bytes());
        head[4..8].copy_from_slice(&(millis() as u32).to_le_bytes());
        head[8..10].copy_from_slice(&w.to_le_bytes());
        head[10..12].copy_from_slice(&h.to_le_bytes());
        let v = Video {
            flags: if snapshot { FLAG_SNAPSHOT } else { 0 },
            head,
            jpeg,
        };
        let mut q = self.queues.lock().unwrap();
        if snapshot {
            q.ctrl.push_back(Item::Video(v));
        } else {
            q.video = Some(v);
        }
        self.wake.notify_one();
        true
    }

    /// A live frame is still waiting to go out (the camera can skip a capture).
    pub fn video_pending(&self) -> bool {
        self.queues.lock().unwrap().video.is_some()
    }

    pub fn send_mic(&self, samples: &[i16]) {
        if !self.is_online() {
            return;
        }
        let mut q = self.queues.lock().unwrap();
        for chunk in samples.chunks(MIC_SAMPLES_PER_DATAGRAM) {
            let payload = 8 + chunk.len() * 2;
            let mut d = Vec::with_capacity(HEADER_LEN + payload);
            header(&mut d, FrameType::MicPcm, 0, payload);
            let seq = self.mic_seq.fetch_add(1, Ordering::Relaxed) + 1;
            d.extend_from_slice(&seq.to_le_bytes());
            d.extend_from_slice(&(millis() as u32).to_le_bytes());
            for s in chunk {
                d.extend_from_slice(&s.to_le_bytes());
            }
            if q.mic.len() >= MIC_QUEUE {
                q.mic.pop_front();
            }
            q.mic.push_back(d);
        }
        self.wake.notify_one();
    }

    fn drain(&self) {
        let mut q = self.queues.lock().unwrap();
        q.ctrl.clear();
        q.mic.clear();
        q.video = None;
    }

    fn tx_loop(&self) {
        loop {
            let item = {
                let mut q = self.queues.lock().unwrap();
                loop {
                    if let Some(i) = q.ctrl.pop_front() {
                        break i;
                    }
                    if let Some(d) = q.mic.pop_front() {
                        break Item::Datagram(d);
                    }
                    if let Some(v) = q.video.take() {
                        break Item::Video(v);
                    }
                    q = self.wake.wait(q).unwrap();
                }
            };
            let Some(sock) = self.sock.lock().unwrap().clone() else {
                continue;
            };
            let Some(target) = *self.target.lock().unwrap() else {
                continue;
            };
            match item {
                Item::Datagram(d) => {
                    send_raw(&sock, &d, target, 20);
                }
                Item::Video(v) => {
                    let snapshot = v.flags & FLAG_SNAPSHOT != 0;
                    self.send_video_datagrams(&sock, target, &v, !snapshot);
                }
            }
        }
    }

    fn send_video_datagrams(
        &self,
        sock: &UdpSocket,
        target: SocketAddr,
        v: &Video,
        interleave: bool,
    ) {
        let chunk = MAX_DATAGRAM_PAYLOAD - VIDEO_HEADER;
        let frags = v.jpeg.len().div_ceil(chunk).max(1);
        let mut d = Vec::with_capacity(HEADER_LEN + MAX_DATAGRAM_PAYLOAD);
        for (i, part) in v.jpeg.chunks(chunk).enumerate() {
            d.clear();
            header(&mut d, FrameType::Video, v.flags, VIDEO_HEADER + part.len());
            d.extend_from_slice(&v.head);
            d.extend_from_slice(&(i as u16).to_le_bytes());
            d.extend_from_slice(&(frags as u16).to_le_bytes());
            d.extend_from_slice(part);
            if !send_raw(sock, &d, target, 100) {
                return; // the plane drops the incomplete frame
            }
            if interleave {
                // Control replies must not wait behind a whole frame.
                loop {
                    let next = {
                        let mut q = self.queues.lock().unwrap();
                        match q.ctrl.front() {
                            Some(Item::Datagram(_)) => q.ctrl.pop_front(),
                            _ => None,
                        }
                    };
                    match next {
                        Some(Item::Datagram(c)) => {
                            send_raw(sock, &c, target, 20);
                        }
                        _ => break,
                    }
                }
            }
        }
    }
}

/// Send one datagram, waiting up to `wait_ms` for a free Wi-Fi buffer.
fn send_raw(sock: &UdpSocket, d: &[u8], target: SocketAddr, wait_ms: u64) -> bool {
    let deadline = millis() + wait_ms;
    loop {
        match sock.send_to(d, target) {
            Ok(n) if n == d.len() => return true,
            Ok(_) => return false,
            Err(e) => {
                let busy = matches!(e.raw_os_error(), Some(12) | Some(11));
                if !busy || millis() >= deadline {
                    return false;
                }
                sleep_ms(1);
            }
        }
    }
}
