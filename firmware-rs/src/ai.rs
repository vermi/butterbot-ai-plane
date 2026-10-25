//! AI mode: the robot's one job. In AI mode Wi-Fi is up and the robot is the plane's: it runs
//! the plane's requests, reports its sensors, and keeps itself safe (drive expiry and checks).
//! Out of AI mode (switched off with a short press, or the plane out of reach) Wi-Fi is off; the
//! robot goes back into AI mode on its own, quietly, unless it was switched off on purpose.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use bbproto::ctrl::{LedMode, LedTarget, Rhythm};
use bbproto::{FromRobot, ToRobot};
use serde_json::{json, Value};

mod handheld;

use crate::audio::{Mic, Phrase, Speaker, SpeakerEvent};
use crate::base::{Base, BaseEvent, Charge, Prox};
use crate::battery::{Battery, BatteryEvent, Reading, LEVEL_CRITICAL};
use crate::ble::{Ble, BleEvent};
use crate::camera::Camera;
use crate::led::{rhythm, Led};
use crate::link::{Link, LinkEvent};
use crate::motion::{Motion, MotionEvent};
use crate::net::Net;
use crate::ota;
use crate::phone::{Phone, PhoneEvent};
use crate::power::{self, Boot, Button};
use crate::remote::{self, AiStatus, Home};
use crate::store::{LinkConfig, Store};
use crate::util::{self, millis, sleep_ms};

pub enum Event {
    Button(Button),
    Base(BaseEvent),
    Battery(BatteryEvent),
    Motion(MotionEvent),
    Link(LinkEvent),
    Speaker(SpeakerEvent),
    Ble(BleEvent),
    Phone(PhoneEvent),
}

impl From<BleEvent> for Event {
    fn from(e: BleEvent) -> Self {
        Event::Ble(e)
    }
}
impl From<PhoneEvent> for Event {
    fn from(e: PhoneEvent) -> Self {
        Event::Phone(e)
    }
}

impl From<Button> for Event {
    fn from(b: Button) -> Self {
        Event::Button(b)
    }
}
impl From<BaseEvent> for Event {
    fn from(e: BaseEvent) -> Self {
        Event::Base(e)
    }
}
impl From<BatteryEvent> for Event {
    fn from(e: BatteryEvent) -> Self {
        Event::Battery(e)
    }
}
impl From<MotionEvent> for Event {
    fn from(e: MotionEvent) -> Self {
        Event::Motion(e)
    }
}
impl From<SpeakerEvent> for Event {
    fn from(e: SpeakerEvent) -> Self {
        Event::Speaker(e)
    }
}
impl From<LinkEvent> for Event {
    fn from(e: LinkEvent) -> Self {
        Event::Link(e)
    }
}

// Status LED rhythms the robot shows on its own; the plane picks the others.
pub const BOOT: Rhythm = Rhythm::steady(Rhythm::BRIGHT);
/// A short tick every second.
const CONNECTING: Rhythm = rhythm(60, 0, 1, 1000, 0.0, Rhythm::BRIGHT);
/// Three quick blinks, pause.
const TROUBLE: Rhythm = rhythm(80, 120, 3, 1600, 0.0, Rhythm::BRIGHT);
/// Until the plane picks one.
const ONLINE: Rhythm = Rhythm::steady(Rhythm::DIM);
const TALKING: Rhythm = Rhythm::steady(Rhythm::BRIGHT);
/// AI mode off: a slow blink.
const IDLE: Rhythm = rhythm(50, 1000, 1, 1050, 0.0, Rhythm::BRIGHT);
const BATTERY_CRITICAL: Rhythm = rhythm(200, 300, 1, 500, 0.0, Rhythm::BRIGHT);
pub const UPDATING: Rhythm = rhythm(100, 100, 1, 200, Rhythm::DIM, Rhythm::BRIGHT);

