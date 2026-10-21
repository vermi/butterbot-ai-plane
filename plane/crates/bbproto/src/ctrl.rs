//! JSON control messages carried in `Ctrl` frames. Tagged by `"t"`.
//!
//! Requests that expect an answer carry an `id`; the robot replies with `ack`
//! (optionally with `data`) or `error` using the same id.

use serde::{Deserialize, Serialize};

/// Camera resolutions the firmware maps onto `framesize_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Res {
    /// 128x128, the stock size used by on-robot face/object detection.
    R128,
    /// 320x240
    #[default]
    Qvga,
    /// 640x480
    Vga,
    /// 800x600
    Svga,
    /// 1024x768
    Xga,
    /// 1280x720
    Hd,
    /// 1600x1200
    Uxga,
    /// 2048x1536, the OV3660 maximum
    Qxga,
}

impl Res {
    pub fn size(self) -> (u16, u16) {
        match self {
            Res::R128 => (128, 128),
            Res::Qvga => (320, 240),
            Res::Vga => (640, 480),
            Res::Svga => (800, 600),
            Res::Xga => (1024, 768),
            Res::Hd => (1280, 720),
            Res::Uxga => (1600, 1200),
            Res::Qxga => (2048, 1536),
        }
    }
}

/// How the robot produces JPEG frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    /// Capture RGB565 and encode on the robot's CPU (the stock RC video path). Reliable.
    Sw,
    /// Use the camera sensor's own JPEG output. Faster, but needs a suitable clock/DMA mode.
    Sensor,
}

/// Camera capture options; unset fields use the robot's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CamOpts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<Encoder>,
    /// Sensor clock in MHz (board default 27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xclk_mhz: Option<u8>,
    /// esp32-camera PSRAM DMA mode (board default on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psram_dma: Option<bool>,
}

