//! Firmware updates over Wi-Fi. `bbplane ota <image>` hands the image to the running plane
//! (`POST /api/ota`), which serves it over HTTP on a port of its own for the length of the update,
//! tells the robot where it is (`ToRobot::Ota`), follows its progress and waits for it to come back
//! on the new firmware. The robot only keeps the new firmware once it has reached the plane again;
//! if it cannot, its bootloader goes back to the previous one at the next restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bbproto::ToRobot;
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::control::Robot;

/// The robot's app slots (firmware-rs/partitions.csv).
const SLOT_SIZE: usize = 8000 * 1024;
/// ESP-IDF image chip id of the ESP32-S3.
const CHIP_ESP32S3: u16 = 9;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// From the request to the first bytes downloaded: restart, join Wi-Fi, erase the spare slot.
const START: Duration = Duration::from_secs(120);
/// No bytes taken for this long: the download is stuck.
const STALL: Duration = Duration::from_secs(30);
/// Below this, and not charging, the robot is not updated (it must not run flat halfway).
const MIN_BATTERY: f64 = 30.0;
/// From the last byte written to the robot back on the link with the new firmware.
const RESTART: Duration = Duration::from_secs(120);

/// A firmware image, checked.
#[derive(Clone)]
pub struct Image {
    pub bytes: Bytes,
    pub sha256: String,
    /// The start of the ELF's SHA-256, as the firmware reports it in its hello.
    pub build: String,
    pub version: String,
}

impl Image {
    pub fn parse(bytes: Bytes) -> Result<Image> {
        // ESP-IDF app image: a 24-byte header (magic 0xE9, chip id at 12), then the first segment,
        // whose 8-byte header is followed by the app description (esp_app_desc_t).
        if bytes.len() < 256 || bytes[0] != 0xE9 {
            bail!("not an ESP32 firmware image");
        }
        let chip = u16::from_le_bytes([bytes[12], bytes[13]]);
        if chip != CHIP_ESP32S3 {
            bail!("the image is for another chip (id {chip}), not the robot's ESP32-S3");
        }
        if bytes.len() > SLOT_SIZE {
            bail!(
                "the image is {} KB, the robot's app slots hold {} KB",
                bytes.len() / 1024,
                SLOT_SIZE / 1024
            );
        }
        let desc = &bytes[32..];
        if u32::from_le_bytes([desc[0], desc[1], desc[2], desc[3]]) != APP_DESC_MAGIC {
            bail!("no app description in the image (a bootloader or data image?)");
        }
        let text = |r: std::ops::Range<usize>| {
            let raw = &desc[r];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let build = desc[144..152].iter().map(|b| format!("{b:02x}")).collect();
        let version = format!(
            "{} {} ({} {})",
            text(48..80),
            text(16..48),
            text(96..112),
            text(80..96)
        );
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Image {
            bytes,
            sha256,
            build,
            version,
        })
    }
}

/// Where the current (or last) update is, for `GET /api/ota` and the CLI.
static STATUS: LazyLock<Mutex<Value>> = LazyLock::new(|| Mutex::new(json!({ "state": "idle" })));

pub fn status() -> Value {
    STATUS.lock().unwrap().clone()
}

fn set(v: Value) {
    if let Some(s) = v["state"].as_str() {
        tracing::info!("ota: {s} {}", v["error"].as_str().unwrap_or_default());
    }
    *STATUS.lock().unwrap() = v;
}

fn running() -> bool {
    !matches!(
        STATUS.lock().unwrap()["state"].as_str(),
        Some("idle" | "done" | "failed")
    )
}

/// Start an update in the background; `status()` follows it.
pub fn start(robot: &Robot, image: Image) -> Result<()> {
    if running() {
        bail!("an update is already running");
    }
    set(json!({ "state": "starting", "build": image.build, "version": image.version }));
    let robot = robot.clone();
    tokio::spawn(async move {
        let (build, version) = (image.build.clone(), image.version.clone());
        match update(&robot, image).await {
            Ok(()) => set(json!({ "state": "done", "build": build, "version": version })),
            Err(e) => set(json!({ "state": "failed", "build": build, "error": format!("{e:#}") })),
        }
    });
    Ok(())
}

