//! A fake robot that speaks the link protocol, for testing the plane and MCP tools
//! without hardware. Behaves like the firmware's `LinkService` + `RemoteState`.

use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::Result;
use bbproto::ctrl::Res;
use bbproto::frame::{MediaHeader, MicChunk, VideoFrame};
use bbproto::{Frame, FrameType, ToRobot};
use bytes::{Bytes, BytesMut};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

struct Sim {
    remote: bool,
    camera: Option<(Res, u8, u8)>,
    mic: bool,
    drive_until: Option<Instant>,
    speaking_until: Option<Instant>,
    spk_samples: usize,
    started: Instant,
    seq: u32,
}

fn ctrl(v: Value) -> Frame {
    Frame::new(FrameType::Ctrl, serde_json::to_vec(&v).unwrap())
}

/// Run against the plane at `addr` (UDP) until it has been silent for a while.
pub async fn run(addr: &str) -> Result<()> {
    run_lossy(addr, 0.0).await
}

/// Tiny deterministic RNG for simulated packet loss.
struct Lossy {
    state: u64,
    loss: f64,
}

impl Lossy {
    fn lose(&mut self) -> bool {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.state >> 33) as f64 / (1u64 << 31) as f64) < self.loss
    }
}

/// Like `run`, dropping a fraction `loss` of datagrams in each direction (like a weak Wi-Fi link).
pub async fn run_lossy(addr: &str, loss: f64) -> Result<()> {
    let tx_loss = Arc::new(Mutex::new(Lossy { state: 1, loss }));
    let mut rx_loss = Lossy { state: 2, loss };
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    sock.connect(addr).await?;
    let (tx, mut rx) = mpsc::channel::<Frame>(256);
    // Replies by request id, like the firmware: retransmitted requests get the same reply.
    let replies: Arc<Mutex<HashMap<u32, Option<Frame>>>> = Default::default();
    let writer = {
        let sock = sock.clone();
        let replies = replies.clone();
        let tx_loss = tx_loss.clone();
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if f.ty == FrameType::Ctrl
                    && let Ok(v) = serde_json::from_slice::<Value>(&f.payload)
                    && (v["t"] == "ack" || v["t"] == "error")
                    && let Some(id) = v["id"].as_u64()
                {
                    replies.lock().unwrap().insert(id as u32, Some(f.clone()));
                }
                if tx_loss.lock().unwrap().lose() {
                    continue;
                }
                let _ = sock.send(&f.to_bytes()).await;
            }
        })
    };

    let hello = ctrl(
        json!({"t":"hello","proto":bbproto::PROTO_VERSION,"fw":"bbsim","mac":"00:00:00:00:00:00","caps":["camera","mic","speaker","drive","sim"]}),
    );

    let mut s = Sim {
        remote: false,
        camera: None,
        mic: false,
        drive_until: None,
        speaking_until: None,
        spk_samples: 0,
        started: Instant::now(),
        seq: 0,
    };
    let mut buf = vec![0u8; 65536];
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    let mut next_frame = Instant::now();
    let mut next_mic = Instant::now();
    let mut next_hello = Instant::now();
    let mut heard = false;
    let mut last_rx = Instant::now();
    let mut phase = 0f32;

    let result = loop {
        tokio::select! {
            r = sock.recv(&mut buf) => {
                let Ok(n) = r else { continue };
                if rx_loss.lose() {
                    continue;
                }
                let mut b = BytesMut::from(&buf[..n]);
                let Ok(Some(f)) = Frame::decode(&mut b) else { continue };
                last_rx = Instant::now();
                if !heard {
                    heard = true;
                    tx.send(hello.clone()).await?;
                    tx.send(ctrl(json!({"t":"mode","mode":"idle"}))).await?;
                    tx.send(ctrl(json!({"t":"battery","pct":87.0,"level":5,"charging":"unplugged"}))).await?;
                }
                // De-duplicate retransmitted requests.
                if f.ty == FrameType::Ctrl
                    && let Ok(msg) = serde_json::from_slice::<ToRobot>(&f.payload)
                    && !matches!(msg, ToRobot::Ping { .. })
                    && let Some(id) = msg.id()
                {
                    let cached = replies.lock().unwrap().get(&id).cloned();
                    match cached {
                        Some(Some(reply)) => { tx.send(reply).await?; continue; }
                        Some(None) => continue,
                        None => { replies.lock().unwrap().insert(id, None); }
                    }
                }
                handle(&mut s, f, &tx).await?;
            }
            _ = tick.tick() => {
                let now = Instant::now();
                if !heard {
                    if now >= next_hello {
                        next_hello = now + Duration::from_secs(1);
                        let _ = sock.send(&hello.to_bytes()).await;
                    }
                    continue;
                }
                if last_rx.elapsed() > Duration::from_secs(8) {
                    break Ok(());
                }
                if let Some(t) = s.drive_until && now >= t {
                    s.drive_until = None;
                    tx.send(ctrl(json!({"t":"drive_stopped","reason":"watchdog"}))).await?;
                }
                if let Some(t) = s.speaking_until && now >= t {
                    s.speaking_until = None;
                    tx.send(ctrl(json!({"t":"speaker","playing":false}))).await?;
                }
                if let Some((res, fps, q)) = s.camera && s.remote && now >= next_frame {
                    next_frame = now + Duration::from_millis(1000 / fps.max(1) as u64);
                    s.seq += 1;
                    for f in video(&s, res, q, false, s.seq).to_datagrams() {
                        let _ = tx.try_send(f);
                    }
                }
                if s.mic && s.remote && now >= next_mic {
                    next_mic += Duration::from_millis(32);
                    if next_mic < now { next_mic = now + Duration::from_millis(32); }
                    let gated = s.speaking_until.is_some();
                    let samples: Vec<i16> = (0..bbproto::frame::MIC_SAMPLES_PER_DATAGRAM).map(|_| {
                        phase += 2.0 * std::f32::consts::PI * 440.0 / 16000.0;
                        if gated { 0 } else { (phase.sin() * 6000.0) as i16 }
                    }).collect();
                    let c = MicChunk { header: MediaHeader { seq: s.seq, ts_ms: s.started.elapsed().as_millis() as u32 }, samples };
                    let _ = tx.try_send(c.to_frame());
                }
            }
        }
    };
    writer.abort();
    result
}

