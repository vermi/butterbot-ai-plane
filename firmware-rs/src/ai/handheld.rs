//! AI mode and the handheld remote (over Bluetooth) and the phone: the remote's buttons and menu
//! go to the plane, Manual Override drives straight from its stick, and the remote shows AI mode's
//! status, the plane's news and the camera (the plane streams that to it over Wi-Fi).

use bbproto::FromRobot;
use serde_json::{json, Value};

use super::Robot;
use crate::audio::Phrase;
use crate::battery::Reading;
use crate::ble::BleEvent;
use crate::clock;
use crate::phone::{Kind, MediaState, PhoneEvent, CATEGORIES};
use crate::remote::{self, ai, idle, state, Command, Home};
use crate::util::millis;

/// The remote's home screen is refreshed this often (the battery moves on).
const HOME_REFRESH_MS: u64 = 60_000;
/// After the remote subscribes, the welcome goes again this much later.
const REWELCOME_MS: u64 = 2000;
/// How often to check that the robot still advertises.
const ADV_CHECK_MS: u64 = 5000;
/// The remote shows "listening" this long after its Listen button, at most.
const LISTEN_WINDOW_MS: u64 = 12_000;

/// At most `max` bytes of `s`, cut at a character boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

impl Robot {
    pub(super) fn ble_event(&mut self, e: BleEvent) {
        match e {
            BleEvent::Connected(conn) => {
                log::info!("bluetooth: connection {conn}");
                if let Some(p) = &self.phone {
                    p.connected(conn);
                }
            }
            BleEvent::Disconnected(conn) => {
                if let Some(p) = &self.phone {
                    p.disconnected(conn);
                }
            }
            BleEvent::Encrypted(conn) => {
                // Settled: slow it down (the remote speeds up for Manual Override).
                if let Some(b) = &self.ble {
                    b.pace(conn, false);
                }
                if let Some(p) = &self.phone {
                    p.encrypted(conn);
                }
            }
            BleEvent::ComSubscribed(true) => {
                log::info!("remote connected");
                self.remote_welcome();
                // The remote is still between its pairing and home screens when it subscribes,
                // and nothing listens there: say it all again once it has settled.
                self.rewelcome_at = Some(millis() + REWELCOME_MS);
            }
            BleEvent::ComSubscribed(false) => {
                log::info!("remote gone");
                self.peeking = false;
                if self.manual {
                    // Nobody holds the stick any more.
                    self.manual = false;
                    self.stop_motors();
                }
            }
            BleEvent::Com(packet) => {
                if let Some(c) = remote::parse(&packet) {
                    self.remote_command(c);
                }
            }
            BleEvent::UartSubscribed(conn, on) => {
                if let Some(p) = &self.phone {
                    p.uart_subscribed(conn, on);
                }
            }
            BleEvent::Uart(data) => {
                if let Some(p) = &self.phone {
                    p.uart(data);
                }
            }
        }
    }

    pub(super) fn phone_event(&mut self, e: PhoneEvent) {
        match e {
            PhoneEvent::Connected(connected) => {
                self.link.send(&FromRobot::PhoneLink { connected });
            }
            PhoneEvent::Call(from) => {
                self.link.send(&FromRobot::PhoneCall {
                    from: cut(&from, 80).to_string(),
                });
            }
            PhoneEvent::Media => self.send_home(),
        }
    }

    fn remote_send(&self, state: u32, action: u32, data: &[u8]) {
        if let Some(b) = &self.ble {
            b.com_send(state, action, data);
        }
    }