async fn update(robot: &Robot, image: Image) -> Result<()> {
    let hub = &robot.hub;
    let (peer, caps, mode, build, battery) = {
        let s = hub.status.lock().await;
        let peer = s.peer.clone().filter(|_| s.connected);
        (
            peer,
            s.caps.clone(),
            s.mode.clone(),
            s.build.clone(),
            s.battery.clone(),
        )
    };
    let Some(peer) = peer else {
        bail!("the robot is not connected");
    };
    if !caps.iter().any(|c| c == "ota") {
        bail!(
            "this robot's firmware cannot update over Wi-Fi: flash it over USB once (tools/flash.py --all)"
        );
    }
    if mode.as_deref() != Some("remote") {
        bail!("the robot is not in AI mode");
    }
    if let Some(b) = &battery
        && b["charging"] == "unplugged"
        && b["pct"].as_f64().unwrap_or(0.0) < MIN_BATTERY
    {
        bail!("the robot's battery is low: charge it first (or plug it in)");
    }
    if build
        .as_deref()
        .is_some_and(|b| same_build(b, &image.build))
    {
        bail!("the robot already runs this build ({})", image.build);
    }
    let robot_ip: std::net::IpAddr = peer
        .rsplit_once(':')
        .map(|(ip, _)| ip.trim_matches(['[', ']']))
        .unwrap_or(&peer)
        .parse()
        .context("robot address")?;

    // Served on the address the robot reaches this machine by, on a port of its own, only for as
    // long as the update runs (the live view and API stay on localhost).
    let local_ip = {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        probe.connect((robot_ip, 9))?;
        probe.local_addr()?.ip()
    };
    let listener = tokio::net::TcpListener::bind((local_ip, 0)).await?;
    let token = format!("{:x}{:x}", crate::hub::now_ms(), std::process::id());
    let path = format!("/fw/{token}.bin");
    let url = format!("http://{}{path}", listener.local_addr()?);
    let served = Arc::new(Served::default());
    let server = tokio::spawn(serve(
        listener,
        path,
        robot_ip,
        image.bytes.clone(),
        served.clone(),
    ));
    let _server = AbortOnDrop(server);

    let total = image.bytes.len() as u32;
    hub.request_timeout(
        ToRobot::Ota {
            id: hub.next_id(),
            url,
            size: total,
            sha256: image.sha256.clone(),
        },
        Duration::from_secs(10),
    )
    .await
    .context("starting the update")?;

    // It restarts into update mode (nothing but Wi-Fi and the download, off the link), erases its
    // spare slot and fetches the image; progress is what was sent to it. Then it says how it went,
    // and restarts into AI mode: its next hello says which firmware runs.
    set(
        json!({ "state": "restarting into update mode", "build": image.build, "version": image.version }),
    );
    hub.status.lock().await.build = None;
    let mut last = (0u64, tokio::time::Instant::now());
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let result = served.result.lock().unwrap().clone();
        match result {
            Some(Ok(())) => break,
            Some(Err(e)) => bail!("the robot: {e}"),
            None => {}
        }
        if let Some(b) = hub.status.lock().await.build.clone() {
            // Back without a report (it got lost on the way).
            if same_build(&b, &image.build) {
                return Ok(());
            }
            bail!("the robot came back on its old firmware without updating");
        }
        let sent = served.sent.load(Ordering::Relaxed);
        if sent != last.0 {
            last = (sent, tokio::time::Instant::now());
            set(
                json!({ "state": "downloading", "done": sent.min(total as u64), "total": total, "build": image.build, "version": image.version }),
            );
        }
        if sent == 0 && last.1.elapsed() > START {
            bail!(
                "the robot did not start the download (could it reach this computer in update mode?); it goes back to AI mode on its old firmware"
            );
        }
        if sent > 0 && last.1.elapsed() > STALL {
            bail!("the download stalled; the robot goes back to AI mode on its old firmware");
        }
    }

    set(json!({ "state": "restarting", "build": image.build, "version": image.version }));
    let deadline = tokio::time::Instant::now() + RESTART;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        match hub.status.lock().await.build.clone() {
            Some(b) if same_build(&b, &image.build) => return Ok(()),
            Some(_) => bail!(
                "the robot came back on its previous firmware (the new one did not start, and was rolled back)"
            ),
            None => {}
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "the robot did not come back within {}s (if it cannot reach the plane on the new firmware, it goes back to the previous one at its next restart)",
                RESTART.as_secs()
            );
        }
    }
}