async fn handle(s: &mut Sim, f: Frame, tx: &mpsc::Sender<Frame>) -> Result<()> {
    match f.ty {
        FrameType::SpkPcm => {
            s.spk_samples += f.payload.len() / 2;
            return Ok(());
        }
        FrameType::Ctrl => {}
        _ => return Ok(()),
    }
    let msg: ToRobot = match serde_json::from_slice(&f.payload) {
        Ok(m) => m,
        Err(e) => {
            tx.send(ctrl(
                json!({"t":"log","level":"warn","msg":format!("bad ctrl: {e}")}),
            ))
            .await?;
            return Ok(());
        }
    };
    let ack = |id: u32, data: Value| ctrl(json!({"t":"ack","id":id,"data":data}));
    let gated = !matches!(
        msg,
        ToRobot::Ping { .. } | ToRobot::EnterRemote { .. } | ToRobot::ExitRemote { .. }
    );
    if gated && !s.remote {
        if let Some(id) = msg.id() {
            tx.send(ctrl(
                json!({"t":"error","id":id,"msg":"not in remote mode"}),
            ))
            .await?;
        }
        return Ok(());
    }
    match msg {
        ToRobot::Hello => {
            tx.send(ctrl(json!({"t":"hello","proto":bbproto::PROTO_VERSION,"fw":"bbsim","mac":"00:00:00:00:00:00","caps":["camera","mic","speaker","drive","sim"]}))).await?;
            tx.send(ctrl(
                json!({"t":"mode","mode": if s.remote { "remote" } else { "idle" }}),
            ))
            .await?;
        }
        ToRobot::Ping { id, ts } => tx.send(ctrl(json!({"t":"pong","id":id,"ts":ts}))).await?,
        ToRobot::EnterRemote { id } => {
            s.remote = true;
            tx.send(ack(id, Value::Null)).await?;
            tx.send(ctrl(json!({"t":"mode","mode":"remote"}))).await?;
        }
        ToRobot::ExitRemote { id } => {
            s.remote = false;
            s.camera = None;
            s.mic = false;
            tx.send(ack(id, Value::Null)).await?;
            tx.send(ctrl(json!({"t":"mode","mode":"idle"}))).await?;
        }
        ToRobot::Drive { ms, .. } => {
            if s.remote {
                s.drive_until = Some(Instant::now() + Duration::from_millis(ms as u64));
            }
        }
        ToRobot::Stop => s.drive_until = None,
        ToRobot::Say { id, text } => {
            tx.send(ack(id, Value::Null)).await?;
            tx.send(ctrl(json!({"t":"speaker","playing":true}))).await?;
            s.speaking_until =
                Some(Instant::now() + Duration::from_millis(60 * text.len() as u64 + 200));
        }
        ToRobot::SpkBegin { id } => {
            s.spk_samples = 0;
            tx.send(ack(id, Value::Null)).await?;
            tx.send(ctrl(json!({"t":"speaker","playing":true}))).await?;
        }
        ToRobot::SpkEnd => {
            let ms = (s.spk_samples as u64 * 1000 / 16000).min(300);
            s.speaking_until = Some(Instant::now() + Duration::from_millis(ms));
        }
        ToRobot::SpkAbort => {
            s.speaking_until = Some(Instant::now());
        }
        ToRobot::Volume { .. } => {}
        ToRobot::Camera {
            id,
            on,
            res,
            fps,
            quality,
            ..
        } => {
            s.camera = on.then_some((res, fps, quality));
            tx.send(ack(id, Value::Null)).await?;
        }
        ToRobot::Snapshot { id, res, quality } => {
            tx.send(ack(id, Value::Null)).await?;
            for f in video(s, res, quality, true, id).to_datagrams() {
                tx.send(f).await?;
            }
        }
        ToRobot::Mic { id, on, .. } => {
            s.mic = on;
            tx.send(ack(id, Value::Null)).await?;
        }
        ToRobot::Led { id, .. } => tx.send(ack(id, Value::Null)).await?,
        ToRobot::StatusLight(_)
        | ToRobot::AiStatus { .. }
        | ToRobot::HomeInfo { .. }
        | ToRobot::PeekCaption { .. } => {}
        ToRobot::Phone { id, .. } => {
            tx.send(ack(
                id,
                json!({"connected": false, "notifications": [], "total": 0, "next": null}),
            ))
            .await?
        }
        ToRobot::Scenario { id, scenario, .. } => {
            tx.send(ctrl(
                json!({"t":"bb","state":2,"action":scenario,"data":""}),
            ))
            .await?;
            // The firmware acks when the routine has finished.
            tx.send(ack(id, json!({"done": true}))).await?;
        }
        ToRobot::IrList { id } => tx.send(ack(id, json!({"commands": []}))).await?,
        ToRobot::IrSend { id, .. } => {
            tx.send(ctrl(
                json!({"t":"error","id":id,"msg":"no IR module inserted"}),
            ))
            .await?
        }
        ToRobot::Safety { id, .. } => tx.send(ack(id, Value::Null)).await?,
        ToRobot::Restart { id } => tx.send(ack(id, Value::Null)).await?,
        ToRobot::Ota { id, .. } => {
            tx.send(ctrl(
                json!({"t":"error","id":id,"msg":"the simulator has no firmware to update"}),
            ))
            .await?
        }
        ToRobot::Modules { id } => {
            tx.send(ack(
                id,
                json!({"modules": [{"bus":0,"type":"temp_hum","temperature_c":22,"humidity":40}]}),
            ))
            .await?
        }
    }
    Ok(())
}