/// Join Wi-Fi, get an address and reach the plane (normally 3-10 s).
const CONNECT_MS: u64 = 30_000;
/// After losing the plane: how long to wait for it before leaving AI mode.
const LINK_LOSS_GRACE_MS: u64 = 15_000;
/// Still connecting after this long: show trouble.
const TROUBLE_AFTER_MS: u64 = 12_000;
/// Out of AI mode on its own: try again after this long.
const RETRY_MS: u64 = 30_000;
const WIFI_REPORT_MS: u64 = 3000;
/// Every this many Wi-Fi reports, the battery too (the plane must know about a cable).
const BATTERY_EVERY: u32 = 10;
const MAX_DRIVE_MS: u64 = 5000;

/// What the robot is made of.
pub struct Parts {
    pub link: Arc<Link>,
    pub net: Arc<Net>,
    pub base: Arc<Base>,
    pub battery: Arc<Battery>,
    pub motion: Option<Arc<Motion>>,
    pub led: Arc<Led>,
    pub camera: Arc<Camera>,
    pub mic: Option<Arc<Mic>>,
    pub speaker: Option<Arc<Speaker>>,
    pub ble: Option<Arc<Ble>>,
    pub phone: Option<Arc<Phone>>,
    pub store: Store,
}

pub struct Robot {
    link: Arc<Link>,
    net: Arc<Net>,
    base: Arc<Base>,
    battery: Arc<Battery>,
    motion: Option<Arc<Motion>>,
    led: Arc<Led>,
    camera: Arc<Camera>,
    mic: Option<Arc<Mic>>,
    speaker: Option<Arc<Speaker>>,
    ble: Option<Arc<Ble>>,
    phone: Option<Arc<Phone>>,
    store: Store,

    cfg: Option<LinkConfig>,
    ai_on: bool,
    /// AI mode is where the robot lives: back into it on its own unless switched off on purpose.
    ai_auto: bool,
    quiet: bool,
    connected_once: bool,
    entered_at: u64,
    deadline: Option<u64>,
    retry_at: Option<u64>,

    driving: bool,
    drive_forward: bool,
    drive_safe: bool,
    drive_until: u64,
    check_tilt: bool,
    check_obstacle: bool,
    front: Prox,
    bottom: Prox,

    plane_led: Rhythm,
    talking: bool,
    battery_critical: bool,
    next_wifi_report: u64,
    reports: u32,

    // The handheld remote (see `handheld`).
    ai_status: AiStatus,
    news: Home,
    news_at: u64,
    home_sent_at: u64,
    peeking: bool,
    /// Manual Override: the remote's stick drives.
    manual: bool,
    listen_until: Option<u64>,
    muted: bool,
}

impl Robot {
    pub fn new(p: Parts) -> Robot {
        let cfg = p.store.link_config();
        match &cfg {
            Some(c) => log::info!("link configured: {c:?}"),
            None => log::warn!("no link configuration stored; provision with bbplane"),
        }
        Robot {
            link: p.link,
            net: p.net,
            base: p.base,
            battery: p.battery,
            motion: p.motion,
            led: p.led,
            camera: p.camera,
            mic: p.mic,
            speaker: p.speaker,
            ble: p.ble,
            phone: p.phone,
            store: p.store,
            cfg,
            ai_on: false,
            ai_auto: true,
            quiet: false,
            connected_once: false,
            entered_at: 0,
            deadline: None,
            retry_at: None,
            driving: false,
            drive_forward: false,
            drive_safe: true,
            drive_until: 0,
            check_tilt: true,
            check_obstacle: true,
            front: Prox::Unknown,
            bottom: Prox::Unknown,
            plane_led: ONLINE,
            talking: false,
            battery_critical: false,
            next_wifi_report: 0,
            reports: 0,
            ai_status: AiStatus {
                mode: remote::MODE_NO_BRAIN,
                activity: 2,
                ..Default::default()
            },
            news: Home::default(),
            news_at: 0,
            home_sent_at: 0,
            peeking: false,
            manual: false,
            listen_until: None,
            muted: false,
        }
    }

