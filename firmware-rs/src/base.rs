//! The motor/base board: a microcontroller at I2C 0x69 with the motors, the front and floor IR
//! proximity sensors and the charger. Its firmware is fixed; this is its register interface.
//!
//! It stops the motors after 500 ms without a motor write, so moving motors are rewritten every
//! 250 ms. Proximity scanning is paused around a motor change and rebaselined 100 ms later (the
//! motors disturb the IR readings). Events (proximity, charger) are queued on the board and
//! signalled on its interrupt line.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use esp_idf_svc::sys::{self, EspError};

use crate::i2c::{Bus, Dev};
use crate::pins;
use crate::util::{self, millis, sleep_ms, Stack};

const REG_IDENTIFY: u8 = 0x00;
const REG_RESET: u8 = 0x01;
const REG_MOTOR_L: u8 = 0x02;
const REG_PROX_SCANNING: u8 = 0x04;
const REG_PROX_REQUEST: u8 = 0x09;
const REG_CHARGE: u8 = 0x0A;
const REG_EVENT_COUNT: u8 = 0x0B;
const REG_EVENTS_GET: u8 = 0x0C;

const MOTOR_REFRESH_MS: u64 = 250;
const PROX_SETTLE_MS: u64 = 3;
const PROX_REBASELINE_MS: u64 = 100;
const RESET_DELAY_MS: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prox {
    Unknown,
    Covered,
    Uncovered,
}

