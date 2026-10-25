//! The add-on card slot. A card is in when DET_1 reads low and DET_2 high; six address lines
//! give its family (61: the temperature and humidity card, found by probing its sensor; 16: the
//! others, with a 3-bit sub-address on control lines CTRL_4, CTRL_5 and CTRL_6).
//!
//! - temperature & humidity: an AHT20 on the card's I2C bus;
//! - CO2 and gas: an analog level on CTRL_6;
//! - motion (PIR): a level on CTRL_1, indicator LEDs on CTRL_2;
//! - LED: PWM on CTRL_1;
//! - prototyping card: a VL53L1X distance sensor on the card's I2C bus, if fitted;
//! - IR: recognised, not driven (the IR features are gone).

use std::sync::{Arc, Mutex};

use bbproto::ctrl::LedMode;
use bbproto::FromRobot;
use esp_idf_svc::sys;
use serde_json::{json, Value};

use crate::battery::Battery;
use crate::i2c::{Bus, Dev};
use crate::link::Link;
use crate::pins::{self, exp};
use crate::tca::Tca;
use crate::tof::Tof;
use crate::util::{self, millis, sleep_ms, Stack};

const AHT20: u16 = 0x38;
/// Analog level above which the CO2 or gas card says the air is fine (as stock).
const AIR_OK_MV: f32 = 1100.0;
/// The distance is news when it moves this much.
const RANGE_STEP_MM: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TempHum,
    Co2,
    Gas,
    Motion,
    Led,
    Ir,
    PerfBoard,
    Unknown,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::TempHum => "temp_hum",
            Kind::Co2 => "co2",
            Kind::Gas => "gas",
            Kind::Motion => "motion",
            Kind::Led => "led",
            Kind::Ir => "ir",
            Kind::PerfBoard => "perfboard",
            Kind::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy)]
struct LedState {
    mode: LedMode,
    level: f32,
    since: u64,
}

struct State {
    kind: Option<Kind>,
    aht: Option<Dev>,
    tof: Option<Tof>,
    range: Option<u16>,
    motion: bool,
    led: LedState,
}

pub struct Cards {
    st: Mutex<State>,
    bus: Bus,
    tca: Arc<Tca>,
    battery: Arc<Battery>,
}

fn gpio_input(pin: i32, pull_up: bool, pull_down: bool) {
    let cfg = sys::gpio_config_t {
        pin_bit_mask: 1u64 << pin,
        mode: sys::gpio_mode_t_GPIO_MODE_INPUT,
        pull_up_en: pull_up as u32,
        pull_down_en: pull_down as u32,
        intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
    };
    unsafe { sys::gpio_config(&cfg) };
}

fn level(pin: i32) -> bool {
    unsafe { sys::gpio_get_level(pin) != 0 }
}

impl Cards {
    pub fn start(tca: Arc<Tca>, battery: Arc<Battery>, link: Arc<Link>) -> Option<Arc<Cards>> {
        let bus = match Bus::new(1, pins::I2C_CARD_SDA, pins::I2C_CARD_SCL) {
            Ok(b) => b,
            Err(e) => {
                log::error!("cards: i2c: {e}");
                return None;
            }
        };
        gpio_input(pins::CTRL_6, true, false);
        let c = Arc::new(Cards {
            st: Mutex::new(State {
                kind: None,
                aht: None,
                tof: None,
                range: None,
                motion: false,
                led: LedState {
                    mode: LedMode::Off,
                    level: 1.0,
                    since: 0,
                },
            }),
            bus,
            tca,
            battery,
        });
        let me = c.clone();
        util::spawn(c"cards", 4096, 4, Stack::Internal, move || me.run(&link));
        Some(c)
    }

    pub fn kind(&self) -> Option<Kind> {
        self.st.lock().unwrap().kind
    }

    fn inserted(&self) -> Option<u16> {
        let v = self.tca.read_all().ok()?;
        let det1 = v & (1 << exp::DET_1) != 0;
        let det2 = v & (1 << exp::DET_2) != 0;
        (!det1 && det2).then_some(v)
    }

    fn identify(&self, v: u16) -> Kind {
        let main = (v >> exp::ADDR_1) & 0x3f;
        match main {
            61 if self.bus.probe(AHT20) => Kind::TempHum,
            16 => {
                let sub = ((v >> exp::CTRL_4) & 1)
                    | (((v >> exp::CTRL_5) & 1) << 1)
                    | ((level(pins::CTRL_6) as u16) << 2);
                match sub {
                    0 => Kind::Motion,
                    1 => Kind::Co2,
                    2 => Kind::Ir,
                    3 => Kind::Led,
                    4 => Kind::PerfBoard,
                    5 => Kind::Gas,
                    _ => Kind::Unknown,
                }
            }
            _ => {
                log::info!("cards: unknown card (address {main})");
                Kind::Unknown
            }
        }
    }