/// The robot's build id is the image's, or (firmware 0.1) a shorter start of it.
fn same_build(robot: &str, image: &str) -> bool {
    robot.len() >= 8 && image.starts_with(robot)
}

/// What the download server saw: bytes sent to the robot, and its report.
#[derive(Default)]
struct Served {
    sent: AtomicU64,
    result: Mutex<Option<std::result::Result<(), String>>>,
}

/// A minimal HTTP server for one file, to the robot only. `GET <path>` downloads it;
/// `GET <path>?result=ok` (or `?result=failed&error=...`) is the robot's report afterwards.
async fn serve(
    listener: tokio::net::TcpListener,
    path: String,
    robot: std::net::IpAddr,
    image: Bytes,
    served: Arc<Served>,
) {
    // Dropped (and so aborted) with this task when the update ends.
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let Ok((mut sock, from)) = listener.accept().await else {
            return;
        };
        if from.ip() != robot {
            tracing::warn!("ota: refused a download from {from}");
            continue;
        }
        let path = path.clone();
        let image = image.clone();
        let served = served.clone();
        conns.spawn(async move {
            let mut req = Vec::with_capacity(1024);
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 8192 {
                match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => req.extend_from_slice(&buf[..n]),
                    _ => return,
                }
            }
            let head = String::from_utf8_lossy(&req);
            let target = head
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("GET "))
                .and_then(|l| l.split(' ').next())
                .unwrap_or_default();
            let (wanted, query) = target.split_once('?').unwrap_or((target, ""));
            if wanted != path {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                return;
            }
            if !query.is_empty() {
                let field = |k: &str| {
                    query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                        .map(percent_decode)
                };
                let result = match field("result").as_deref() {
                    Some("ok") => Ok(()),
                    _ => Err(field("error").unwrap_or_else(|| "failed".into())),
                };
                tracing::info!("ota: the robot reports {result:?}");
                *served.result.lock().unwrap() = Some(result);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                image.len()
            );
            if sock.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            for chunk in image.chunks(16 * 1024) {
                if let Err(e) = sock.write_all(chunk).await {
                    tracing::warn!("ota: sending the image broke off: {e}");
                    return;
                }
                served.sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            let _ = sock.shutdown().await;
        });
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_image() -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0] = 0xE9;
        b[12..14].copy_from_slice(&CHIP_ESP32S3.to_le_bytes());
        b[32..36].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        b[32 + 16..32 + 21].copy_from_slice(b"1.2.3");
        b[32 + 48..32 + 66].copy_from_slice(b"ButterBot-Firmware");
        for (i, x) in b[32 + 144..32 + 176].iter_mut().enumerate() {
            *x = i as u8 + 0xa0;
        }
        b
    }

    #[test]
    fn reads_the_build_id_and_checksum() {
        let img = Image::parse(Bytes::from(fake_image())).unwrap();
        assert_eq!(img.build, "a0a1a2a3a4a5a6a7");
        assert_eq!(img.sha256.len(), 64);
        assert!(img.version.starts_with("ButterBot-Firmware 1.2.3"));
    }

    #[test]
    fn decodes_the_robots_report() {
        assert_eq!(
            percent_decode("could%20not%20join%20Wi-Fi"),
            "could not join Wi-Fi"
        );
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn refuses_other_images() {
        let mut b = fake_image();
        b[12] = 0; // ESP32
        assert!(Image::parse(Bytes::from(b)).is_err());
        let mut b = fake_image();
        b[32] = 0; // no app description
        assert!(Image::parse(Bytes::from(b)).is_err());
        assert!(Image::parse(Bytes::from(vec![0u8; 10])).is_err());
    }
}
