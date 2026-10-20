//! MCP server exposing the robot to an AI.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bbproto::ctrl::{LedMode, LedTarget, Res};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::control::{self, Robot};

const INSTRUCTIONS: &str = "\
Controls a ButterBot toy robot (two-wheel drive, camera, microphone, speaker).
- Movement is open loop: no encoders. `turn` estimates angle from time; check with `look`.
- Every drive stops on its own after its duration; the robot refuses to start a move when tipped over, stops \
driving forward when something is in front, and reports `drive_stopped` events.
- Using a control tool switches the robot into AI mode (its own autonomy pauses). \
Call `exit_ai_mode` to give it back.
- `look` returns what the camera sees now (320x240 live frame); `snapshot` takes a 640x480 still.
- `listen` returns microphone audio as WAV plus loudness stats.
- `say` speaks through the robot (on-robot voice by default).
- `events` long-polls robot events (sensors, battery, button, scenario progress).";

#[derive(Clone)]
pub struct BotServer {
    robot: Robot,
    tool_router: ToolRouter<Self>,
}

fn ok_text(s: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
}

fn ok_json(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    ok_text(serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// Operational failures go back to the model as tool errors, not protocol errors.
fn fail(e: impl std::fmt::Display) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        e.to_string(),
    )]))
}

