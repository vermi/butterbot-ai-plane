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

use crate::control::{self, AgentEvent, Robot};

const INSTRUCTIONS: &str = "\
Controls a ButterBot toy robot (two-wheel drive, camera, microphone, speaker).
- Movement is open loop: no encoders. `turn` estimates angle from time; check with `look`.
- Every drive stops on its own after its duration; the robot refuses to start a move when tipped over, stops \
driving forward when something is in front, and reports `drive_stopped` events.
- Using a control tool switches the robot into AI mode (its own autonomy pauses). \
Call `exit_ai_mode` to give it back.
- `look` returns what the camera sees now (640x480 live frame by default); `snapshot` takes a \
fresh still.
- `hear` waits until someone talks to the robot (\"Butterbot, ...\") and returns what they said as \
text. For a conversation: `hear` once, then answer each time with `reply`, which speaks and \
returns the next thing said. \
After the robot answers, people can reply for a while without saying its name. A `hear` that \
times out just means nobody spoke; call it again.
- `listen` returns microphone audio as WAV plus loudness stats and a transcript.
- `say` speaks through the robot (on-robot voice by default).
- `events` long-polls robot events (sensors, battery, button, scenario progress, faces).
- Face recognition runs on this machine. People enrolled with `remember_face` are recognized \
in AI mode: `face` events report arrivals and departures, `who_is_here` and `status` show who is \
in view, and the robot says hello to them. Only enroll someone who has agreed to it.
- Object detection also runs on this machine: `hear` returns a `noticed` nudge when a cat, dog or \
bird comes into view, and `who_is_here` and `status` list its guesses at what else is in view.";

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
    /// qvga (about 6 fps) or vga (about 2 fps, the default). Larger sizes are experimental.
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
    /// Transcribe the audio to text on this machine (default true).
    pub transcribe: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusArgs {
    /// How long to stay quiet, in minutes (default from the config, usually 60).
    pub minutes: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplyArgs {
    /// What to say (one or two short sentences work best).
    pub text: String,
    /// Longest to wait for the next thing someone says, in seconds (default 50, max 3600).
    pub timeout_s: Option<u64>,
    /// Also return what the camera sees as the person finishes speaking. Default: only when
    /// what they said is about seeing ("what do you see", "look", "who is this", colours...).
    pub include_image: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HearArgs {
    /// Longest to wait for someone to speak, in seconds (default 50, max 3600). Keep it under
    /// your client's tool-call timeout.
    pub timeout_s: Option<u64>,
    /// Also return what the camera sees as the person finishes speaking, saving a separate
    /// `look`. Default: only when what they said is about seeing ("what do you see", "look",
    /// "who is this", colours...); images make every later turn slower, so not by default.
    pub include_image: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FaceNameArgs {
    /// The person's name, as the robot should say it.
    pub name: String,
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
    /// Brightness 0.0..1.0 (default 1.0). The status LED has three levels: off (0), dim (below
    /// 0.5) and bright.
    pub level: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ActivityArgs {
    /// quiet (speaks only when spoken to), calm, normal or lively (chats often, roams about).
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FeelArgs {
    /// content, happy, excited, curious, playful, calm, sleepy, bored, sad, grumpy, surprised or
    /// worried.
    pub emotion: String,
    /// 0.1 (a touch) to 1 (overwhelming); default 0.6.
    pub intensity: Option<f32>,
    /// A few words on why.
    pub why: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// One short sentence worth knowing next time, with names.
    pub fact: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// Id of the remembered fact (listed in the agent's instructions, and by `memories`).
    pub id: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemindArgs {
    /// What to remind them of.
    pub text: String,
    /// Minutes from now.
    pub in_minutes: Option<f64>,
    /// Or a local time, "HH:MM" (24-hour): the next time the clock says so.
    pub at: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemindersArgs {
    /// Id of a reminder to cancel (omit to list them).
    pub cancel: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PhoneArgs {
    /// `status` (connected, notification count, what is playing), `notifications` (all of them),
    /// or a music control: `play`, `pause`, `next`, `previous`.
    pub action: String,
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

    #[tool(
        description = "Learn the face of the one person in front of the camera under a name, so the robot \
recognizes and greets them. Takes 5-12 s: ask them to face the robot from about an arm's length, alone \
in view. Enrolling the same name again adds to what it knows. The first use downloads the face models \
(about 40 MB). Only enroll someone who agreed to it."
    )]
    async fn remember_face(
        &self,
        Parameters(a): Parameters<FaceNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let r = tryt!(self.robot.faces.remember(&self.robot, &a.name).await);
        let mut msg = format!("Remembered {} ({} new samples).", r.name, r.samples_added);
        if let Some(other) = r.resembles {
            msg.push_str(&format!(
                " Note: some samples also resemble {other}; if they are different people, \
recognition may mix them up."
            ));
        }
        ok_text(msg)
    }

    #[tool(description = "Forget a person enrolled with remember_face (deletes their face data).")]
    async fn forget_face(
        &self,
        Parameters(a): Parameters<FaceNameArgs>,
    ) -> Result<CallToolResult, McpError> {
        if tryt!(self.robot.faces.forget(&a.name).await) {
            ok_text(format!("Forgot {}.", a.name.trim()))
        } else {
            fail(format!(
                "Nobody called {} is enrolled. Known: {}",
                a.name.trim(),
                self.robot.faces.known().join(", ")
            ))
        }
    }

    #[tool(
        description = "Who is in front of the camera right now: enrolled people by name (with a 0..1 \
similarity score), anyone else as unknown, plus everyone enrolled, and the detector's guesses at \
objects in view (COCO's 80 everyday classes, so a fireplace may come out as an oven)."
    )]
    async fn who_is_here(&self) -> Result<CallToolResult, McpError> {
        let seen = tryt!(self.robot.faces.who_is_here(&self.robot).await);
        ok_json(serde_json::json!({
            "in_view": seen,
            "known": self.robot.faces.known(),
            "things": self.robot.objects.kinds_in_view(),
        }))
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
        let mut info = serde_json::to_value(&stats).unwrap_or_default();
        if a.transcribe.unwrap_or(true) {
            match self.robot.ears.transcribe(pcm).await {
                Ok(t) => info["transcript"] = t.into(),
                Err(e) => info["transcript_error"] = format!("{e:#}").into(),
            }
        }
        let mut out = vec![ContentBlock::text(format!(
            "{}\nsaved to {path}",
            serde_json::to_string_pretty(&info).unwrap_or_default()
        ))];
        if a.include_audio.unwrap_or(true) {
            out.push(ContentBlock::audio(B64.encode(&wav), "audio/wav"));
        }
        Ok(CallToolResult::success(out))
    }

    #[tool(
        description = "Wait until someone speaks to the robot and return what they said, as text. \
Speech must start with the robot's name (\"Butterbot, ...\") unless the robot has just answered \
someone, in which case they can simply reply. Other speech is ignored. Returns the command, the \
full transcript, and enrolled people in view (likely the speaker); or timed_out if nobody spoke. \
The first call loads the speech model (downloading about 150 MB once)."
    )]
    async fn hear(&self, Parameters(a): Parameters<HearArgs>) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.ensure_remote().await);
        self.next_command(a.timeout_s, a.include_image).await
    }

    #[tool(
        description = "Say something in the robot's voice, then wait for what the person says \
next and return it, like `hear`. One call per conversational turn, so prefer it over say + hear: \
the robot is listening again the moment it stops talking."
    )]
    async fn reply(
        &self,
        Parameters(a): Parameters<ReplyArgs>,
    ) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.say(&a.text, Some("robot"), true).await);
        self.next_command(a.timeout_s, a.include_image).await
    }

    #[tool(
        description = "Focus mode: stay quiet (no idle chatter, no nudges, no follow-up listening) \
until called by name or until the time is up. Use when someone says they need to focus or want \
quiet."
    )]
    async fn focus(
        &self,
        Parameters(a): Parameters<FocusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let minutes = a
            .minutes
            .unwrap_or(self.robot.hub.config.life.default_focus_minutes)
            .clamp(1, 24 * 60);
        self.robot
            .focus(std::time::Duration::from_secs(minutes * 60));
        ok_text(format!(
            "Focus mode for {minutes} minutes: no idle chatter until called by name."
        ))
    }

    #[tool(
        description = "The person is done talking (for now): after your reply only speech that \
starts with the robot's name counts again. A plain goodbye does this by itself."
    )]
    async fn end_conversation(&self) -> Result<CallToolResult, McpError> {
        self.robot.ears.end_conversation();
        ok_text("Conversation ended.")
    }

    #[tool(description = "End focus mode early.")]
    async fn unfocus(&self) -> Result<CallToolResult, McpError> {
        self.robot.unfocus();
        ok_text("Focus mode off.")
    }

    #[tool(
        description = "Switch the robot off: it says so and powers down (the stock shutdown \
routine). Only when someone asks for it. Nobody can use the robot again until it is switched on \
by hand."
    )]
    async fn power_off(&self) -> Result<CallToolResult, McpError> {
        tryt!(self.robot.power_off().await);
        ok_text("The robot is shutting down. Finish now.")
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

    #[tool(
        description = "Control an LED. The status LED shows the robot's state by itself (listening, thinking, focus...); what you set stays until that state changes."
    )]
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

    #[tool(
        description = "Set how much the robot does of its own accord: quiet, calm, normal or lively. Kept across \
restarts. Use it when someone asks the robot to be livelier, calmer or quieter (focus mode is separate)."
    )]
    async fn set_activity(
        &self,
        Parameters(a): Parameters<ActivityArgs>,
    ) -> Result<CallToolResult, McpError> {
        match crate::life::Activity::parse(&a.level) {
            Some(level) => {
                self.robot.set_activity(level);
                ok_json(serde_json::json!({ "activity": level.name() }))
            }
            None => Err(McpError::invalid_params(
                "level must be quiet, calm, normal or lively",
                None,
            )),
        }
    }

    #[tool(
        description = "Say how the robot feels now. It lasts (fading over an hour or so), comes back with every \
event as `mood`, sets how soon its own ideas come, and shows on the controller's face."
    )]
    async fn feel(&self, Parameters(a): Parameters<FeelArgs>) -> Result<CallToolResult, McpError> {
        match crate::mood::Emotion::parse(&a.emotion) {
            Some(e) => {
                self.robot.mood.feel(
                    e,
                    a.intensity.unwrap_or(0.6),
                    a.why.as_deref().unwrap_or_default(),
                );
                ok_json(self.robot.mood.for_agent())
            }
            None => Err(McpError::invalid_params(
                "emotion must be content, happy, excited, curious, playful, calm, sleepy, bored, \
sad, grumpy, surprised or worried",
                None,
            )),
        }
    }

    #[tool(
        description = "Wander a little: turn somewhere at random and drive a short hop. Then `look`."
    )]
    async fn wander(&self) -> Result<CallToolResult, McpError> {
        ok_text(tryt!(self.robot.wander().await))
    }

    #[tool(
        description = "Remember something across sessions (what people like, plans, names, what happened). \
Remembered facts are put into every new agent session's instructions."
    )]
    async fn remember(
        &self,
        Parameters(a): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, McpError> {
        let f = tryt!(self.robot.memory.remember(&a.fact));
        ok_json(serde_json::json!({ "id": f.id }))
    }

    #[tool(description = "Forget a remembered fact that is no longer true.")]
    async fn forget(
        &self,
        Parameters(a): Parameters<ForgetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(serde_json::json!({ "forgotten": tryt!(self.robot.memory.forget(a.id)) }))
    }

    #[tool(description = "Everything remembered across sessions.")]
    async fn memories(&self) -> Result<CallToolResult, McpError> {
        ok_json(serde_json::json!({ "facts": self.robot.memory.facts() }))
    }

    #[tool(
        description = "Set a reminder. It comes back from `hear`/`reply` as a `reminder` nudge when due (in focus \
mode too); tell the person then. If no agent passes it on within a minute, the robot says it itself."
    )]
    async fn remind(
        &self,
        Parameters(a): Parameters<RemindArgs>,
    ) -> Result<CallToolResult, McpError> {
        let r = tryt!(
            self.robot
                .reminders
                .add(&a.text, a.in_minutes, a.at.as_deref())
        );
        ok_json(serde_json::json!({
            "id": r.id,
            "due_at": r.due_local(),
            "now": chrono::Local::now().format("%H:%M").to_string(),
        }))
    }

    #[tool(description = "List the reminders that are set, or cancel one by id.")]
    async fn reminders(
        &self,
        Parameters(a): Parameters<RemindersArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(id) = a.cancel {
            return ok_json(
                serde_json::json!({ "cancelled": tryt!(self.robot.reminders.cancel(id)) }),
            );
        }
        let list: Vec<serde_json::Value> = self
            .robot
            .reminders
            .list()
            .iter()
            .map(|r| serde_json::json!({ "id": r.id, "at": r.due_local(), "text": r.text }))
            .collect();
        ok_json(serde_json::json!({ "reminders": list }))
    }

    #[tool(
        description = "The phone paired with the robot over Bluetooth (iPhone, or Android with the CircuitMess app): \
its status and what is playing, its notifications, or music controls. Read notifications only when someone asks."
    )]
    async fn phone(
        &self,
        Parameters(a): Parameters<PhoneArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(tryt!(self.robot.phone(&a.action).await))
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

impl BotServer {
    /// The next thing for the agent (see `Robot::next_event`), as a tool result.
    async fn next_command(
        &self,
        timeout_s: Option<u64>,
        include_image: Option<bool>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = std::time::Duration::from_secs(timeout_s.unwrap_or(50).clamp(1, 3600));
        let ev = tryt!(self.robot.next_event(timeout, include_image).await);
        let (v, image) = match ev {
            AgentEvent::Command { info, image } | AgentEvent::Nudge { info, image } => {
                (info, image)
            }
            AgentEvent::EndSession => (
                serde_json::json!({
                    "end_session": true,
                    "note": "time for a fresh session: finish now without speaking; you will be \
                started again with a recap of the conversation",
                }),
                None,
            ),
            AgentEvent::TimedOut { focus_minutes_left } => (
                serde_json::json!({
                    "timed_out": true,
                    "focus_minutes_left": focus_minutes_left,
                    "hint": "nothing happened; call hear again to keep listening",
                }),
                None,
            ),
        };
        let mut out = vec![ContentBlock::text(
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )];
        if let Some(jpeg) = image {
            out.push(ContentBlock::image(B64.encode(&jpeg), "image/jpeg"));
        }
        Ok(CallToolResult::success(out))
    }
}

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