impl Prox {
    fn from_raw(v: u8) -> Prox {
        match v {
            1 => Prox::Covered,
            2 => Prox::Uncovered,
            _ => Prox::Unknown,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Prox::Covered => "covered",
            Prox::Uncovered => "uncovered",
            Prox::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    Unplugged,
    Charging,
    Full,
}

impl Charge {
    fn from_raw(v: u8) -> Charge {
        match v {
            1 => Charge::Charging,
            2 => Charge::Full,
            _ => Charge::Unplugged,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Charge::Unplugged => "unplugged",
            Charge::Charging => "charging",
            Charge::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BaseEvent {
    /// Answer to `request_proximity`.
    Reading {
        front: Prox,
        bottom: Prox,
    },
    /// A sensor crossed its threshold (scanning on).
    Front(bool),
    Bottom(bool),
    Charge(Charge),
}

struct State {
    left: i8,
    right: i8,
    scanning: bool,
    written_at: u64,
    rebaseline_at: Option<u64>,
}

pub struct Base {
    dev: Dev,
    st: Mutex<State>,
}

impl Base {
    pub fn new(bus: &Bus) -> Result<Base, String> {
        if !bus.probe(pins::ADDR_BASE) {
            return Err("base board not found".into());
        }
        let dev = bus
            .device(pins::ADDR_BASE, 400_000)
            .map_err(|e| e.to_string())?;
        let id = dev.read_u8(REG_IDENTIFY).map_err(|e| e.to_string())?;
        if id != pins::ADDR_BASE as u8 {
            return Err(format!("base board id {id:#x}"));
        }
        dev.write(&[REG_RESET]).map_err(|e| e.to_string())?;
        sleep_ms(RESET_DELAY_MS);
        let cfg = sys::gpio_config_t {
            pin_bit_mask: 1u64 << pins::BASE_INT,
            mode: sys::gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
            pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_ENABLE,
            intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
        };
        unsafe { sys::gpio_config(&cfg) };
        Ok(Base {
            dev,
            st: Mutex::new(State {
                left: 0,
                right: 0,
                scanning: false,
                written_at: 0,
                rebaseline_at: None,
            }),
        })
    }

    /// Poll the interrupt line and keep moving motors alive.
    pub fn start<E: From<BaseEvent> + Send + 'static>(self: &Arc<Self>, tx: Sender<E>) {
        let base = self.clone();
        util::spawn(c"base", 4096, 7, Stack::Internal, move || loop {
            sleep_ms(10);
            base.service(&tx);
        });
    }

    fn service<E: From<BaseEvent>>(&self, tx: &Sender<E>) {
        {
            let mut st = self.st.lock().unwrap();
            let now = millis();
            if (st.left != 0 || st.right != 0) && now - st.written_at >= MOTOR_REFRESH_MS {
                self.write_motors(&mut st);
            }
            if st.rebaseline_at.is_some_and(|t| now >= t) {
                st.rebaseline_at = None;
                if st.scanning {
                    self.log(self.dev.write(&[REG_PROX_SCANNING, 0]));
                    sleep_ms(PROX_SETTLE_MS);
                    self.log(self.dev.write(&[REG_PROX_SCANNING, 1]));
                }
            }
        }
        if unsafe { sys::gpio_get_level(pins::BASE_INT) } == 0 {
            return;
        }
        for _ in 0..8 {
            let Ok(n) = self.dev.read_u8(REG_EVENT_COUNT) else {
                return;
            };
            if n == 0 {
                return;
            }
            let n = n.min(16) as usize;
            let mut raw = [0u8; 48];
            if self
                .dev
                .write_read(&[REG_EVENTS_GET, n as u8], &mut raw[..n * 3])
                .is_err()
            {
                return;
            }
            for e in raw[..n * 3].chunks_exact(3) {
                let ev = match e[0] {
                    0 => BaseEvent::Reading {
                        front: Prox::from_raw(e[1]),
                        bottom: Prox::from_raw(e[2]),
                    },
                    1 if e[1] == 0 => BaseEvent::Front(e[2] != 0),
                    1 => BaseEvent::Bottom(e[2] != 0),
                    2 => BaseEvent::Charge(Charge::from_raw(e[1])),
                    other => {
                        log::warn!("base: unknown event {other}");
                        continue;
                    }
                };
                let _ = tx.send(ev.into());
            }
        }
    }

    fn write_motors(&self, st: &mut State) {
        // The board's motors run the other way round.
        let l = st.left.saturating_neg() as u8;
        let r = st.right.saturating_neg() as u8;
        self.log(self.dev.write(&[REG_MOTOR_L, l, r]));
        st.written_at = millis();
    }

    fn log(&self, r: Result<(), EspError>) {
        if let Err(e) = r {
            log::warn!("base: i2c: {e}");
        }
    }

    /// Motor speeds -100..100, positive forward.
    pub fn set_motors(&self, left: i8, right: i8) {
        let mut st = self.st.lock().unwrap();
        if st.left == left && st.right == right {
            return;
        }
        st.left = left;
        st.right = right;
        if st.scanning {
            self.log(self.dev.write(&[REG_PROX_SCANNING, 0]));
            sleep_ms(PROX_SETTLE_MS);
        }
        self.write_motors(&mut st);
        if st.scanning {
            st.rebaseline_at = Some(millis() + PROX_REBASELINE_MS);
        }
    }

    pub fn stop(&self) {
        self.set_motors(0, 0);
    }

    pub fn set_scanning(&self, on: bool) {
        let mut st = self.st.lock().unwrap();
        if st.scanning == on {
            return;
        }
        st.scanning = on;
        self.log(self.dev.write(&[REG_PROX_SCANNING, on as u8]));
    }

    /// Restart the proximity baseline (after the robot was set down).
    pub fn rebaseline(&self) {
        let mut st = self.st.lock().unwrap();
        if st.scanning {
            st.rebaseline_at = Some(millis());
        }
    }

    /// Ask for a `Reading` event.
    pub fn request_proximity(&self) {
        let _st = self.st.lock().unwrap();
        self.log(self.dev.write(&[REG_PROX_REQUEST]));
    }

    pub fn charge(&self) -> Charge {
        let _st = self.st.lock().unwrap();
        match self.dev.read_u8(REG_CHARGE) {
            Ok(v) if v <= 2 => Charge::from_raw(v),
            Ok(v) => {
                log::warn!("base: charge state {v}");
                Charge::Unplugged
            }
            Err(e) => {
                log::warn!("base: charge state: {e}");
                Charge::Unplugged
            }
        }
    }
}