macro_rules! tryt {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return fail(e),
        }
    };
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriveArgs {
    /// Left motor speed, -100..100 (positive = forward).
    pub left: i8,
    /// Right motor speed, -100..100 (positive = forward).
    pub right: i8,
    /// How long to drive, in milliseconds (max 10000).
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Forward,
    Backward,
    Left,
    Right,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    pub direction: Direction,
    /// Speed 1..100 (default 60).
    pub speed: Option<u8>,
    /// Duration in milliseconds (max 10000).
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TurnArgs {
    /// Degrees to spin in place; positive turns right (clockwise), negative left.
    pub degrees: f64,
    /// Speed 30..100 (default 70). Slower is more accurate.
    pub speed: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnapshotArgs {
    /// Default vga (640x480). Up to vga is reliable; larger sizes (svga and up) are experimental
    /// and may not fit in the robot's memory while Wi-Fi is on.
    pub resolution: Option<Res>,
    /// JPEG quality 4..63, lower is better (default 10).
    pub quality: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StreamArgs {
    /// Live stream on/off.
    pub on: bool,
    /// r128 (default, reliable). qvga/vga/svga are experimental on this robot.
    pub resolution: Option<Res>,
    /// Frames per second, 1..30.
    pub fps: Option<u8>,
    /// JPEG quality 4..63, lower is better.
    pub quality: Option<u8>,
    /// Capture options (advanced): encoder "sw" (robot CPU, default) or "sensor", xclk_mhz,
    /// psram_dma. Also used by later snapshots.
    #[serde(flatten)]
    pub opts: bbproto::ctrl::CamOpts,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListenArgs {
    /// Seconds of audio, 0.5..30 (default 5).
    pub seconds: Option<f32>,
    /// true: return the last N seconds already heard instead of recording the next N.
    pub past: Option<bool>,
    /// Include the WAV audio in the result (default true). The file path is always returned.
    pub include_audio: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SayArgs {
    /// What to say.
    pub text: String,
    /// `robot` (on-robot voice, default) or `command` (laptop TTS streamed to the robot).
    pub engine: Option<String>,
    /// Wait until speech finishes (default true).
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayArgs {
    /// Path to a WAV file on the machine running the plane.
    pub path: String,
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VolumeArgs {
    /// Speaker gain 0.0..1.0.
    pub gain: f32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventsArgs {
    /// Return events with sequence number greater than this (0 = all buffered).
    pub since: Option<u64>,
    /// Wait up to this many milliseconds for new events (max 30000, default 0).
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LedArgs {
    /// `status` (on the robot) or `module` (LED add-on module).
    pub target: LedTarget,
    /// on, off, blink or breathe.
    pub mode: LedMode,
    /// Brightness 0.0..1.0 (default 1.0).
    pub level: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScenarioArgs {
    /// Stock behavior to run, e.g. joke, fact, current_time, pass_the_butter, temp_hum_module,
    /// led_strobe, phone_whats_playing. See `scenarios`.
    pub name: String,
    /// Scenario argument (dice type, face mode, voice-control direction). Default 0.
    pub arg: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IrSendArgs {
    /// Index of a learned IR command (see `ir_list`).
    pub index: u8,
}

#[tool_router]
impl BotServer {
    pub fn new(robot: Robot) -> Self {
        Self {
            robot,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Robot connection, mode, battery, sensors and stream state.")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        ok_json(self.robot.status_json().await)
    }

    #[tool(
        description = "Switch the robot into AI mode: pauses its autonomy and starts camera and microphone streams."
    )]
    async fn enter_ai_mode(&self) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.enter_remote().await);
        ok_text("AI mode on")
    }

    #[tool(description = "Leave AI mode and give the robot back its own behavior.")]
    async fn exit_ai_mode(&self) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.exit_remote().await);
        ok_text("AI mode off")
    }

    #[tool(description = "Set both motor speeds for a duration, then stop. Positive is forward.")]
    async fn drive(
        &self,
        Parameters(a): Parameters<DriveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let l = a.left.clamp(-100, 100);
        let r = a.right.clamp(-100, 100);
        ok_json(json!(tryt!(
            self.robot.drive_for(l, r, a.duration_ms).await
        )))
    }

    #[tool(
        name = "move",
        description = "Move forward/backward or spin left/right for a duration."
    )]
    async fn move_robot(
        &self,
        Parameters(a): Parameters<MoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s = a.speed.unwrap_or(60).clamp(1, 100) as i8;
        let (l, r) = match a.direction {
            Direction::Forward => (s, s),
            Direction::Backward => (-s, -s),
            Direction::Left => (-s, s),
            Direction::Right => (s, -s),
        };
        ok_json(json!(tryt!(
            self.robot.drive_for(l, r, a.duration_ms).await
        )))
    }

    #[tool(
        description = "Spin in place by approximately the given degrees (open loop, estimate from time)."
    )]
    async fn turn(&self, Parameters(a): Parameters<TurnArgs>) -> Result<CallToolResult, McpError> {
        ok_json(json!(tryt!(
            self.robot.turn(a.degrees, a.speed.unwrap_or(70)).await
        )))
    }

    #[tool(description = "Stop the motors now.")]
    async fn stop(&self) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.stop().await);
        ok_text("stopped")
    }

    #[tool(description = "See what the camera sees right now (latest live-stream frame).")]
    async fn look(&self) -> Result<CallToolResult, McpError> {
        let f = tryt!(self.robot.snapshot(None, 12).await);
        Ok(CallToolResult::success(vec![
            ContentBlock::image(B64.encode(&f.jpeg), "image/jpeg"),
            ContentBlock::text(format!("{}x{} live frame", f.width, f.height)),
        ]))
    }

    #[tool(
        description = "Take a still photo (640x480 by default). Sharper than `look`, but takes a second or two."
    )]
    async fn snapshot(
        &self,
        Parameters(a): Parameters<SnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let res = a.resolution.unwrap_or(Res::Vga);
        let f = tryt!(
            self.robot
                .snapshot(Some(res), a.quality.unwrap_or(10))
                .await
        );
        let path = save(&self.robot, "jpg", &f.jpeg);
        Ok(CallToolResult::success(vec![
            ContentBlock::image(B64.encode(&f.jpeg), "image/jpeg"),
            ContentBlock::text(format!(
                "{}x{} snapshot, {} bytes, saved to {}",
                f.width,
                f.height,
                f.jpeg.len(),
                path
            )),
        ]))
    }

    #[tool(description = "Configure the live camera stream (used by `look` and the live view).")]
    async fn set_stream(
        &self,
        Parameters(a): Parameters<StreamArgs>,
    ) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.ensure_remote().await);
        let c = &self.robot.hub.config.camera;
        let res = a.resolution.unwrap_or(c.res);
        tryt!(
            self.robot
                .set_camera(
                    a.on,
                    res,
                    a.fps.unwrap_or(c.fps),
                    a.quality.unwrap_or(c.quality),
                    a.opts.or(c.opts),
                )
                .await
        );
        ok_text(if a.on {
            format!("streaming {res:?}")
        } else {
            "stream off".into()
        })
    }

    #[tool(description = "Record microphone audio. Returns a WAV and loudness/activity stats.")]
    async fn listen(
        &self,
        Parameters(a): Parameters<ListenArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pcm = tryt!(
            self.robot
                .listen(a.seconds.unwrap_or(5.0), a.past.unwrap_or(false))
                .await
        );
        let (wav, stats) = control::clip_summary(&pcm);
        let path = save(&self.robot, "wav", &wav);
        let mut out = vec![ContentBlock::text(format!(
            "{}\nsaved to {path}",
            serde_json::to_string_pretty(&stats).unwrap_or_default()
        ))];
        if a.include_audio.unwrap_or(true) {
            out.push(ContentBlock::audio(B64.encode(&wav), "audio/wav"));
        }
        Ok(CallToolResult::success(out))
    }

    #[tool(description = "Make the robot speak.")]
    async fn say(&self, Parameters(a): Parameters<SayArgs>) -> Result<CallToolResult, McpError> {
        let engine = tryt!(
            self.robot
                .say(&a.text, a.engine.as_deref(), a.wait.unwrap_or(true))
                .await
        );
        ok_text(format!("said ({engine} voice)"))
    }

    #[tool(description = "Play a WAV file through the robot speaker.")]
    async fn play_audio(
        &self,
        Parameters(a): Parameters<PlayArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pcm = tryt!(crate::audio::read_wav_file_16k(std::path::Path::new(
            &a.path
        )));
        let secs = pcm.len() as f32 / crate::audio::RATE as f32;
        tryt!(self.robot.play_pcm(pcm, a.wait.unwrap_or(true)).await);
        ok_text(format!("played {secs:.1} s"))
    }

    #[tool(description = "Stop any speech or audio playing on the robot.")]
    async fn stop_audio(&self) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.stop_audio().await);
        ok_text("audio stopped")
    }

    #[tool(description = "Set the robot speaker volume.")]
    async fn set_volume(
        &self,
        Parameters(a): Parameters<VolumeArgs>,
    ) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.set_volume(a.gain).await);
        ok_text(format!("volume {:.2}", a.gain.clamp(0.0, 1.0)))
    }

    #[tool(
        description = "Robot events since a sequence number: sensors, motion, battery, button, mode, scenario progress. Long-polls with wait_ms."
    )]
    async fn events(
        &self,
        Parameters(a): Parameters<EventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let wait = Duration::from_millis(a.wait_ms.unwrap_or(0).min(30_000));
        let evs = self
            .robot
            .hub
            .events_since(a.since.unwrap_or(0), wait)
            .await;
        let last = evs.last().map(|e| e.seq).unwrap_or(a.since.unwrap_or(0));
        ok_json(json!({ "events": evs, "next_since": last }))
    }

    #[tool(description = "Control an LED.")]
    async fn led(&self, Parameters(a): Parameters<LedArgs>) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(
            self.robot
                .led(a.target, a.mode, a.level.unwrap_or(1.0))
                .await
        ))
    }

    #[tool(description = "List the stock behaviors usable with run_scenario in AI mode.")]
    async fn scenarios(&self) -> Result<CallToolResult, McpError> {
        ok_json(json!({
            "available": bbproto::bb::remote_scenarios().collect::<Vec<_>>(),
            "not_in_ai_mode": bbproto::bb::NEEDS_MODELS,
        }))
    }

    #[tool(
        description = "Run one of the robot's stock behaviors (joke, dance, face_detect, ...) and wait for it to finish."
    )]
    async fn run_scenario(
        &self,
        Parameters(a): Parameters<ScenarioArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(
            self.robot.scenario(&a.name, a.arg.unwrap_or(0)).await
        ))
    }

    #[tool(description = "List IR remote commands the robot has learned (needs the IR module).")]
    async fn ir_list(&self) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(self.robot.ir_list().await))
    }

    #[tool(description = "Send a learned IR remote command (needs the IR module).")]
    async fn ir_send(
        &self,
        Parameters(a): Parameters<IrSendArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(self.robot.ir_send(a.index).await))
    }

    #[tool(description = "Read inserted add-on modules (temperature/humidity, CO2, motion, ...).")]
    async fn read_modules(&self) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(self.robot.modules().await))
    }
}

