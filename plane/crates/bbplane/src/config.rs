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
    /// Have the robot say "Connected." when it links up in AI mode (it says "A I mode on" when
    /// AI mode starts, before joining Wi-Fi).
    pub announce_connect: bool,
    pub camera: CameraConfig,
    pub mic: MicConfig,
    pub tts: TtsConfig,
    pub safety: SafetyConfig,
    pub faces: FacesConfig,
    pub stt: SttConfig,
    /// Where `listen` recordings and snapshots are saved.
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, from = "RawCameraConfig")]
pub struct CameraConfig {
    pub res: Res,
    pub fps: u8,
    pub quality: u8,
    /// encoder ("sw" or "sensor"), xclk_mhz, psram_dma.
    #[serde(flatten)]
    pub opts: bbproto::ctrl::CamOpts,
}

/// `CameraConfig` as written in the file. serde leaves flattened fields that are missing at
/// `None` instead of taking them from `Default`, so they are filled in here.
#[derive(Deserialize)]
#[serde(default)]
struct RawCameraConfig {
    res: Res,
    fps: u8,
    quality: u8,
    #[serde(flatten)]
    opts: bbproto::ctrl::CamOpts,
}

impl Default for RawCameraConfig {
    fn default() -> Self {
        let d = CameraConfig::default();
        Self {
            res: d.res,
            fps: d.fps,
            quality: d.quality,
            opts: bbproto::ctrl::CamOpts::default(),
        }
    }
}

impl From<RawCameraConfig> for CameraConfig {
    fn from(r: RawCameraConfig) -> Self {
        Self {
            res: r.res,
            fps: r.fps,
            quality: r.quality,
            opts: r.opts.or(CameraConfig::default().opts),
        }
    }
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
            announce_connect: true,
            camera: CameraConfig::default(),
            mic: MicConfig::default(),
            tts: TtsConfig::default(),
            safety: SafetyConfig::default(),
            faces: FacesConfig::default(),
            stt: SttConfig::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FacesConfig {
    /// Recognise enrolled people in AI mode (runs on this machine; idle until someone is enrolled).
    pub enabled: bool,
    /// Say hello when an enrolled person arrives.
    pub greet: bool,
    /// Greet the same person at most this often.
    pub greet_cooldown_secs: u64,
    /// Similarity (0..1) needed to name someone. Higher means fewer mix-ups but more misses.
    pub threshold: f32,
}

impl Default for FacesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            greet: true,
            greet_cooldown_secs: 600,
            threshold: 0.40,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// Speech-to-text on this machine, for `hear` and `listen` transcripts.
    pub enabled: bool,
    /// Only speech that starts with this name is passed to the agent ("Butterbot, ..."). Empty
    /// passes everything that is said.
    pub wake_word: String,
    /// Commands not picked up by the agent within this many seconds are dropped.
    pub command_max_age_secs: u64,
    /// After the robot finishes answering, anything said within this many seconds counts as a
    /// command without the wake word, so a conversation can carry on.
    pub follow_up_secs: u64,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wake_word: "Butterbot".into(),
            command_max_age_secs: 60,
            follow_up_secs: 30,
        }
    }
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
        // At the board's default 27 MHz sensor clock, frames larger than the stock 128x128 lose
        // lines in the camera DMA and come back corrupted. 20 MHz gives clean QVGA (and VGA) with
        // headroom; the robot's CPU JPEG encoder caps QVGA at about 5-6 fps either way.
        Self {
            res: Res::Qvga,
            fps: 10,
            quality: 12,
            opts: bbproto::ctrl::CamOpts {
                encoder: Some(bbproto::ctrl::Encoder::Sw),
                xclk_mhz: Some(20),
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
        assert_eq!(c.camera.fps, 10);
        assert_eq!(c.camera.opts.xclk_mhz, Some(20));

        let c: Config = toml::from_str("[camera]\nxclk_mhz = 16\n").unwrap();
        assert_eq!(c.camera.opts.xclk_mhz, Some(16));
        assert_eq!(c.camera.opts.encoder, Some(bbproto::ctrl::Encoder::Sw));
        assert_eq!(c.camera.res, Res::Qvga);
        assert_eq!(c.link_port, 7531);
        assert!(c.mic.gate);
    }
}