    fn remote_button(&self, button: &str, extra: Value) {
        let mut v = json!({ "t": "remote", "button": button });
        if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
            o.extend(e);
        }
        if let Ok(msg) = serde_json::from_value::<FromRobot>(v) {
            self.link.send(&msg);
        }
    }

    /// What a remote that just connected needs: AI mode's status, the battery, not muted, the
    /// home screen, and nothing on its screen.
    fn remote_welcome(&mut self) {
        self.send_ai_status();
        self.send_remote_battery(self.battery.reading());
        self.remote_send(state::IDLE, idle::SHUT_UP, &[0]);
        self.send_home();
        self.remote_send(state::IDLE, idle::NONE, &[]);
    }

    pub(super) fn send_ai_status(&self) {
        let mut s = if self.ai_on {
            self.ai_status
        } else {
            remote::AiStatus::default()
        };
        s.speaking = self.talking;
        self.remote_send(state::AI, ai::STATUS, &s.bytes());
    }

    pub(super) fn send_remote_battery(&self, r: Reading) {
        self.remote_send(state::IDLE, idle::BATTERY_LEVEL, &[r.level, r.charge as u8]);
    }

    pub(super) fn send_home(&mut self) {
        self.home_sent_at = millis();
        if !self.ble.as_ref().is_some_and(|b| b.remote_connected()) {
            return;
        }
        let mut h = if self.ai_on {
            self.news.clone()
        } else {
            Home::default()
        };
        if self.ai_on && !h.seen.is_empty() {
            h.seen_secs = h
                .seen_secs
                .saturating_add(((millis() - self.news_at) / 1000).min(u16::MAX as u64) as u16);
        }
        h.percent = self.battery.reading().pct.clamp(0.0, 100.0) as u8;
        h.clock = clock::now();
        if let Some(p) = &self.phone {
            let m = p.media_info();
            if m.state == MediaState::Playing {
                h.playing = if m.artist.is_empty() {
                    m.title
                } else {
                    format!("{} - {}", m.title, m.artist)
                };
            }
        }
        self.remote_send(state::IDLE, idle::HOME, &h.bytes());
    }

    /// The remote's camera view: it joins the home Wi-Fi and the plane streams to it.
    fn peek(&mut self, on: bool) {
        self.peeking = on;
        if on {
            if let (Some(cfg), Some(ip)) = (&self.cfg, self.link.plane_ip()) {
                let d = remote::peek_wifi(&cfg.ssid, &cfg.password, ip);
                self.remote_send(state::AI, ai::PEEK_WIFI, &d);
            }
        }
        self.remote_button(
            "scenario",
            json!({ "scenario": remote::SCENARIO_AI_PEEK, "arg": on as u8 }),
        );
    }

    fn remote_command(&mut self, c: Command) {
        match c {
            Command::LinkConfig(cfg) => {
                let ok = cfg.as_ref().is_some_and(|c| self.store.set_link_config(c));
                self.remote_send(state::IDLE, remote::LINK_CONFIG_ACK, &[ok as u8]);
                if let Some(cfg) = cfg.filter(|_| ok) {
                    log::info!("new link configuration: {cfg:?}");
                    self.cfg = Some(cfg);
                    self.store.set_channel(0); // perhaps another network: scan for it
                    if self.ai_on {
                        self.ai_stop();
                    }
                    self.ai_auto = true;
                    self.ai_start(false);
                }
            }
            Command::LinkWake => {
                let ok = self.cfg.is_some();
                self.remote_send(state::IDLE, remote::LINK_WAKE_ACK, &[ok as u8]);
                if ok && !self.ai_on {
                    self.ai_auto = true;
                    self.ai_start(false);
                }
            }
            // The C firmware's RAM thresholds for Wi-Fi: nothing to tune here.
            Command::LinkTune => self.remote_send(state::IDLE, remote::LINK_TUNE_ACK, &[1]),
            Command::ShutUp => {
                if !self.ai_on {
                    // Out of AI mode the button mutes, as it did on the stock robot.
                    self.muted = !self.muted;
                    if let Some(s) = &self.speaker {
                        s.set_gain(if self.muted { 0.0 } else { 0.8 });
                    }
                    self.remote_send(state::IDLE, idle::SHUT_UP, &[self.muted as u8]);
                    return;
                }
                // Talking: stop now, and the plane drops the rest. Quiet: focus mode.
                let talking = self.talking || self.speaker.as_ref().is_some_and(|s| s.playing());
                if talking {
                    if let Some(s) = &self.speaker {
                        s.abort();
                    }
                }
                self.remote_button("shut_up", json!({ "talking": talking }));
            }
            Command::Poke => self.remote_button("poke", json!({})),
            Command::Summon => self.remote_button("summon", json!({})),
            Command::Listen => {
                self.remote_button("listen", json!({}));
                // The remote's own listening window, as stock showed for voice commands.
                self.remote_send(state::LISTEN, 0, &[0]);
                self.listen_until = Some(millis() + LISTEN_WINDOW_MS);
            }
            Command::Scenario(s, arg) if s == remote::SCENARIO_AI_PEEK => self.peek(arg != 0),
            Command::Scenario(s, _) if s == remote::SCENARIO_SHUTDOWN => self.shutdown(),
            Command::Scenario(s, arg) => {
                // Every menu pick is a request to the plane's agent; it answers out loud.
                self.remote_button("scenario", json!({ "scenario": s, "arg": arg }));
                self.remote_send(state::IDLE, idle::NONE, &[]);
            }
            Command::EnterRc => {
                log::info!("manual override");
                self.set_manual(true);
                self.stop_motors();
                self.remote_button("override", json!({ "on": true }));
                self.say(Phrase::ManualOverride, false);
            }
            Command::Drive(x, y) => {
                // The stick drives the motors directly, without checks, like stock RC mode.
                self.set_manual(true);
                self.driving = false;
                let (l, r) = remote::joystick_motors(x, y);
                self.base.set_motors(l, r);
            }
            Command::ExitRc => {
                log::info!("manual override ends");
                self.set_manual(false);
                self.stop_motors();
                self.remote_button("override", json!({ "on": false }));
                self.say(Phrase::BackToAi, false);
            }
            Command::RcSound => {}
            Command::Other(c) => self.remote_button("other", json!({ "command": c })),
        }
    }

    /// Manual Override on or off; the remote's connection is fast while it drives.
    fn set_manual(&mut self, on: bool) {
        if self.manual == on {
            return;
        }
        self.manual = on;
        if let Some(b) = &self.ble {
            for conn in b.remote_conns() {
                b.pace(conn, on);
            }
        }
    }

    pub(super) fn handheld_tick(&mut self) {
        let now = millis();
        if now - self.adv_checked_at >= ADV_CHECK_MS {
            self.adv_checked_at = now;
            if let Some(b) = &self.ble {
                b.keep_advertising();
            }
        }
        if self.listen_until.is_some_and(|t| now >= t) {
            self.listen_until = None;
            self.remote_send(state::IDLE, idle::NONE, &[]);
        }
        if self.rewelcome_at.is_some_and(|t| now >= t) {
            self.rewelcome_at = None;
            self.remote_welcome();
        }
        if now - self.home_sent_at >= HOME_REFRESH_MS {
            // With AI mode's status: the plane sends that only when it changes.
            self.send_ai_status();
            self.send_home();
        }
    }

    // ---------------------------------------------------------------- from the plane

    pub(super) fn plane_ai_status(&mut self, s: remote::AiStatus) {
        // The agent has what was said (or the window closed): the listening window goes.
        if self.listen_until.is_some() && s.mode != remote::MODE_ATTENTIVE {
            self.listen_until = None;
            self.remote_send(state::IDLE, idle::NONE, &[]);
        }
        self.ai_status = s;
        self.send_ai_status();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn plane_home_info(
        &mut self,
        c: [u16; 6],
        next: String,
        seen: String,
        seen_secs: u32,
        said: String,
        heard: String,
        heard_last: bool,
        doing: String,
    ) {
        clock::set_local(
            c[0], c[1] as u8, c[2] as u8, c[3] as u8, c[4] as u8, c[5] as u8,
        );
        self.news = Home {
            next,
            seen,
            seen_secs: seen_secs.min(u16::MAX as u32) as u16,
            said,
            heard,
            heard_last,
            doing,
            ..Default::default()
        };
        self.news_at = millis();
        self.send_home();
    }

    pub(super) fn plane_peek_caption(&self, text: &str) {
        if self.peeking {
            self.remote_send(state::AI, ai::PEEK_CAPTION, &remote::caption(text));
        }
    }

    /// The plane asks about the phone: `status`, a page of `notifications`, or a music control.
    pub(super) fn phone_request(&mut self, id: u32, action: &str, index: u8) {
        let Some(phone) = self.phone.clone() else {
            self.link.error(id, "the phone link is not available");
            return;
        };
        let kind = phone.kind();
        match action {
            "play" | "pause" | "next" | "previous" => {
                let a = match action {
                    "play" => "play",
                    "pause" => "pause",
                    "next" => "next",
                    _ => "previous",
                };
                phone.media(a);
            }
            "notifications" => {
                let all = phone.readable();
                let mut items = Vec::new();
                let mut size = 0;
                let mut i = index as usize;
                while i < all.len() {
                    let n = &all[i];
                    let item = json!({
                        "app": cut(&n.app, 40),
                        "title": cut(&n.title, 80),
                        "message": cut(&n.message, 240),
                        "category": CATEGORIES.get(n.category as usize).copied().unwrap_or("other"),
                    });
                    let len = item.to_string().len();
                    if size + len > 1000 && !items.is_empty() {
                        break;
                    }
                    size += len;
                    items.push(item);
                    i += 1;
                }
                let next = if i < all.len() { json!(i) } else { Value::Null };
                self.link.ack(
                    id,
                    json!({ "connected": kind.is_some(), "total": all.len(), "next": next, "notifications": items }),
                );
                return;
            }
            "" | "status" => {}
            _ => {
                self.link.error(id, "unknown phone action");
                return;
            }
        }
        let m = phone.media_info();
        let state = match m.state {
            MediaState::Playing => "playing",
            MediaState::Paused => "paused",
            MediaState::Stopped => "stopped",
        };
        let kind = match kind {
            Some(Kind::Iphone) => json!("iphone"),
            Some(Kind::Android) => json!("android"),
            None => Value::Null,
        };
        self.link.ack(
            id,
            json!({
                "connected": !kind.is_null(),
                "phone": kind,
                "notifications": phone.readable().len(),
                "media": {
                    "state": state,
                    "title": cut(&m.title, 120),
                    "artist": cut(&m.artist, 120),
                    "album": cut(&m.album, 120),
                    "app": cut(&m.app, 60),
                },
            }),
        );
    }
}
