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
    pub objects: ObjectsConfig,
    pub sounds: SoundsConfig,
    pub stt: SttConfig,
    pub life: LifeConfig,
    pub agent: AgentConfig,
    pub brain: BrainConfig,
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
            objects: ObjectsConfig::default(),
            sounds: SoundsConfig::default(),
            stt: SttConfig::default(),
            life: LifeConfig::default(),
            agent: AgentConfig::default(),
            brain: BrainConfig::default(),
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
    /// Let the agent drive while the robot is plugged in (it would pull on the cable). The live
    /// view's drive pad is not limited.
    pub drive_when_plugged: bool,
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
    /// Someone counts as arriving (a hello, or a nudge for the agent) only after being out of view
    /// this long. The camera sits low and loses people all the time.
    pub welcome_back_minutes: u64,
    /// Similarity (0..1) needed to name someone. Higher means fewer mix-ups but more misses.
    pub threshold: f32,
}

impl Default for FacesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            greet: true,
            greet_cooldown_secs: 600,
            welcome_back_minutes: 30,
            threshold: 0.40,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectsConfig {
    /// Detect everyday objects in AI mode (runs on this machine; the model, about 36 MB, is
    /// downloaded on first use). People seen without a face count as company for idle nudges.
    pub enabled: bool,
    /// Kinds of things (COCO class names) whose coming into view nudges the agent.
    pub notice: Vec<String>,
    /// Detection confidence (0..1) needed to count something as seen.
    pub threshold: f32,
    /// Notice the same kind of thing at most this often.
    pub notice_cooldown_secs: u64,
    /// With pets living here (`[life] pets`), animals are around all the time: remark on one at
    /// most this often.
    pub pet_cooldown_secs: u64,
}

impl Default for ObjectsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            notice: ["cat", "dog", "bird"].map(String::from).to_vec(),
            threshold: 0.4,
            notice_cooldown_secs: 600,
            pet_cooldown_secs: 1800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundsConfig {
    /// Recognise sounds in AI mode (runs on this machine; the model, about 16 MB, is downloaded
    /// on first use).
    pub enabled: bool,
    /// Sounds (AudioSet class names) that nudge the agent. Alarms ("Smoke detector, smoke alarm",
    /// "Fire alarm", "Alarm"; people hear those anyway) can be added: they come through focus mode.
    pub notice: Vec<String>,
    /// Score (0..1) needed to count a sound as heard.
    pub threshold: f32,
    /// Notice the same sound at most this often.
    pub cooldown_secs: u64,
}

impl Default for SoundsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            notice: [
                "Bark",
                "Doorbell",
                "Ding-dong",
                "Knock",
                "Beep, bleep",
                "Baby cry, infant cry",
                "Shatter",
                "Meow",
            ]
            .map(String::from)
            .to_vec(),
            threshold: 0.3,
            cooldown_secs: 300,
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
            follow_up_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LifeConfig {
    /// Offer the agent nudges (arrivals, idle time) through `hear` and `reply`.
    pub enabled: bool,
    /// Enrolled name of the person whose requests come first.
    pub owner: String,
    /// Animals that live here, as you would introduce them: "Biscuit, a beagle".
    pub pets: Vec<String>,
    /// Topics the people here enjoy: idle chit-chat brings up recent news in them (looked up on
    /// the web) rather than jokes.
    pub interests: Vec<String>,
    /// Random wait, in seconds, between idle nudges while someone is in view...
    pub nudge_in_view_secs: [u64; 2],
    /// ...and while nobody is.
    pub nudge_alone_secs: [u64; 2],
    /// How much the robot does of its own accord at start: quiet, calm, normal or lively (it
    /// can be changed by voice; that choice is kept across restarts).
    pub activity: crate::life::Activity,
    /// Focus mode lasts this long unless told otherwise.
    pub default_focus_minutes: u64,
    /// Start the agent on a fresh session after this many minutes (at a quiet moment), so its
    /// context stays small. 0 keeps one session.
    pub session_minutes: u64,
    /// When the robot writes its diary, local time "HH:MM" (a missed evening is caught up the
    /// next morning). Empty: no diary.
    pub diary_at: String,
    /// Nap on the charger: once plugged in and nothing is going on, it dozes (only its name, a
    /// poke or an alarm get through) and dreams now and then, until it is unplugged.
    pub nap_when_charging: bool,
}

