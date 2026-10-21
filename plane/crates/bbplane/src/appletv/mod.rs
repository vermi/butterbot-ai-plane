//! The Apple TV, through pyatv: play, pause, skip, volume, power, apps, and what is playing.
//!
//! pyatv is Python, so it runs in a helper process (`atv.py`, in a virtual environment under the
//! data folder) that speaks JSON lines. Pairing happens once from the live view: the Apple TV shows
//! a PIN for each protocol (Companion for apps and power, AirPlay for media), typed in there. The
//! chosen Apple TV is kept in appletv/device.json and the credentials in appletv/pyatv.conf. When
//! music starts on it, the robot gets a `music` nudge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::control::Robot;

const HELPER: &str = include_str!("atv.py");
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Try to reconnect this often while the Apple TV is unreachable (sooner at first).
const RECONNECT: Duration = Duration::from_secs(60);
const RECONNECT_FIRST: Duration = Duration::from_secs(10);

/// What the agent may do with the Apple TV.
pub const ACTIONS: [&str; 22] = [
    "status",
    "play",
    "pause",
    "play_pause",
    "stop",
    "next",
    "previous",
    "volume_up",
    "volume_down",
    "set_volume",
    "turn_on",
    "turn_off",
    "apps",
    "launch",
    "home",
    "menu",
    "select",
    "up",
    "down",
    "left",
    "right",
    "now_playing",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub address: String,
    pub name: String,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

pub struct AppleTv {
    dir: PathBuf,
    helper: Mutex<Option<(Child, ChildStdin)>>,
    pending: std::sync::Arc<StdMutex<Pending>>,
    next_id: AtomicU64,
    /// The latest "playing" report.
    playing: std::sync::Arc<StdMutex<Option<Value>>>,
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AppleTv {
    pub fn new(data_dir: &Path) -> AppleTv {
        AppleTv {
            dir: data_dir.join("appletv"),
            helper: Mutex::new(None),
            pending: Default::default(),
            next_id: AtomicU64::new(1),
            playing: Default::default(),
            connected: Default::default(),
        }
    }

    fn python(&self) -> PathBuf {
        self.dir.join("venv/bin/python")
    }

    pub fn installed(&self) -> bool {
        self.python().exists()
    }

    pub fn device(&self) -> Option<Device> {
        std::fs::read(self.dir.join("device.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn save_device(&self, d: &Device) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join("device.json"), serde_json::to_vec_pretty(d)?)?;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn now_playing(&self) -> Option<Value> {
        self.playing.lock().unwrap().clone()
    }

    /// Make the virtual environment and install pyatv into it (needs the internet once).
    pub async fn setup(&self) -> Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        if !self.installed() {
            let out = Command::new("python3")
                .args(["-m", "venv"])
                .arg(self.dir.join("venv"))
                .output()
                .await
                .context("running python3 (is it installed?)")?;
            if !out.status.success() {
                bail!(
                    "could not make the environment: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        let out = Command::new(self.dir.join("venv/bin/pip"))
            .args([
                "install",
                "-q",
                "--disable-pip-version-check",
                "--upgrade",
                "pyatv",
            ])
            .output()
            .await
            .context("running pip")?;
        if !out.status.success() {
            bail!(
                "could not install pyatv: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok("pyatv is installed".into())
    }

    /// Start the helper if it is not running.
    async fn ensure_helper(&self, robot: &Robot) -> Result<()> {
        let mut h = self.helper.lock().await;
        if let Some((child, _)) = h.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            return Ok(());
        }
        if !self.installed() {
            bail!("pyatv is not set up yet (live view, Apple TV card)");
        }
        let script = self.dir.join("atv.py");
        std::fs::write(&script, HELPER)?;
        let mut child = Command::new(self.python())
            .arg("-I")
            .arg(&script)
            .arg("--storage")
            .arg(self.dir.join("pyatv.conf"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting the Apple TV helper")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!("apple tv helper: {l}");
                }
            });
        }
        let pending = self.pending.clone();
        let playing = self.playing.clone();
        let connected = self.connected.clone();
        let robot = robot.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = v["id"].as_u64() {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let r = if v["ok"] == true {
                            Ok(v["result"].clone())
                        } else {
                            Err(v["error"].as_str().unwrap_or("failed").to_string())
                        };
                        tx.send(r).ok();
                    }
                    continue;
                }
                match v["event"].as_str() {
                    Some("playing") => {
                        let before = playing.lock().unwrap().replace(v.clone());
                        on_playing(&robot, before.as_ref(), &v).await;
                    }
                    Some("disconnected") => {
                        connected.store(false, Ordering::Relaxed);
                        tracing::info!("apple tv: disconnected");
                    }
                    Some("error") => tracing::debug!("apple tv: {}", v["error"]),
                    _ => {}
                }
            }
            connected.store(false, Ordering::Relaxed);
            for (_, tx) in pending.lock().unwrap().drain() {
                tx.send(Err("the Apple TV helper stopped".into())).ok();
            }
        });
        *h = Some((child, stdin));
        Ok(())
    }

    async fn request(&self, robot: &Robot, cmd: &str, args: Value) -> Result<Value> {
        self.ensure_helper(robot).await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // "id" is the request's own; a device goes as "device".
        let mut msg = args;
        msg["id"] = id.into();
        msg["cmd"] = cmd.into();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut h = self.helper.lock().await;
            let (_, stdin) = h.as_mut().ok_or_else(|| anyhow!("no helper"))?;
            stdin
                .write_all(format!("{msg}\n").as_bytes())
                .await
                .context("talking to the Apple TV helper")?;
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("the Apple TV helper stopped"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("the Apple TV did not answer")
            }
        }
    }

    pub async fn scan(&self, robot: &Robot, address: Option<&str>) -> Result<Value> {
        self.request(robot, "scan", json!({ "address": address, "timeout": 5 }))
            .await
    }

    /// Start pairing `protocol` ("companion" or "airplay"); the Apple TV shows a PIN.
    pub async fn pair_start(&self, robot: &Robot, d: Device, protocol: &str) -> Result<Value> {
        self.save_device(&d)?;
        self.request(
            robot,
            "pair_start",
            json!({ "address": d.address, "device": d.id, "protocol": protocol }),
        )
        .await
    }

    pub async fn pair_pin(&self, robot: &Robot, pin: &str) -> Result<Value> {
        let r = self
            .request(robot, "pair_pin", json!({ "pin": pin }))
            .await?;
        // Paired: connect with what we have now.
        if let Err(e) = self.connect(robot).await {
            tracing::debug!("apple tv connect after pairing: {e:#}");
        }
        Ok(r)
    }

    pub async fn connect(&self, robot: &Robot) -> Result<()> {
        let d = self
            .device()
            .ok_or_else(|| anyhow!("no Apple TV chosen yet (live view, Apple TV card)"))?;
        self.request(
            robot,
            "connect",
            json!({ "address": d.address, "device": d.id }),
        )
        .await?;
        self.connected.store(true, Ordering::Relaxed);
        tracing::info!("apple tv: connected to {}", d.name);
        Ok(())
    }

    /// Do `action` (see `ACTIONS`); `value`: the volume (0-100) or the app to launch.
    pub async fn act(&self, robot: &Robot, action: &str, value: Option<&str>) -> Result<Value> {
        if !self.is_connected() {
            self.connect(robot).await?;
        }
        match action {
            "status" => self.request(robot, "status", json!({})).await,
            "now_playing" => Ok(self.now_playing().unwrap_or(Value::Null)),
            "apps" => self.request(robot, "apps", json!({})).await,
            "launch" => {
                let app = value.ok_or_else(|| anyhow!("which app?"))?;
                self.request(robot, "launch", json!({ "app": app })).await
            }
            "set_volume" => {
                let v: f64 = value
                    .and_then(|v| v.trim().trim_end_matches('%').parse().ok())
                    .ok_or_else(|| anyhow!("set_volume needs a value from 0 to 100"))?;
                self.request(
                    robot,
                    "control",
                    json!({ "action": "set_volume", "value": v.clamp(0.0, 100.0) }),
                )
                .await
            }
            a if ACTIONS.contains(&a) => {
                self.request(robot, "control", json!({ "action": a })).await
            }
            _ => bail!(
                "no action called {action:?}; there are: {}",
                ACTIONS.join(", ")
            ),
        }
    }

    pub async fn status_json(&self, robot: &Robot) -> Value {
        let mut v = json!({
            "installed": self.installed(),
            "device": self.device(),
            "connected": self.is_connected(),
            "playing": self.now_playing(),
        });
        if self.is_connected()
            && let Ok(s) = self.request(robot, "status", json!({})).await
        {
            v["status"] = s;
        }
        v
    }
}