/// A test card: colour bars with a moving stripe so motion is visible.
fn video(s: &Sim, res: Res, quality: u8, snapshot: bool, seq: u32) -> VideoFrame {
    let (w, h) = res.size();
    let t = s.started.elapsed().as_millis() as u32;
    let stripe = (t / 20) % w as u32;
    let img = image::RgbImage::from_fn(w as u32, h as u32, |x, y| {
        if x.abs_diff(stripe) < 4 {
            return image::Rgb([255, 255, 255]);
        }
        let bar = x * 7 / w as u32;
        let c = [
            [230, 200, 30],
            [30, 200, 220],
            [40, 190, 60],
            [210, 40, 200],
            [220, 40, 40],
            [40, 60, 220],
            [20, 20, 20],
        ][bar as usize];
        let shade = 255 - (y * 80 / h as u32) as u8;
        image::Rgb([
            (c[0] as u16 * shade as u16 / 255) as u8,
            (c[1] as u16 * shade as u16 / 255) as u8,
            (c[2] as u16 * shade as u16 / 255) as u8,
        ])
    });
    let mut jpeg = Vec::new();
    // Firmware quality is 0..63 lower-is-better; map roughly to 1..100 higher-is-better.
    let q = (100u32.saturating_sub(quality as u32 * 3 / 2)).clamp(10, 95) as u8;
    image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), q)
        .encode_image(&img)
        .expect("jpeg encode");
    VideoFrame {
        header: MediaHeader { seq, ts_ms: t },
        snapshot,
        width: w,
        height: h,
        jpeg: Bytes::from(jpeg),
    }
}