impl CamOpts {
    /// `self` with unset fields taken from `base`.
    pub fn or(self, base: CamOpts) -> CamOpts {
        CamOpts {
            encoder: self.encoder.or(base.encoder),
            xclk_mhz: self.xclk_mhz.or(base.xclk_mhz),
            psram_dma: self.psram_dma.or(base.psram_dma),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LedTarget {
    /// The status LED on the robot body.
    Status,
    /// The LED add-on module, if inserted.
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum LedMode {
    On,
    Off,
    Blink,
    Breathe,
}

/// A status LED rhythm. The status LED has three levels: off (0), dim (above 0, below 0.5) and
/// bright. It sits at `base`, and every period starts with `count` flashes to `flash`, `on_ms`
/// long and `off_ms` apart. `count` 0 is steady at `base`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rhythm {
    pub on_ms: u16,
    pub off_ms: u16,
    pub count: u8,
    pub period_ms: u16,
    pub base: f32,
    pub flash: f32,
}

impl Rhythm {
    pub const OFF: f32 = 0.0;
    pub const DIM: f32 = 0.2;
    pub const BRIGHT: f32 = 1.0;

    pub const fn steady(level: f32) -> Rhythm {
        Rhythm {
            on_ms: 0,
            off_ms: 0,
            count: 0,
            period_ms: 1000,
            base: level,
            flash: level,
        }
    }
}

fn yes() -> bool {
    true
}

/// Plane -> robot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToRobot {
    /// Ask the robot to introduce itself again (hello + status), e.g. after a plane restart.
    Hello,
    Ping {
        id: u32,
        ts: u64,
    },
    /// Leave autonomy and hand control to the plane.
    EnterRemote {
        id: u32,
    },
    /// Return to the stock idle behavior.
    ExitRemote {
        id: u32,
    },
    /// Set motor speeds (-100..100, positive is forward) for `ms` milliseconds.
    /// The robot stops on its own when `ms` expires; resend to keep moving.
    Drive {
        l: i8,
        r: i8,
        ms: u16,
        /// AI drive: apply the safety checks. false = direct control, no checks (like the RC controller).
        #[serde(default = "yes")]
        safe: bool,
    },
    Stop,
    /// Speak with the on-robot flite voice.
    Say {
        id: u32,
        text: String,
    },
    /// Prepare the speaker for a `SpkPcm` stream.
    SpkBegin {
        id: u32,
    },
    /// No more `SpkPcm` will follow; play out what is buffered.
    SpkEnd,
    /// Drop buffered speaker audio and stop immediately.
    SpkAbort,
    /// Speaker gain 0.0..1.0.
    Volume {
        gain: f32,
    },
    /// Live camera stream settings. `on=false` stops the stream.
    Camera {
        id: u32,
        on: bool,
        res: Res,
        fps: u8,
        quality: u8,
        #[serde(flatten)]
        opts: CamOpts,
    },
    /// One still frame; answered with a `Video` frame flagged as snapshot, seq = id.
    Snapshot {
        id: u32,
        res: Res,
        quality: u8,
    },
    /// Live microphone stream. `gate` mutes it while the speaker plays (no echo cancel).
    Mic {
        id: u32,
        on: bool,
        gate: bool,
        gain: f32,
    },
    Led {
        id: u32,
        target: LedTarget,
        mode: LedMode,
        level: f32,
    },
    /// What the status LED shows in AI mode, unless the robot is talking (bright) or cannot reach
    /// the plane (it shows those itself). Not acknowledged.
    StatusLight(Rhythm),
    /// AI mode's state for the handheld controller's screen: `mode` 1 listening for the name,
    /// 2 in a conversation, 3 thinking, 4 focus, 5 no agent; `activity` 0 quiet .. 3 lively;
    /// minutes left in focus mode; its mood (`emotion` in `bbplane::mood::Emotion` order,
    /// `intensity` 0-100). Not acknowledged.
    AiStatus {
        mode: u8,
        activity: u8,
        focus_minutes: u8,
        emotion: u8,
        intensity: u8,
    },
    /// The handheld controller's home screen in AI mode: the local `clock` (year, month 1-12,
    /// day, hour, minute, second; the robot sets its own clock from it), the next reminder, the
    /// latest thing noticed and how long ago, and the last thing said. Not acknowledged.
    HomeInfo {
        clock: [u16; 6],
        next: String,
        seen: String,
        seen_secs: u32,
        said: String,
    },
    /// A small camera picture for the handheld controller's screen (base64 JPEG) and what is in
    /// it. Not acknowledged.
    Peek {
        jpeg: String,
        caption: String,
    },
    /// Run a stock scenario routine (`BB::Action::Scenario` value + `ScenarioData.raw`).
    Scenario {
        id: u32,
        scenario: u32,
        arg: u8,
    },
    IrList {
        id: u32,
    },
    IrSend {
        id: u32,
        index: u8,
    },
    /// Read inserted add-on modules.
    Modules {
        id: u32,
    },
    /// The phone link: `status`, a page of `notifications` from `index`, or a music control
    /// (`play`, `pause`, `next`, `previous`).
    Phone {
        id: u32,
        action: String,
        #[serde(default)]
        index: u8,
    },
    /// Drive safety checks: refuse to start moving when tipped over; stop forward driving when
    /// the front sensor is covered.
    Safety {
        id: u32,
        tilt: bool,
        obstacle: bool,
    },
}

impl ToRobot {
    pub fn id(&self) -> Option<u32> {
        use ToRobot::*;
        match self {
            Ping { id, .. }
            | EnterRemote { id }
            | ExitRemote { id }
            | Say { id, .. }
            | SpkBegin { id }
            | Camera { id, .. }
            | Snapshot { id, .. }
            | Mic { id, .. }
            | Led { id, .. }
            | Scenario { id, .. }
            | IrList { id }
            | IrSend { id, .. }
            | Modules { id }
            | Phone { id, .. }
            | Safety { id, .. } => Some(*id),
            Hello
            | Drive { .. }
            | Stop
            | SpkEnd
            | SpkAbort
            | Volume { .. }
            | StatusLight(_)
            | AiStatus { .. }
            | HomeInfo { .. }
            | Peek { .. } => None,
        }
    }
}

/// Robot -> plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromRobot {
    Hello {
        proto: u32,
        fw: String,
        mac: String,
        #[serde(default)]
        caps: Vec<String>,
    },
    Pong {
        id: u32,
        ts: u64,
    },
    Ack {
        id: u32,
        #[serde(default)]
        data: serde_json::Value,
    },
    Error {
        id: u32,
        msg: String,
    },
    /// Robot mode changed: `idle`, `listen`, `scenario`, `rc`, `remote`.
    Mode {
        mode: String,
    },
    /// Mirror of a stock BLE `Com::sendData` message. `data` is the raw payload, hex.
    Bb {
        state: u32,
        action: u32,
        data: String,
    },
    /// Proximity sensors: `covered`, `uncovered` or `unknown`.
    Proximity {
        front: String,
        bottom: String,
    },
    /// `fall`, `upside_down`, `pickup`, `shake`.
    Motion {
        kind: String,
    },
    /// The robot stopped the motors on its own.
    DriveStopped {
        reason: String,
    },
    Battery {
        pct: f32,
        level: u8,
        charging: String,
    },
    Speaker {
        playing: bool,
        #[serde(default)]
        buffered_ms: u32,
        #[serde(default)]
        underruns: u32,
    },
    /// The paired phone is ringing (AI mode only).
    PhoneCall {
        from: String,
    },
    /// The paired phone connected or went away (AI mode only).
    PhoneLink {
        connected: bool,
    },
    /// A button on the handheld controller in AI mode: `override` (Manual Override: the robot
    /// leaves AI mode for stock RC mode and comes back), `shut_up` (`talking`: it was cut off),
    /// `poke`, `summon`, `listen`, a menu `scenario` (`scenario`, `arg`) the robot leaves to the
    /// agent, or `other` (`command`, for anything else).
    Remote {
        button: String,
        #[serde(default)]
        on: Option<bool>,
        #[serde(default)]
        talking: Option<bool>,
        #[serde(default)]
        scenario: Option<u32>,
        #[serde(default)]
        arg: Option<u8>,
        #[serde(default)]
        command: Option<u32>,
    },
    /// Wi-Fi signal strength, reported every few seconds while connected.
    Wifi {
        rssi: i32,
        /// Free internal RAM in bytes, and the largest free block.
        #[serde(default)]
        ram_free: Option<u32>,
        #[serde(default)]
        ram_block: Option<u32>,
    },
    /// Power button press while in remote mode, etc.
    Button {
        action: String,
    },
    Log {
        level: String,
        msg: String,
    },
}

impl FromRobot {
    /// Parse a `Ctrl` payload. Unknown message types are returned as raw JSON
    /// so a newer firmware never breaks an older plane.
    pub fn parse(
        payload: &[u8],
    ) -> Result<Result<FromRobot, serde_json::Value>, serde_json::Error> {
        let v: serde_json::Value = serde_json::from_slice(payload)?;
        Ok(serde_json::from_value::<FromRobot>(v.clone()).map_err(|_| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_robot_wire_shape() {
        let m = ToRobot::Drive {
            l: 50,
            r: -50,
            ms: 300,
            safe: true,
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"t":"drive","l":50,"r":-50,"ms":300,"safe":true})
        );
        let m = ToRobot::Camera {
            id: 3,
            on: true,
            res: Res::Vga,
            fps: 10,
            quality: 12,
            opts: CamOpts::default(),
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"t":"camera","id":3,"on":true,"res":"vga","fps":10,"quality":12})
        );
        let m = ToRobot::Camera {
            id: 3,
            on: true,
            res: Res::Qvga,
            fps: 10,
            quality: 12,
            opts: CamOpts {
                encoder: Some(Encoder::Sensor),
                xclk_mhz: Some(16),
                psram_dma: Some(false),
            },
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["encoder"], "sensor");
        assert_eq!(v["xclk_mhz"], 16);
        assert_eq!(v["psram_dma"], false);
        assert_eq!(serde_json::from_value::<ToRobot>(v).unwrap(), m);
        assert_eq!(
            serde_json::to_value(ToRobot::Stop).unwrap(),
            json!({"t":"stop"})
        );
        let m = ToRobot::Snapshot {
            id: 1,
            res: Res::R128,
            quality: 10,
        };
        assert_eq!(serde_json::to_value(&m).unwrap()["res"], "r128");
        let m = ToRobot::StatusLight(Rhythm {
            on_ms: 100,
            off_ms: 0,
            count: 1,
            period_ms: 200,
            base: 0.25,
            flash: 1.0,
        });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            json!({"t":"status_light","on_ms":100,"off_ms":0,"count":1,"period_ms":200,"base":0.25,"flash":1.0})
        );
        assert_eq!(serde_json::from_value::<ToRobot>(v).unwrap(), m);
    }

    #[test]
    fn from_robot_parses_and_tolerates_unknown() {
        let hello = br#"{"t":"hello","proto":1,"fw":"bb-ai 0.1","mac":"14:c1:9f:52:32:a0"}"#;
        match FromRobot::parse(hello).unwrap() {
            Ok(FromRobot::Hello { proto, caps, .. }) => {
                assert_eq!(proto, 1);
                assert!(caps.is_empty());
            }
            other => panic!("{other:?}"),
        }
        let ack = br#"{"t":"ack","id":4}"#;
        assert!(matches!(
            FromRobot::parse(ack).unwrap(),
            Ok(FromRobot::Ack { id: 4, .. })
        ));
        let unknown = br#"{"t":"future_thing","x":1}"#;
        assert!(FromRobot::parse(unknown).unwrap().is_err());
    }
}