/// Music starting on the Apple TV is something the robot may want to dance to.
async fn on_playing(robot: &Robot, before: Option<&Value>, now: &Value) {
    let playing = now["state"] == "playing";
    let was = before.is_some_and(|b| b["state"] == "playing" && b["title"] == now["title"]);
    if !playing || was {
        return;
    }
    let title = now["title"].as_str().unwrap_or("something");
    let what = match now["artist"].as_str() {
        Some(a) if !a.is_empty() => format!("{title} by {a}"),
        _ => title.to_string(),
    };
    robot
        .hub
        .push_event(
            "appletv",
            json!({ "title": now["title"], "artist": now["artist"], "app": now["app"], "media_type": now["media_type"] }),
        )
        .await;
    if now["media_type"] == "music" {
        robot
            .life
            .music(&format!("{what} is playing on the Apple TV"), None);
    }
}

/// Keep connected to the chosen Apple TV. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let mut tries = 0u32;
    loop {
        let atv = &robot.appletv;
        if atv.installed() && atv.device().is_some() && !atv.is_connected() {
            if let Err(e) = atv.connect(&robot).await {
                tracing::debug!("apple tv: {e:#}");
            }
        }
        tries += 1;
        tokio::time::sleep(if tries < 6 { RECONNECT_FIRST } else { RECONNECT }).await;
    }
}
