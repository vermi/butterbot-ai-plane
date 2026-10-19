use std::path::PathBuf;

use anyhow::{Context, Result};
use bbproto::ctrl::Res;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address the robot link listens on. The robot dials this.
    pub link_bind: String,
    pub link_port: u16,
    /// Address of the live view, REST API and HTTP MCP endpoint.
    pub web_bind: String,
    pub web_port: u16,
    /// Extra host names the live view and MCP endpoint may be reached under. IP addresses and
    /// `localhost` always work; anything else is refused to stop DNS rebinding attacks.
    pub allowed_hosts: Vec<String>,
    /// Enter remote (AI) mode automatically when a control tool is used.
    pub auto_enter_remote: bool,
    /// Wake the robot over Bluetooth when it is not connected. The robot only runs Wi-Fi in AI mode.
    pub ble_wake: bool,
    pub camera: CameraConfig,
    pub mic: MicConfig,
    pub tts: TtsConfig,
    pub safety: SafetyConfig,
    /// Where `listen` recordings and snapshots are saved.
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    pub res: Res,
    pub fps: u8,
    pub quality: u8,
    /// encoder ("sw" or "sensor"), xclk_mhz, psram_dma.
    #[serde(flatten)]
    pub opts: bbproto::ctrl::CamOpts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MicConfig {
    /// Mute the mic while the robot speaks (the robot has no echo cancellation).
    pub gate: bool,
    pub gain: f32,
    /// Seconds of microphone audio kept for `listen` with `past=true`.
    pub history_secs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// `robot` (on-robot flite voice) or `command` (laptop TTS streamed to the speaker).
    pub engine: String,
    /// Command for the `command` engine. `{out}` is a WAV path to write, `{text}` the text.
    /// When empty, a per-platform preset is used.
    pub command: Vec<String>,
    /// Feed the text on stdin instead of substituting `{text}` (e.g. piper).
    pub text_on_stdin: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            link_bind: "0.0.0.0".into(),
            link_port: bbproto::LINK_PORT,
            web_bind: "127.0.0.1".into(),
            web_port: 7532,
            allowed_hosts: Vec::new(),
            auto_enter_remote: true,
            ble_wake: true,
            camera: CameraConfig::default(),
            mic: MicConfig::default(),
            tts: TtsConfig::default(),
            safety: SafetyConfig::default(),
            data_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Refuse to start a move when the robot is tipped over (accelerometer).
    pub tilt: bool,
    /// Stop driving forward when the front sensor is covered.
    pub obstacle: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            tilt: true,
            obstacle: true,
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        // 128x128 is the size the stock firmware streams; larger frames come back corrupted on this
        // board (camera DMA cannot keep up while Wi-Fi is busy).
        Self {
            res: Res::R128,
            fps: 15,
            quality: 12,
            opts: bbproto::ctrl::CamOpts {
                encoder: Some(bbproto::ctrl::Encoder::Sw),
                ..Default::default()
            },
        }
    }
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            gate: true,
            gain: 1.0,
            history_secs: 30,
        }
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            engine: "robot".into(),
            command: Vec::new(),
            text_on_stdin: false,
        }
    }
}

fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "bbplane")
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        dirs().map(|d| d.config_dir().join("config.toml"))
    }

    /// Load from `path`, or the platform default path. A missing file means defaults.
    pub fn load(path: Option<&PathBuf>) -> Result<Config> {
        let Some(path) = path.cloned().or_else(Self::default_path) else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .or_else(|| dirs().map(|d| d.data_dir().to_path_buf()))
            .unwrap_or_else(|| std::env::temp_dir().join("bbplane"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let c: Config = toml::from_str("web_port = 9000\n[camera]\nres = \"vga\"\n").unwrap();
        assert_eq!(c.web_port, 9000);
        assert_eq!(c.camera.res, Res::Vga);
        assert_eq!(c.camera.fps, 15);
        assert_eq!(c.link_port, 7531);
        assert!(c.mic.gate);
    }
}
