//! Shared state between the robot link, the MCP server and the web UI.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use bbproto::frame::{MicChunk, VideoFrame};
use bbproto::{Frame, FrameType, FromRobot, ToRobot};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};

use crate::config::Config;

const EVENT_LOG_LEN: usize = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// See `Hub::heal_speaker_flag`.
const SPEAKER_STUCK: Duration = Duration::from_secs(20);
/// Resend an unanswered request this often (lost datagrams).
const RETRANSMIT: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    /// Unix time in milliseconds.
    pub ts: u64,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RobotStatus {
    pub connected: bool,
    pub peer: Option<String>,
    pub fw: Option<String>,
    pub mac: Option<String>,
    pub caps: Vec<String>,
    /// `idle`, `listen`, `scenario`, `rc` or `remote`.
    pub mode: Option<String>,
    pub battery: Option<Value>,
    pub proximity: Option<Value>,
    pub speaker_playing: bool,
    /// When `speaker_playing` last turned on.
    #[serde(skip)]
    pub speaker_since: Option<std::time::Instant>,
    pub rtt_ms: Option<u64>,
    pub camera_streaming: bool,
    pub mic_streaming: bool,
    pub video_frames: u64,
    /// Video frames lost in transit (some datagrams never arrived).
    pub video_dropped: u64,
    pub mic_chunks: u64,
    /// Robot's Wi-Fi signal strength in dBm.
    pub wifi_rssi: Option<i32>,
    /// Robot's free internal RAM and its largest free block, in bytes.
    pub ram_free: Option<u32>,
    pub ram_block: Option<u32>,
}

struct Conn {
    /// Generation counter so a stale link task cannot tear down a newer one.
    generation: u64,
    tx: mpsc::Sender<Frame>,
}