    pub fn run(mut self, rx: Receiver<Event>, boot: Boot) -> ! {
        // Straight into AI mode; after a restart of its own (an update, say) without a word.
        if boot == Boot::PowerOn {
            self.say(Phrase::Hello, true);
        }
        self.ai_start(boot != Boot::PowerOn);
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => sleep_ms(20),
            }
            self.tick();
        }
    }

    // ---------------------------------------------------------------- AI mode on/off

    fn ai_start(&mut self, quiet: bool) {
        let Some(cfg) = self.cfg.clone() else {
            log::warn!("AI mode cannot start: no Wi-Fi settings");
            if !quiet {
                self.say(Phrase::NoSettings, false);
            }
            return;
        };
        log::info!("AI mode on{}", if quiet { " (quietly)" } else { "" });
        if !quiet {
            self.say(Phrase::AiOn, false);
        }
        self.ai_on = true;
        self.quiet = quiet;
        self.connected_once = false;
        self.entered_at = millis();
        self.deadline = Some(millis() + CONNECT_MS);
        self.retry_at = None;
        self.plane_led = ONLINE;
        self.ai_status = AiStatus {
            mode: remote::MODE_NO_BRAIN,
            activity: 2,
            ..Default::default()
        };
        self.link.configure(cfg.clone());
        if let Err(e) = self.net.up(&cfg, self.store.channel()) {
            log::error!("wifi: {e}");
        }
        self.link.set_wanted(true);
    }

    fn ai_stop(&mut self) {
        log::info!("AI mode off");
        self.stop_motors();
        self.streams_off();
        self.link.set_wanted(false);
        // Queued replies (an exit_remote ack) go out before Wi-Fi goes down.
        for _ in 0..30 {
            if !self.link.is_online() {
                break;
            }
            sleep_ms(10);
        }
        self.net.down();
        self.ai_on = false;
        self.deadline = None;
        self.peeking = false;
        // The remote drops AI mode's entries.
        self.send_ai_status();
        self.send_home();
    }

    fn tick(&mut self) {
        let now = millis();
        if let Some(ch) = self.net.take_joined_channel() {
            self.store.set_channel(ch);
        }
        self.show_led();

        if self.ai_on && self.deadline.is_some_and(|d| now > d) {
            if self.quiet && !self.connected_once {
                log::info!("AI mode retry failed; trying again later");
            } else if self.connected_once {
                log::warn!("lost the computer");
                self.say(Phrase::LostPlane, true);
            } else {
                log::warn!("can't reach the computer");
                self.say(Phrase::NoPlane, true);
            }
            self.ai_stop();
            if self.ai_auto {
                self.retry_at = Some(now + RETRY_MS);
            }
        }
        if !self.ai_on && self.ai_auto && self.retry_at.is_some_and(|t| now >= t) {
            self.ai_start(true);
        }

        if self.driving && now >= self.drive_until {
            self.stop_driving("watchdog");
        }
        self.handheld_tick();

        if self.link.is_online() && now >= self.next_wifi_report {
            self.next_wifi_report = now + WIFI_REPORT_MS;
            let (free, block) = util::internal_ram();
            if let Some(rssi) = Net::rssi() {
                self.link.send(&FromRobot::Wifi {
                    rssi,
                    ram_free: Some(free),
                    ram_block: Some(block),
                });
            }
            self.reports += 1;
            if self.reports >= BATTERY_EVERY {
                self.reports = 0;
                self.send_battery(self.battery.reading());
            }
        }
    }

    fn show_led(&self) {
        let want = if self.battery_critical {
            BATTERY_CRITICAL
        } else if !self.ai_on {
            IDLE
        } else if self.talking {
            TALKING
        } else if !self.link.is_online() {
            if self.connected_once || millis() - self.entered_at > TROUBLE_AFTER_MS {
                TROUBLE
            } else {
                CONNECTING
            }
        } else {
            self.plane_led
        };
        self.led.show(want);
    }

    // ---------------------------------------------------------------- events

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Link(LinkEvent::Online) => {
                self.deadline = None;
                ota::confirm();
                power::ai_mode_worked();
                if !self.connected_once {
                    let (free, block) = util::internal_ram();
                    log::info!(
                        "up: internal RAM free {free} (block {block}), PSRAM free {}, {} Bluetooth connection(s), {} phone",
                        util::psram_free(),
                        self.ble.as_ref().map_or(0, |b| b.connections()),
                        match self.phone.as_ref().and_then(|p| p.kind()) {
                            Some(crate::phone::Kind::Iphone) => "iPhone",
                            Some(crate::phone::Kind::Android) => "Android",
                            None => "no",
                        }
                    );
                }
                self.connected_once = true;
                self.status_snapshot();
            }
            Event::Link(LinkEvent::Offline) => {
                // Never keep driving without the plane; give it a moment to come back.
                self.stop_motors();
                self.streams_off();
                if self.ai_on {
                    self.deadline = Some(millis() + LINK_LOSS_GRACE_MS);
                }
            }
            Event::Link(LinkEvent::PlaneHello) => self.status_snapshot(),
            Event::Link(LinkEvent::Request(msg)) => self.request(msg),
            Event::Button(b) => self.button(b),
            Event::Base(e) => self.base_event(e),
            Event::Battery(BatteryEvent::Changed(r)) => self.battery_changed(r),
            Event::Motion(MotionEvent::Motion(kind)) => {
                log::info!("motion: {}", kind.name());
                self.link.send(&FromRobot::Motion {
                    kind: kind.name().into(),
                });
            }
            Event::Motion(MotionEvent::SetDown) => self.base.rebaseline(),
            Event::Ble(e) => self.ble_event(e),
            Event::Phone(e) => self.phone_event(e),
            Event::Speaker(SpeakerEvent::Playing { on, underruns }) => {
                self.talking = on;
                // The remote's face moves its mouth.
                self.send_ai_status();
                self.link.send(&FromRobot::Speaker {
                    playing: on,
                    buffered_ms: 0,
                    underruns,
                });
            }
        }
    }

    /// One of the robot's own phrases; `wait` until it has been said.
    fn say(&mut self, p: Phrase, wait: bool) {
        if let Some(s) = &self.speaker {
            s.say(p);
            if wait {
                // Light the LED meanwhile: this thread runs it.
                self.talking = true;
                self.show_led();
                s.wait_idle(5000);
            }
        }
    }

    /// The plane is gone or AI mode ends: no streams.
    fn streams_off(&mut self) {
        self.camera.stop();
        if let Some(m) = &self.mic {
            m.set(false, true, 1.0);
        }
        if let Some(s) = &self.speaker {
            s.abort();
        }
    }

    /// Who we are and what we see, for a plane that just answered or restarted.
    fn status_snapshot(&mut self) {
        self.link.send_hello();
        self.link.send(&FromRobot::Mode {
            mode: "remote".into(),
        });
        self.send_battery(self.battery.reading());
        self.base.request_proximity();
    }

    fn send_battery(&self, r: Reading) {
        self.link.send(&FromRobot::Battery {
            pct: r.pct,
            level: r.level,
            charging: r.charge.name().into(),
        });
    }

    fn send_proximity(&self) {
        self.link.send(&FromRobot::Proximity {
            front: self.front.name().into(),
            bottom: self.bottom.name().into(),
        });
    }

    fn battery_changed(&mut self, r: Reading) {
        self.send_battery(r);
        self.send_remote_battery(r);
        self.send_home();
        if r.level == LEVEL_CRITICAL && r.charge == Charge::Unplugged {
            log::error!("battery critical while unplugged: switching off");
            self.battery_critical = true;
            self.shutdown();
        }
    }

    fn base_event(&mut self, e: BaseEvent) {
        match e {
            BaseEvent::Reading { front, bottom } => {
                self.front = front;
                if bottom != Prox::Unknown {
                    self.bottom = bottom;
                    if let Some(m) = &self.motion {
                        m.set_lifted(bottom == Prox::Uncovered);
                    }
                }
                self.send_proximity();
            }
            BaseEvent::Front(covered) => {
                self.front = if covered {
                    Prox::Covered
                } else {
                    Prox::Uncovered
                };
                if self.check_obstacle
                    && covered
                    && self.driving
                    && self.drive_forward
                    && self.drive_safe
                {
                    self.stop_driving("obstacle");
                }
                self.send_proximity();
            }
            BaseEvent::Bottom(covered) => {
                // Like the stock movement routines, driving ignores the floor sensor.
                self.bottom = if covered {
                    Prox::Covered
                } else {
                    Prox::Uncovered
                };
                if let Some(m) = &self.motion {
                    m.set_lifted(!covered);
                }
                self.send_proximity();
            }
            BaseEvent::Charge(c) => {
                if let Some(r) = self.battery.set_charge(c) {
                    log::info!("charger: {}", c.name());
                    self.send_battery(r);
                    self.send_remote_battery(r);
                    self.send_home();
                }
            }
        }
    }

    fn button(&mut self, b: Button) {
        match b {
            Button::Press => self.link.send(&FromRobot::Button {
                action: "press".into(),
            }),
            Button::Release => self.link.send(&FromRobot::Button {
                action: "release".into(),
            }),
            Button::Short => {
                if self.ai_on {
                    // Switched off on purpose: stays off until the next short press.
                    self.ai_auto = false;
                    self.stop_motors();
                    self.say(Phrase::AiOff, true);
                    self.ai_stop();
                } else {
                    self.ai_auto = true;
                    self.ai_start(false);
                }
            }
            Button::Long => {
                log::info!("button: switching off");
                self.shutdown();
            }
        }
    }

    fn shutdown(&mut self) -> ! {
        self.stop_motors();
        self.streams_off();
        self.say(
            if self.battery_critical {
                Phrase::BatteryCritical
            } else {
                Phrase::Goodbye
            },
            true,
        );
        if !self.battery_critical {
            self.led.show(Rhythm::steady(0.0));
        }
        sleep_ms(300);
        power::power_off();
    }

    // ---------------------------------------------------------------- requests

    fn request(&mut self, msg: ToRobot) {
        match msg {
            ToRobot::Hello | ToRobot::Ping { .. } => {}
            ToRobot::EnterRemote { id } => {
                self.link.ack(id, Value::Null);
                self.link.send(&FromRobot::Mode {
                    mode: "remote".into(),
                });
            }
            ToRobot::ExitRemote { id } => {
                self.link.ack(id, Value::Null);
                self.ai_auto = false;
                self.stop_motors();
                self.say(Phrase::AiOff, true);
                self.ai_stop();
            }
            // The remote's stick has the motors during Manual Override.
            ToRobot::Drive { .. } if self.manual => self.stop_driving("manual_override"),
            ToRobot::Drive { l, r, ms, safe } => self.drive(l, r, ms, safe),
            ToRobot::Stop => self.stop_motors(),
            ToRobot::Safety { id, tilt, obstacle } => {
                self.check_tilt = tilt;
                self.check_obstacle = obstacle;
                self.link.ack(id, Value::Null);
            }
            ToRobot::Led {
                id,
                target,
                mode,
                level,
            } => self.led_request(id, target, mode, level),
            ToRobot::StatusLight(r) => self.plane_led = r,
            ToRobot::Ota {
                id,
                url,
                size,
                sha256,
            } => {
                if url.is_empty() || size == 0 || sha256.len() != 64 {
                    self.link.error(id, "ota needs url, size and sha256");
                    return;
                }
                self.stop_motors();
                self.link.ack(id, Value::Null);
                sleep_ms(300);
                ota::restart_into_update(&url, size, &sha256);
            }
            ToRobot::Restart { id } => {
                self.stop_motors();
                self.link.ack(id, Value::Null);
                sleep_ms(300);
                power::restart();
            }
            ToRobot::Scenario { id, scenario, .. } => {
                if Some(scenario) == bbproto::bb::scenario_id("shutdown") {
                    self.link.ack(id, Value::Null);
                    sleep_ms(200);
                    self.shutdown();
                }
                self.link.error(id, "no routine for this scenario");
            }
            ToRobot::Modules { id } => self.link.ack(id, json!({ "modules": [] })),
            // The robot's voice is the plane's now (it streams it to the speaker).
            ToRobot::Say { id, .. } => self
                .link
                .error(id, "no voice on the robot: speak with spk_begin"),
            ToRobot::SpkBegin { id } => match &self.speaker {
                Some(s) => {
                    s.begin();
                    self.link.ack(id, Value::Null);
                }
                None => self.link.error(id, "audio unavailable"),
            },
            ToRobot::SpkEnd => {
                if let Some(s) = &self.speaker {
                    s.end();
                }
            }
            ToRobot::SpkAbort => {
                if let Some(s) = &self.speaker {
                    s.abort();
                }
            }
            ToRobot::Volume { gain } => {
                if let Some(s) = &self.speaker {
                    s.set_gain(gain);
                }
            }
            ToRobot::Camera {
                id,
                on,
                res,
                fps,
                quality,
                opts,
            } => {
                self.camera
                    .set_stream(on, res, fps, quality, opts.xclk_mhz, opts.psram_dma);
                self.link.ack(id, Value::Null);
            }
            ToRobot::Snapshot { id, res, quality } => {
                self.camera.snapshot(&self.link, id, res, quality)
            }
            ToRobot::Mic { id, on, gate, gain } => match &self.mic {
                Some(m) => {
                    m.set(on, gate, gain);
                    self.link.ack(id, Value::Null);
                }
                None => self.link.error(id, "microphone unavailable"),
            },
            ToRobot::Phone { id, action, index } => self.phone_request(id, &action, index),
            ToRobot::AiStatus {
                mode,
                activity,
                focus_minutes,
                emotion,
                intensity,
            } => self.plane_ai_status(AiStatus {
                mode: mode.min(5),
                activity: activity.min(3),
                focus_minutes,
                speaking: false,
                emotion: emotion.min(11),
                intensity: intensity.min(100),
            }),
            ToRobot::HomeInfo {
                clock,
                next,
                seen,
                seen_secs,
                said,
                heard,
                heard_last,
                doing,
            } => self.plane_home_info(clock, next, seen, seen_secs, said, heard, heard_last, doing),
            ToRobot::PeekCaption { text } => self.plane_peek_caption(&text),
            ToRobot::IrList { id } | ToRobot::IrSend { id, .. } => {
                self.link.error(id, "no IR module inserted")
            }
        }
    }

    fn led_request(&mut self, id: u32, target: LedTarget, mode: LedMode, level: f32) {
        if target == LedTarget::Module {
            self.link.error(id, "no LED module inserted");
            return;
        }
        // Off, dim or bright; it cannot fade, so "breathe" swaps dim and the level slowly.
        self.plane_led = match mode {
            LedMode::Off => Rhythm::steady(0.0),
            LedMode::Blink => rhythm(250, 0, 1, 500, 0.0, level),
            LedMode::Breathe => rhythm(1000, 0, 1, 2000, Rhythm::DIM, level),
            LedMode::On => Rhythm::steady(level),
        };
        self.link.ack(id, Value::Null);
    }

    // ---------------------------------------------------------------- driving

    fn stop_motors(&mut self) {
        self.base.stop();
        self.driving = false;
    }

    fn stop_driving(&mut self, reason: &str) {
        self.stop_motors();
        log::info!("drive stopped: {reason}");
        self.link.send(&FromRobot::DriveStopped {
            reason: reason.into(),
        });
    }

    /// The stock autonomous-move checks, made when a movement starts (or turns forward): not on
    /// a cable, upright, and nothing in front when going forward.
    fn refusal(&self, forward: bool) -> Option<&'static str> {
        if self.battery.reading().charge != Charge::Unplugged {
            return Some("plugged_in");
        }
        if self.check_tilt && self.motion.as_ref().is_some_and(|m| m.tipped()) {
            return Some("tipped_over");
        }
        if self.check_obstacle && forward && self.front == Prox::Covered {
            return Some("obstacle");
        }
        None
    }

    fn drive(&mut self, l: i8, r: i8, ms: u16, safe: bool) {
        let (l, r) = (l.clamp(-100, 100), r.clamp(-100, 100));
        if l == 0 && r == 0 {
            if self.driving {
                self.stop_motors();
            }
            return;
        }
        // Direct control (safe = false) is like the RC controller: no checks. The plane refreshes
        // a drive every 250 ms; re-checking each time would trip on motor noise.
        let forward = l as i16 + r as i16 > 0;
        if safe && (!self.driving || !self.drive_safe || (forward && !self.drive_forward)) {
            if let Some(reason) = self.refusal(forward) {
                self.stop_driving(reason);
                return;
            }
        }
        self.base.set_motors(l, r);
        self.driving = true;
        self.drive_forward = forward;
        self.drive_safe = safe;
        self.drive_until = millis() + (ms as u64).min(MAX_DRIVE_MS);
    }
}