impl Default for LifeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            owner: String::new(),
            pets: Vec::new(),
            interests: Vec::new(),
            nudge_in_view_secs: [120, 300],
            nudge_alone_secs: [900, 1800],
            activity: crate::life::Activity::Normal,
            default_focus_minutes: 60,
            session_minutes: 30,
            diary_at: "21:30".into(),
            nap_when_charging: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Run the agent while the robot is in AI mode (started and restarted by `bbplane serve`).
    pub enabled: bool,
    /// `realtime`: the built-in brain on OpenAI's Realtime API (fastest, speaks with an OpenAI
    /// voice). `text`: the built-in brain on a text model, speaking sentence by sentence.
    /// `command`: an external MCP agent (`command`, Codex by default). See [brain].
    pub kind: AgentKind,
    /// The agent's command. `{prompt}` is replaced by the persona plus a recap of recent
    /// conversation. The agent must have this plane's MCP server configured.
    pub command: Vec<String>,
    /// Persona prompt file (default: the built-in Butterbot persona).
    pub persona: Option<std::path::PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: AgentKind::Realtime,
            command: [
                "codex",
                "--search",
                "exec",
                "--skip-git-repo-check",
                "-c",
                "model_reasoning_effort=\"low\"",
                // Codex hides tools behind a search step that fails in exec mode when this
                // server adds many; the robot needs none of it.
                "-c",
                "mcp_servers.node_repl.enabled=false",
                // `hear` waits until something happens (up to 30 min): one model call per event,
                // not one per minute of silence.
                "-c",
                "mcp_servers.butterbot.tool_timeout_sec=2000",
                "{prompt}",
            ]
            .map(String::from)
            .to_vec(),
            persona: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Realtime,
    Text,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrainConfig {
    /// File holding the OpenAI API key (default: the OPENAI_API_KEY environment variable).
    pub api_key_file: Option<PathBuf>,
    pub realtime_model: String,
    /// `robot`: the robot's own voice, each sentence spoken as soon as it is written (both
    /// brains). Or an OpenAI voice (alloy, ash, ballad, cedar, coral, echo, marin, sage, shimmer,
    /// verse): realtime audio, or `tts_model` for the text brain.
    pub voice: String,
    pub text_model: String,
    /// Reasoning effort for the text model (`none` is fastest where the model allows it).
    pub text_reasoning: String,
    pub tts_model: String,
    /// Model used for the web_search tool.
    pub search_model: String,
    /// Embedding model for recall (see `crate::recall`).
    pub embedding_model: String,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            api_key_file: None,
            realtime_model: "gpt-realtime-2.1".into(),
            voice: "robot".into(),
            text_model: "gpt-5.6-terra".into(),
            text_reasoning: "none".into(),
            tts_model: "gpt-4o-mini-tts".into(),
            search_model: "gpt-5.6-terra".into(),
            embedding_model: "text-embedding-3-small".into(),
        }
    }
}

impl BrainConfig {
    /// The API key, from `api_key_file` or OPENAI_API_KEY. Never logged.
    pub fn api_key(&self) -> Result<String> {
        if let Some(p) = &self.api_key_file {
            let p = expand_home(p);
            let k = std::fs::read_to_string(&p)
                .with_context(|| format!("reading the API key from {}", p.display()))?;
            return Ok(k.trim().to_string());
        }
        std::env::var("OPENAI_API_KEY")
            .map(|k| k.trim().to_string())
            .context("no API key: set [brain] api_key_file or OPENAI_API_KEY")
    }
}

fn expand_home(p: &std::path::Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            tilt: true,
            obstacle: true,
            drive_when_plugged: false,
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        // At the board's default 27 MHz sensor clock, frames larger than the stock 128x128 lose
        // lines in the camera DMA and come back corrupted. 20 MHz gives clean QVGA and VGA with
        // headroom. The robot's CPU JPEG encoder manages about 2 fps at VGA (5-6 at QVGA), but at
        // QVGA the object detector misses a dog lying across the room and faces are too small.
        Self {
            res: Res::Vga,
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

/// Write `bytes` to `path` readable by this user only (atomically, through a temporary file).
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    std::io::Write::write_all(&mut f, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
        assert_eq!(c.camera.res, Res::Vga);
        assert_eq!(c.link_port, 7531);
        assert!(c.mic.gate);
    }
}