pub struct Hub {
    /// When speaker audio last went to the robot (Unix ms).
    last_speaker_tx: std::sync::atomic::AtomicU64,
    pub config: Config,
    conn: Mutex<Option<Conn>>,
    generation: AtomicU32,
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<Result<Value, String>>>>,
    snapshots: Mutex<HashMap<u32, oneshot::Sender<VideoFrame>>>,
    pub status: Mutex<RobotStatus>,
    events: Mutex<VecDeque<Event>>,
    event_seq: Mutex<u64>,
    event_notify: Notify,
    pub video: broadcast::Sender<Arc<VideoFrame>>,
    latest_frame: Mutex<Option<Arc<VideoFrame>>>,
    pub mic: broadcast::Sender<Arc<Vec<i16>>>,
    mic_history: Mutex<VecDeque<i16>>,
    /// When a robot in remote mode last dropped off; it keeps retrying on its own for a while.
    pub remote_lost_at: Mutex<Option<tokio::time::Instant>>,
    video_assembler: Mutex<bbproto::VideoAssembler>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Hub {
    pub fn new(config: Config) -> Arc<Hub> {
        Arc::new(Hub {
            last_speaker_tx: Default::default(),
            config,
            conn: Mutex::new(None),
            generation: AtomicU32::new(0),
            // Random start: a restarted plane must not reuse ids the robot still remembers.
            next_id: AtomicU32::new((now_ms() as u32 & 0x7fff_ffff) | 1),
            pending: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            status: Mutex::new(RobotStatus::default()),
            events: Mutex::new(VecDeque::new()),
            event_seq: Mutex::new(0),
            event_notify: Notify::new(),
            video: broadcast::channel(8).0,
            latest_frame: Mutex::new(None),
            mic: broadcast::channel(64).0,
            mic_history: Mutex::new(VecDeque::new()),
            remote_lost_at: Mutex::new(None),
            video_assembler: Mutex::new(bbproto::VideoAssembler::default()),
        })
    }

    pub fn next_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ---- connection lifecycle (called by the link) ----

    pub async fn attach(&self, peer: String, tx: mpsc::Sender<Frame>) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) as u64 + 1;
        *self.conn.lock().await = Some(Conn { generation, tx });
        {
            let mut s = self.status.lock().await;
            *s = RobotStatus {
                connected: true,
                peer: Some(peer.clone()),
                ..Default::default()
            };
        }
        self.push_event("link", json!({ "connected": true, "peer": peer }))
            .await;
        generation
    }

    pub async fn detach(&self, generation: u64) {
        let mut conn = self.conn.lock().await;
        if conn.as_ref().map(|c| c.generation) != Some(generation) {
            return;
        }
        *conn = None;
        drop(conn);
        {
            let mut s = self.status.lock().await;
            s.connected = false;
            if s.mode.as_deref() == Some("remote") {
                *self.remote_lost_at.lock().await = Some(tokio::time::Instant::now());
            }
        }
        for (_, tx) in self.pending.lock().await.drain() {
            let _ = tx.send(Err("robot disconnected".into()));
        }
        self.snapshots.lock().await.clear();
        // A frame from before the link dropped must not pass for what the camera sees now.
        *self.latest_frame.lock().await = None;
        self.push_event("link", json!({ "connected": false })).await;
    }

    pub async fn is_connected(&self) -> bool {
        self.conn.lock().await.is_some()
    }

    // ---- outgoing ----

    /// The robot reports its speaker stopping once, by datagram; if that is lost the flag stays on
    /// (the face keeps talking, sound recognition stays paused). Turn it off when it has been on for
    /// a while and nothing has been sent to the speaker lately.
    pub async fn heal_speaker_flag(&self) {
        let quiet_for = now_ms().saturating_sub(self.last_speaker_tx.load(Ordering::Relaxed));
        let mut st = self.status.lock().await;
        if st.speaker_playing
            && st.speaker_since.is_some_and(|t| t.elapsed() >= SPEAKER_STUCK)
            && quiet_for >= SPEAKER_STUCK.as_millis() as u64
        {
            tracing::info!("speaker still marked playing with nothing sent to it: clearing it");
            st.speaker_playing = false;
        }
    }

    pub async fn send_frame(&self, frame: Frame) -> Result<()> {
        let tx = {
            let conn = self.conn.lock().await;
            conn.as_ref()
                .map(|c| c.tx.clone())
                .ok_or_else(|| anyhow!("robot is not connected"))?
        };
        if frame.ty == FrameType::SpkPcm {
            self.last_speaker_tx.store(now_ms(), Ordering::Relaxed);
        }
        tx.send(frame)
            .await
            .map_err(|_| anyhow!("robot link closed"))
    }

    /// Fire-and-forget control message.
    pub async fn send(&self, msg: ToRobot) -> Result<()> {
        self.send_frame(Frame::ctrl(&msg)).await
    }

    /// Control message that expects `ack`/`error` with the same id.
    pub async fn request(&self, msg: ToRobot) -> Result<Value> {
        self.request_timeout(msg, REQUEST_TIMEOUT).await
    }

    pub async fn request_timeout(&self, msg: ToRobot, timeout: Duration) -> Result<Value> {
        let id = msg.id().ok_or_else(|| anyhow!("message has no id"))?;
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // UDP: resend until answered. The robot runs each id once and repeats its reply.
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Err(e) = self.send(msg.clone()).await {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }
            let next = (tokio::time::Instant::now() + RETRANSMIT).min(deadline);
            match tokio::time::timeout_at(next, &mut rx).await {
                Ok(Ok(Ok(v))) => return Ok(v),
                Ok(Ok(Err(msg))) => bail!("robot: {msg}"),
                Ok(Err(_)) => bail!("robot disconnected"),
                Err(_) if tokio::time::Instant::now() < deadline => continue,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    bail!("robot did not answer within {timeout:?}")
                }
            }
        }
    }

    /// Request a still frame and wait for it.
    pub async fn snapshot(&self, res: bbproto::ctrl::Res, quality: u8) -> Result<VideoFrame> {
        // Over UDP a frame whose fragments got lost never completes, and a retransmitted request
        // only repeats the ack. So take a fresh snapshot (new id) until one arrives whole.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut last_err = anyhow!("snapshot timed out");
        while tokio::time::Instant::now() < deadline {
            let id = self.next_id();
            let (tx, rx) = oneshot::channel();
            self.snapshots.lock().await.insert(id, tx);
            let r = async {
                // The robot acks once the frame is captured; the frame follows right after, in
                // well under a second when it arrives at all. Retry soon rather than wait long.
                self.request_timeout(
                    ToRobot::Snapshot { id, res, quality },
                    Duration::from_secs(15),
                )
                .await?;
                tokio::time::timeout(Duration::from_millis(1500), rx)
                    .await
                    .map_err(|_| anyhow!("snapshot timed out"))?
                    .map_err(|_| anyhow!("robot disconnected"))
            }
            .await;
            self.snapshots.lock().await.remove(&id);
            match r {
                Ok(f) => return Ok(f),
                // A robot-side error (bad resolution, no memory) will not fix itself.
                Err(e) if e.to_string().starts_with("robot:") || !self.is_connected().await => {
                    return Err(e);
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    // ---- incoming (called by the link) ----

    pub async fn handle_frame(&self, frame: Frame) {
        match frame.ty {
            FrameType::Ctrl => match FromRobot::parse(&frame.payload) {
                Ok(Ok(msg)) => self.handle_ctrl(msg).await,
                Ok(Err(raw)) => self.push_event("unknown", raw).await,
                Err(e) => tracing::warn!("bad ctrl json from robot: {e}"),
            },
            FrameType::Video => match bbproto::VideoFragment::parse(&frame) {
                Ok(f) => {
                    let (done, dropped) = {
                        let mut a = self.video_assembler.lock().await;
                        let done = a.push(f);
                        (done, a.dropped)
                    };
                    self.status.lock().await.video_dropped = dropped;
                    if let Some(v) = done {
                        self.handle_video(v).await;
                    }
                }
                Err(e) => tracing::debug!("bad video fragment: {e}"),
            },
            FrameType::MicPcm => match MicChunk::parse(&frame) {
                Ok(c) => self.handle_mic(c).await,
                Err(e) => tracing::warn!("bad mic frame: {e}"),
            },
            FrameType::SpkPcm => tracing::warn!("robot sent a speaker frame, ignoring"),
        }
    }

    async fn handle_ctrl(&self, msg: FromRobot) {
        match msg {
            FromRobot::Ack { id, data } => {
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(Ok(data));
                }
            }
            FromRobot::Error { id, msg } => {
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(Err(msg.clone()));
                } else {
                    self.push_event("error", json!({ "id": id, "msg": msg }))
                        .await;
                }
            }
            FromRobot::Pong { id, ts } => {
                self.status.lock().await.rtt_ms = Some(now_ms().saturating_sub(ts));
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(Ok(Value::Null));
                }
            }
            FromRobot::Hello {
                proto,
                fw,
                mac,
                caps,
            } => {
                {
                    let mut s = self.status.lock().await;
                    s.fw = Some(fw.clone());
                    s.mac = Some(mac.clone());
                    s.caps = caps.clone();
                }
                if proto != bbproto::PROTO_VERSION {
                    tracing::warn!(
                        "robot speaks protocol {proto}, plane {}",
                        bbproto::PROTO_VERSION
                    );
                }
                self.push_event(
                    "hello",
                    json!({ "proto": proto, "fw": fw, "mac": mac, "caps": caps }),
                )
                .await;
            }
            FromRobot::Mode { mode } => {
                {
                    let mut s = self.status.lock().await;
                    s.mode = Some(mode.clone());
                    if mode != "remote" {
                        s.camera_streaming = false;
                        s.mic_streaming = false;
                    }
                }
                if mode != "remote" {
                    *self.latest_frame.lock().await = None;
                }
                self.push_event("mode", json!({ "mode": mode })).await;
            }
            FromRobot::Bb {
                state,
                action,
                data,
            } => {
                let raw = decode_hex(&data);
                let name = bbproto::bb::describe(state, action);
                let decoded = bbproto::bb::decode(state, action, &raw);
                self.push_event("bb", json!({ "name": name, "state": state, "action": action, "data": decoded.unwrap_or(Value::String(data)) }))
                    .await;
            }
            FromRobot::Battery {
                pct,
                level,
                charging,
            } => {
                let v = json!({ "pct": pct, "level": level, "charging": charging });
                self.status.lock().await.battery = Some(v.clone());
                self.push_event("battery", v).await;
            }
            FromRobot::Proximity { front, bottom } => {
                let v = json!({ "front": front, "bottom": bottom });
                self.status.lock().await.proximity = Some(v.clone());
                self.push_event("proximity", v).await;
            }
            FromRobot::Speaker {
                playing,
                buffered_ms,
                underruns,
            } => {
                {
                    let mut st = self.status.lock().await;
                    if playing && !st.speaker_playing {
                        st.speaker_since = Some(std::time::Instant::now());
                    }
                    st.speaker_playing = playing;
                }
                self.push_event("speaker", json!({ "playing": playing, "buffered_ms": buffered_ms, "underruns": underruns }))
                    .await;
            }
            FromRobot::Motion { kind } => self.push_event("motion", json!({ "kind": kind })).await,
            FromRobot::DriveStopped { reason } => {
                self.push_event("drive_stopped", json!({ "reason": reason }))
                    .await
            }
            FromRobot::PhoneLink { connected } => {
                self.push_event("phone_link", json!({ "connected": connected }))
                    .await
            }
            FromRobot::Remote {
                button,
                on,
                talking,
                scenario,
                arg,
                command,
            } => {
                self.push_event(
                    "remote",
                    json!({ "button": button, "on": on, "talking": talking, "scenario": scenario, "arg": arg, "command": command }),
                )
                .await
            }
            FromRobot::PhoneCall { from } => {
                self.push_event("phone_call", json!({ "from": from })).await
            }
            FromRobot::Wifi {
                rssi,
                ram_free,
                ram_block,
            } => {
                let mut s = self.status.lock().await;
                s.wifi_rssi = Some(rssi);
                s.ram_free = ram_free.or(s.ram_free);
                s.ram_block = ram_block.or(s.ram_block);
            }
            FromRobot::Button { action } => {
                self.push_event("button", json!({ "action": action })).await
            }
            FromRobot::Log { level, msg } => {
                tracing::info!(target: "robot", "[{level}] {msg}");
            }
        }
    }

    async fn handle_video(&self, v: VideoFrame) {
        if v.snapshot {
            if let Some(tx) = self.snapshots.lock().await.remove(&v.header.seq) {
                let _ = tx.send(v);
            }
            return;
        }
        let v = Arc::new(v);
        {
            let mut s = self.status.lock().await;
            s.video_frames += 1;
            s.camera_streaming = true;
        }
        *self.latest_frame.lock().await = Some(v.clone());
        let _ = self.video.send(v);
    }

    async fn handle_mic(&self, c: MicChunk) {
        // The firmware high-pass filters the microphone (MicInput::getMono).
        {
            let mut s = self.status.lock().await;
            s.mic_chunks += 1;
            s.mic_streaming = true;
        }
        let cap = self.config.mic.history_secs as usize * bbproto::SAMPLE_RATE as usize;
        {
            let mut h = self.mic_history.lock().await;
            h.extend(c.samples.iter().copied());
            let excess = h.len().saturating_sub(cap);
            h.drain(..excess);
        }
        let _ = self.mic.send(Arc::new(c.samples));
    }

    // ---- queries ----

    pub async fn latest_frame(&self) -> Option<Arc<VideoFrame>> {
        self.latest_frame.lock().await.clone()
    }

    /// The last `secs` seconds of microphone audio.
    pub async fn mic_history(&self, secs: f32) -> Vec<i16> {
        let n = (secs * bbproto::SAMPLE_RATE as f32) as usize;
        let h = self.mic_history.lock().await;
        h.iter().skip(h.len().saturating_sub(n)).copied().collect()
    }

    pub async fn push_event(&self, kind: &str, data: Value) {
        let seq = {
            let mut s = self.event_seq.lock().await;
            *s += 1;
            *s
        };
        let ev = Event {
            seq,
            ts: now_ms(),
            kind: kind.into(),
            data,
        };
        tracing::debug!(?ev, "event");
        {
            let mut log = self.events.lock().await;
            log.push_back(ev);
            while log.len() > EVENT_LOG_LEN {
                log.pop_front();
            }
        }
        self.event_notify.notify_waiters();
    }

    /// Events with `seq > since`. Waits up to `wait` for the first one if none are queued.
    pub async fn events_since(&self, since: u64, wait: Duration) -> Vec<Event> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.event_notify.notified();
            let out: Vec<Event> = self
                .events
                .lock()
                .await
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect();
            if !out.is_empty() || tokio::time::Instant::now() >= deadline {
                return out;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Vec::new();
            }
        }
    }

    /// The most recent logged event that `pick` accepts.
    pub async fn last_event(&self, pick: impl Fn(&Event) -> bool) -> Option<Event> {
        self.events
            .lock()
            .await
            .iter()
            .rev()
            .find(|e| pick(e))
            .cloned()
    }

    pub async fn last_event_seq(&self) -> u64 {
        *self.event_seq.lock().await
    }

    /// Wait until an event matching `pred` arrives after `since`, or the timeout.
    pub async fn wait_event(
        &self,
        since: u64,
        timeout: Duration,
        pred: impl Fn(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut cursor = since;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let evs = self.events_since(cursor, left).await;
            for e in &evs {
                if pred(e) {
                    return Some(e.clone());
                }
            }
            if let Some(last) = evs.last() {
                cursor = last.seq;
            }
        }
    }
}