    fn set_up(&self, st: &mut State, kind: Kind) {
        match kind {
            Kind::TempHum => {
                if let Ok(dev) = self.bus.device(AHT20, 100_000) {
                    // Calibrate if it says it is not.
                    let mut s = [0u8];
                    if dev.read(&mut s).is_ok() && s[0] & 0x08 == 0 {
                        let _ = dev.write(&[0xBE, 0x08, 0x00]);
                        sleep_ms(10);
                    }
                    st.aht = Some(dev);
                }
            }
            Kind::Motion => {
                gpio_input(pins::CTRL_1, false, true);
                // Its indicator LEDs off (active low).
                let cfg = sys::gpio_config_t {
                    pin_bit_mask: 1u64 << pins::CTRL_2,
                    mode: sys::gpio_mode_t_GPIO_MODE_OUTPUT,
                    pull_up_en: 0,
                    pull_down_en: 0,
                    intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
                };
                unsafe {
                    sys::gpio_config(&cfg);
                    sys::gpio_set_level(pins::CTRL_2, 1);
                }
                st.motion = level(pins::CTRL_1);
            }
            Kind::Led => {
                led_init();
                st.led = LedState {
                    mode: LedMode::Off,
                    level: 1.0,
                    since: millis(),
                };
                led_duty(0.0);
            }
            Kind::PerfBoard => st.tof = Tof::new(&self.bus),
            _ => {}
        }
    }

    fn tear_down(&self, st: &mut State) {
        if st.kind == Some(Kind::Led) {
            unsafe {
                sys::ledc_stop(
                    sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
                    sys::ledc_channel_t_LEDC_CHANNEL_1,
                    0,
                )
            };
        }
        if matches!(st.kind, Some(Kind::Led) | Some(Kind::Motion)) {
            unsafe {
                sys::gpio_reset_pin(pins::CTRL_1);
                sys::gpio_reset_pin(pins::CTRL_2);
            }
        }
        st.aht = None;
        st.tof = None;
        st.range = None;
        st.motion = false;
        st.kind = None;
    }

    fn run(&self, link: &Link) {
        let mut next_check = 0;
        let mut motion_seen = (false, 0u8);
        loop {
            sleep_ms(100);
            let now = millis();
            if now >= next_check {
                next_check = now + 500;
                let present = self.inserted();
                let kind = self.kind();
                match (present, kind) {
                    (Some(_), None) => {
                        sleep_ms(200); // contacts settle
                        let Some(v) = self.inserted() else { continue };
                        let k = self.identify(v);
                        log::info!("cards: {} card in", k.name());
                        let mut st = self.st.lock().unwrap();
                        self.set_up(&mut st, k);
                        st.kind = Some(k);
                        drop(st);
                        link.send(&FromRobot::Card {
                            kind: k.name().into(),
                            inserted: true,
                        });
                    }
                    (None, Some(k)) => {
                        log::info!("cards: {} card out", k.name());
                        self.tear_down(&mut self.st.lock().unwrap());
                        link.send(&FromRobot::Card {
                            kind: k.name().into(),
                            inserted: false,
                        });
                    }
                    _ => {}
                }
            }
            let mut st = self.st.lock().unwrap();
            match st.kind {
                Some(Kind::Motion) => {
                    // Two readings in a row decide (a bounce is one).
                    let l = level(pins::CTRL_1);
                    motion_seen = if l == motion_seen.0 {
                        (l, motion_seen.1.saturating_add(1))
                    } else {
                        (l, 1)
                    };
                    if motion_seen.1 >= 2 && l != st.motion {
                        st.motion = l;
                        drop(st);
                        link.send(&FromRobot::CardMotion { detected: l });
                    }
                }
                Some(Kind::PerfBoard) => {
                    let reading = st.tof.as_ref().and_then(|t| t.poll());
                    if let Some(mm) = reading {
                        let changed = match (st.range, mm) {
                            (Some(a), Some(b)) => a.abs_diff(b) >= RANGE_STEP_MM,
                            (a, b) => a.is_some() != b.is_some(),
                        };
                        if changed {
                            st.range = mm;
                            drop(st);
                            link.send(&FromRobot::Range { mm });
                        }
                    }
                }
                Some(Kind::Led) => {
                    let l = st.led;
                    let t = (now - l.since) as f32 / 1000.0;
                    let v = match l.mode {
                        LedMode::Off => 0.0,
                        LedMode::On => l.level,
                        LedMode::Blink => {
                            if (t * 2.0).fract() < 0.5 {
                                l.level
                            } else {
                                0.0
                            }
                        }
                        LedMode::Breathe => {
                            l.level * (0.5 - 0.5 * (t * core::f32::consts::PI).cos())
                        }
                    };
                    led_duty(v);
                }
                _ => {}
            }
        }
    }

