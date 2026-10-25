//! TCA9555 I/O expander: the status LED, the speaker amplifier, the battery calibration switch and
//! the add-on card's address and detect lines.

use std::sync::Mutex;

use esp_idf_svc::sys::EspError;

use crate::i2c::{Bus, Dev};
use crate::pins::{self, exp};

const REG_INPUT: u8 = 0x00;
const REG_OUTPUT: u8 = 0x02;
const REG_POLARITY: u8 = 0x04;
const REG_DIR: u8 = 0x06;

/// Pins the firmware drives (the rest are inputs).
const OUTPUTS: [u8; 6] = [
    exp::LED,
    exp::CALIB_EN,
    exp::SPK_EN,
    exp::CTRL_3,
    exp::CTRL_5,
    exp::CTRL_4,
];

struct Regs {
    dir: [u8; 2],
    out: [u8; 2],
    /// What `set_level` last showed per pin (0 off, 1 dim, 2 on; 0xff unknown).
    shown: [u8; 16],
}

pub struct Tca {
    dev: Dev,
    regs: Mutex<Regs>,
}

fn half(pin: u8) -> usize {
    (pin >= 8) as usize
}

fn mask(pin: u8) -> u8 {
    1 << (pin % 8)
}

impl Tca {
    pub fn new(bus: &Bus) -> Result<Tca, EspError> {
        let dev = bus.device(pins::ADDR_TCA, 400_000)?;
        dev.write(&[REG_DIR, 0xff, 0xff])?;
        dev.write(&[REG_POLARITY, 0, 0])?;
        let tca = Tca {
            dev,
            regs: Mutex::new(Regs {
                dir: [0xff, 0xff],
                out: [0, 0],
                shown: [0xff; 16],
            }),
        };
        for pin in OUTPUTS {
            tca.write(pin, false);
            tca.pin_mode(pin, true);
        }
        Ok(tca)
    }

    pub fn pin_mode(&self, pin: u8, output: bool) {
        let mut r = self.regs.lock().unwrap();
        Self::pin_mode_locked(&self.dev, &mut r, pin, output);
    }

    fn pin_mode_locked(dev: &Dev, r: &mut Regs, pin: u8, output: bool) {
        let h = half(pin);
        let dir = if output {
            r.dir[h] & !mask(pin)
        } else {
            r.dir[h] | mask(pin)
        };
        if dir == r.dir[h] {
            return;
        }
        r.dir[h] = dir;
        if let Err(e) = dev.write_reg(REG_DIR + h as u8, &[dir]) {
            log::warn!("tca: direction write failed: {e}");
        }
    }

    pub fn write(&self, pin: u8, high: bool) {
        let mut r = self.regs.lock().unwrap();
        Self::write_locked(&self.dev, &mut r, pin, high);
    }

    fn write_locked(dev: &Dev, r: &mut Regs, pin: u8, high: bool) {
        let h = half(pin);
        let out = if high {
            r.out[h] | mask(pin)
        } else {
            r.out[h] & !mask(pin)
        };
        r.out[h] = out;
        if let Err(e) = dev.write_reg(REG_OUTPUT + h as u8, &[out]) {
            log::warn!("tca: output write failed: {e}");
        }
    }

    /// An LED on an expander pin has three levels: off, dim (the pin released to the expander's
    /// ~100 kOhm pull-up) and on.
    pub fn set_level(&self, pin: u8, level: u8) {
        let mut r = self.regs.lock().unwrap();
        if r.shown[pin as usize] == level {
            return;
        }
        r.shown[pin as usize] = level;
        if level == 1 {
            Self::pin_mode_locked(&self.dev, &mut r, pin, false);
            return;
        }
        Self::write_locked(&self.dev, &mut r, pin, level == 2);
        Self::pin_mode_locked(&self.dev, &mut r, pin, true);
    }

    pub fn read_all(&self) -> Result<u16, EspError> {
        let _r = self.regs.lock().unwrap();
        let mut b = [0u8; 2];
        self.dev.read_reg(REG_INPUT, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read(&self, pin: u8) -> bool {
        self.read_all()
            .map(|v| v & (1 << pin) != 0)
            .unwrap_or(false)
    }
}
