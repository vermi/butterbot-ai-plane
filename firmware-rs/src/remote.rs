//! The handheld remote's protocol over the Com service (`ButterBot-Common`: BBData.h,
//! CtrlData.h). The remote writes `Ctrl::Command` (u32) and a payload; the robot notifies
//! `BB { state u32, action u32, size u32 }` and a payload. Little endian, packed as on the ESP32.

use crate::store::LinkConfig;

pub mod state {
    pub const IDLE: u32 = 0;
    pub const LISTEN: u32 = 1;
    pub const AI: u32 = 4;
}

pub mod idle {
    pub const NONE: u32 = 0;
    pub const BATTERY_LEVEL: u32 = 21;
    pub const SHUT_UP: u32 = 22;
    pub const HOME: u32 = 24;
}

pub mod ai {
    pub const STATUS: u32 = 0;
    pub const PEEK_WIFI: u32 = 1;
    pub const PEEK_CAPTION: u32 = 2;
}

/// `BB::Action::Scenario` values the robot handles itself.
pub const SCENARIO_SHUTDOWN: u32 = 32;
pub const SCENARIO_AI_PEEK: u32 = 48;

/// Answers to the plane's provisioning commands: state Idle, these actions, payload u8 ok.
pub const LINK_CONFIG_ACK: u32 = 100;
pub const LINK_WAKE_ACK: u32 = 101;
pub const LINK_TUNE_ACK: u32 = 102;

#[derive(Debug)]
pub enum Command {
    EnterRc,
    ExitRc,
    Listen,
    Summon,
    ShutUp,
    Poke,
    Drive(i8, i8),
    RcSound,
    Scenario(u32, u8),
    /// None: not a valid configuration.
    LinkConfig(Option<LinkConfig>),
    LinkWake,
    LinkTune,
    Other(u32),
}

pub fn parse(p: &[u8]) -> Option<Command> {
    if p.len() < 4 {
        return None;
    }
    let cmd = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let d = &p[4..];
    Some(match cmd {
        0 => Command::EnterRc,
        1 => Command::ExitRc,
        2 => Command::Listen,
        3 => Command::Summon,
        4 => Command::ShutUp,
        5 => Command::Poke,
        6 if d.len() >= 2 => Command::Drive(d[0] as i8, d[1] as i8),
        7 => Command::RcSound,
        8 if d.len() >= 5 => Command::Scenario(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), d[4]),
        9 => Command::LinkConfig(LinkConfig::parse(d.get(..162)?)),
        10 => Command::LinkWake,
        11 => Command::LinkTune,
        other => Command::Other(other),
    })
}

/// `AIStatusData`: mode (0 off, 1 listening, 2 attentive, 3 thinking, 4 focus, 5 no brain),
/// activity, minutes of focus left, speaking, emotion, intensity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AiStatus {
    pub mode: u8,
    pub activity: u8,
    pub focus_minutes: u8,
    pub speaking: bool,
    pub emotion: u8,
    pub intensity: u8,
}

pub const MODE_ATTENTIVE: u8 = 2;
pub const MODE_NO_BRAIN: u8 = 5;

impl AiStatus {
    pub fn bytes(&self) -> [u8; 6] {
        [
            self.mode,
            self.activity,
            self.focus_minutes,
            self.speaking as u8,
            self.emotion,
            self.intensity,
        ]
    }
}

/// The remote's home screen (`HomeInfoData`): battery, clock, and in AI mode the plane's news.
#[derive(Debug, Clone, Default)]
pub struct Home {
    pub percent: u8,
    pub clock: Option<crate::clock::Local>,
    pub next: String,
    pub seen: String,
    pub seen_secs: u16,
    pub said: String,
    pub playing: String,
    pub heard: String,
    pub heard_last: bool,
    pub doing: String,
}

/// Printable ASCII only: the remote's font has nothing else.
fn ascii(s: &str) -> String {
    s.chars().filter(|c| (' '..='~').contains(c)).collect()
}

fn put_str(out: &mut Vec<u8>, s: &str, max: usize) {
    let s = ascii(s);
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    out.push(n as u8);
    out.extend_from_slice(&s.as_bytes()[..n]);
}

impl Home {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(400);
        b.push(self.percent);
        let c = self.clock;
        b.push(c.is_some() as u8);
        let c = c.unwrap_or(crate::clock::Local {
            year: 0,
            month: 0,
            day: 0,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
        });
        b.extend_from_slice(&c.year.to_le_bytes());
        b.extend_from_slice(&[c.month, c.day, c.weekday, c.hour, c.minute, c.second]);
        put_str(&mut b, &self.next, 48);
        put_str(&mut b, &self.seen, 32);
        b.extend_from_slice(&self.seen_secs.to_le_bytes());
        put_str(&mut b, &self.said, 100);
        put_str(&mut b, &self.playing, 64);
        put_str(&mut b, &self.heard, 80);
        b.push(self.heard_last as u8);
        put_str(&mut b, &self.doing, 24);
        b
    }
}

/// `PeekWiFiData`: the home Wi-Fi and the plane's address, for the remote's camera view.
pub fn peek_wifi(ssid: &str, password: &str, plane: std::net::Ipv4Addr) -> Vec<u8> {
    let mut b = vec![0u8; 104];
    let s = ssid.as_bytes();
    b[..s.len().min(32)].copy_from_slice(&s[..s.len().min(32)]);
    let p = password.as_bytes();
    b[33..33 + p.len().min(64)].copy_from_slice(&p[..p.len().min(64)]);
    // Network byte order; offset 100 after the struct's padding.
    b[100..104].copy_from_slice(&plane.octets());
    b
}

pub fn caption(text: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, text, 64);
    b
}

/// The stick to track speeds, as the stock RC mode drove: a small dead zone, the faster track
/// at the stick's push (doubled, capped), the other in proportion.
pub fn joystick_motors(x: i8, y: i8) -> (i8, i8) {
    let x = (x as f32 / 100.0).clamp(-1.0, 1.0);
    let y = (y as f32 / 100.0).clamp(-1.0, 1.0);
    let push = (x * x + y * y).sqrt();
    if push < 0.08 {
        return (0, 0);
    }
    let (left, right) = if y >= 0.0 {
        (y + x, y - x)
    } else {
        (y - x, y + x)
    };
    let larger = left.abs().max(right.abs());
    let speed = (push * 2.0).min(1.0) * 100.0;
    (
        (left / larger * speed).round() as i8,
        (right / larger * speed).round() as i8,
    )
}