fn save(robot: &Robot, ext: &str, bytes: &[u8]) -> String {
    let dir = robot.hub.config.data_dir();
    let path = dir.join(format!("{}.{ext}", chrono_like_stamp()));
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, bytes)) {
        Ok(()) => path.display().to_string(),
        Err(e) => format!("(not saved: {e})"),
    }
}

fn chrono_like_stamp() -> String {
    let ms = crate::hub::now_ms();
    format!("bb-{}-{:03}", ms / 1000, ms % 1000)
}

const RES_CAMERA: &str = "butterbot://camera/latest";
const RES_STATUS: &str = "butterbot://status";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BotServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(RES_CAMERA, "camera")
                .with_mime_type("image/jpeg")
                .with_description("Latest live camera frame"),
            Resource::new(RES_STATUS, "status")
                .with_mime_type("application/json")
                .with_description("Robot status"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let contents = match request.uri.as_str() {
            RES_CAMERA => match self.robot.hub.latest_frame().await {
                Some(f) => ResourceContents::blob(B64.encode(&f.jpeg), RES_CAMERA)
                    .with_mime_type("image/jpeg"),
                None => return Err(McpError::resource_not_found("no camera frame yet", None)),
            },
            RES_STATUS => {
                ResourceContents::text(self.robot.status_json().await.to_string(), RES_STATUS)
                    .with_mime_type("application/json")
            }
            other => {
                return Err(McpError::resource_not_found(
                    format!("unknown resource {other}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![contents]).into())
    }
}