fn decode_hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_gets_ack() {
        let hub = Hub::new(Config::default());
        let (tx, mut rx) = mpsc::channel(8);
        let generation = hub.attach("test".into(), tx).await;
        let h2 = hub.clone();
        let task = tokio::spawn(async move { h2.request(ToRobot::Modules { id: 42 }).await });
        let f = rx.recv().await.unwrap();
        assert_eq!(f.ty, FrameType::Ctrl);
        hub.handle_frame(Frame::new(
            FrameType::Ctrl,
            &br#"{"t":"ack","id":42,"data":{"x":1}}"#[..],
        ))
        .await;
        assert_eq!(task.await.unwrap().unwrap()["x"], 1);

        // Disconnect fails outstanding requests.
        let h2 = hub.clone();
        let task = tokio::spawn(async move { h2.request(ToRobot::Modules { id: 43 }).await });
        let _ = rx.recv().await.unwrap();
        hub.detach(generation).await;
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn events_long_poll() {
        let hub = Hub::new(Config::default());
        let h2 = hub.clone();
        let waiter = tokio::spawn(async move { h2.events_since(0, Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.push_event("motion", json!({"kind":"shake"})).await;
        let evs = waiter.await.unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, "motion");
        assert!(hub.events_since(1, Duration::ZERO).await.is_empty());
    }

    #[test]
    fn hex() {
        assert_eq!(decode_hex("0401ff"), vec![4, 1, 255]);
    }
}