    /// The LED card: on, off, blink or breathe, at `level` 0..1.
    pub fn set_led(&self, mode: LedMode, level: f32) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.kind != Some(Kind::Led) {
            return false;
        }
        st.led = LedState {
            mode,
            level: level.clamp(0.0, 1.0),
            since: millis(),
        };
        true
    }

    /// What the card in the slot reads, for the plane's `modules` request.
    pub fn read(&self) -> Value {
        let st = self.st.lock().unwrap();
        let Some(kind) = st.kind else {
            return json!({ "modules": [] });
        };
        let mut m = json!({ "bus": 0, "type": kind.name() });
        match kind {
            Kind::TempHum => {
                if let Some((t, h)) = st.aht.as_ref().and_then(aht_measure) {
                    m["temperature_c"] = json!((t * 10.0).round() / 10.0);
                    m["humidity"] = json!(h.round());
                }
            }
            Kind::Co2 | Kind::Gas => {
                drop(st);
                let mv = self.battery.read_aux_mv(pins::CTRL_6);
                m["reading"] = json!(mv.map(|v| v.round()));
                m["ok"] = json!(mv.map(|v| v > AIR_OK_MV));
                return json!({ "modules": [m] });
            }
            Kind::Motion => m["detected"] = json!(st.motion),
            Kind::PerfBoard => {
                if st.tof.is_some() {
                    m["tof_mm"] = json!(st.range);
                }
            }
            _ => {}
        }
        json!({ "modules": [m] })
    }
}

/// AHT20: start a measurement, wait until it is no longer busy, read humidity and temperature:
/// (temperature C, relative humidity %).
fn aht_measure(dev: &Dev) -> Option<(f32, f32)> {
    dev.write(&[0xAC, 0x33, 0x00]).ok()?;
    sleep_ms(80);
    let mut buf = [0u8; 6];
    for _ in 0..5 {
        dev.read(&mut buf).ok()?;
        if buf[0] & 0x80 == 0 {
            break;
        }
        sleep_ms(10);
    }
    let h = ((buf[1] as u32) << 12) | ((buf[2] as u32) << 4) | ((buf[3] as u32) >> 4);
    let t = (((buf[3] & 0x0f) as u32) << 16) | ((buf[4] as u32) << 8) | buf[5] as u32;
    Some((
        t as f32 * 200.0 / 1_048_576.0 - 50.0,
        h as f32 * 100.0 / 1_048_576.0,
    ))
}

fn led_init() {
    unsafe {
        let timer = sys::ledc_timer_config_t {
            speed_mode: sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
            duty_resolution: sys::ledc_timer_bit_t_LEDC_TIMER_10_BIT,
            timer_num: sys::ledc_timer_t_LEDC_TIMER_1,
            freq_hz: 5000,
            clk_cfg: sys::soc_periph_ledc_clk_src_legacy_t_LEDC_AUTO_CLK,
            deconfigure: false,
        };
        sys::ledc_timer_config(&timer);
        let mut ch: sys::ledc_channel_config_t = core::mem::zeroed();
        ch.gpio_num = pins::CTRL_1;
        ch.speed_mode = sys::ledc_mode_t_LEDC_LOW_SPEED_MODE;
        ch.channel = sys::ledc_channel_t_LEDC_CHANNEL_1;
        ch.timer_sel = sys::ledc_timer_t_LEDC_TIMER_1;
        ch.duty = 0;
        // The card's LED is on when the line is low.
        ch.flags.set_output_invert(1);
        sys::ledc_channel_config(&ch);
    }
}

fn led_duty(v: f32) {
    let duty = (v.clamp(0.0, 1.0) * 1023.0) as u32;
    unsafe {
        sys::ledc_set_duty(
            sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
            sys::ledc_channel_t_LEDC_CHANNEL_1,
            duty,
        );
        sys::ledc_update_duty(
            sys::ledc_mode_t_LEDC_LOW_SPEED_MODE,
            sys::ledc_channel_t_LEDC_CHANNEL_1,
        );
    }
}
