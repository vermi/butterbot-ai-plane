//! VL53L1X time-of-flight distance sensor (I2C 0x29, 16-bit register addresses), as ST's
//! ultra-lite driver sets it up: its default configuration, one ranging to calibrate, then
//! continuous ranging (100 ms timing budget, 10 Hz).

use esp_idf_svc::sys::EspError;

use crate::i2c::{Bus, Dev};
use crate::util::{millis, sleep_ms};

pub const ADDR: u16 = 0x29;

const MODEL_ID: u16 = 0x010F;
const FIRMWARE_SYSTEM_STATUS: u16 = 0x00E5;
const GPIO_HV_MUX_CTRL: u16 = 0x0030;
const GPIO_TIO_HV_STATUS: u16 = 0x0031;
const SYSTEM_INTERRUPT_CLEAR: u16 = 0x0086;
const SYSTEM_MODE_START: u16 = 0x0087;
const RESULT_RANGE_STATUS: u16 = 0x0089;
const RESULT_RANGE_MM: u16 = 0x0096;
const VHV_TIMEOUT_MACROP_LOOP_BOUND: u16 = 0x0008;

/// Registers 0x2D..=0x87 (ST's VL51L1X_DEFAULT_CONFIGURATION).
const DEFAULT_CONFIGURATION: [u8; 91] = [
    0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x02, 0x08, 0x00, 0x08, 0x10, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00, 0xff, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x0b, 0x00, 0x00, 0x02, 0x0a, 0x21,
    0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0xc8, 0x00, 0x00, 0x38, 0xff, 0x01, 0x00, 0x08, 0x00,
    0x00, 0x01, 0xcc, 0x0f, 0x01, 0xf1, 0x0d, 0x01, 0x68, 0x00, 0x80, 0x08, 0xb8, 0x00, 0x00, 0x00,
    0x00, 0x0f, 0x89, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x0f, 0x0d, 0x0e, 0x0e, 0x00,
    0x00, 0x02, 0xc7, 0xff, 0x9B, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
];

pub struct Tof {
    dev: Dev,
    /// The data-ready level (the interrupt polarity).
    ready_level: u8,
}

impl Tof {
    fn write(&self, reg: u16, data: &[u8]) -> Result<(), EspError> {
        let mut b = Vec::with_capacity(2 + data.len());
        b.extend_from_slice(&reg.to_be_bytes());
        b.extend_from_slice(data);
        self.dev.write(&b)
    }

    fn read(&self, reg: u16, out: &mut [u8]) -> Result<(), EspError> {
        self.dev.write_read(&reg.to_be_bytes(), out)
    }

    fn read_u8(&self, reg: u16) -> Result<u8, EspError> {
        let mut b = [0u8];
        self.read(reg, &mut b)?;
        Ok(b[0])
    }

    /// Set up a VL53L1X on `bus`, if one answers.
    pub fn new(bus: &Bus) -> Option<Tof> {
        if !bus.probe(ADDR) {
            return None;
        }
        let dev = bus.device(ADDR, 400_000).ok()?;
        let mut tof = Tof {
            dev,
            ready_level: 1,
        };
        let mut id = [0u8; 2];
        tof.read(MODEL_ID, &mut id).ok()?;
        if u16::from_be_bytes(id) != 0xEACC {
            log::warn!(
                "tof: model id {:#06x}, not a VL53L1X",
                u16::from_be_bytes(id)
            );
            return None;
        }
        // Booted?
        let t = millis();
        while tof.read_u8(FIRMWARE_SYSTEM_STATUS).ok()? & 0x01 == 0 {
            if millis() - t > 500 {
                return None;
            }
            sleep_ms(5);
        }
        tof.write(0x2D, &DEFAULT_CONFIGURATION).ok()?;
        tof.ready_level = if tof.read_u8(GPIO_HV_MUX_CTRL).ok()? & 0x10 != 0 {
            0
        } else {
            1
        };
        // One ranging, to calibrate.
        tof.write(SYSTEM_MODE_START, &[0x40]).ok()?;
        let t = millis();
        while !tof.ready() {
            if millis() - t > 1000 {
                return None;
            }
            sleep_ms(5);
        }
        tof.write(SYSTEM_INTERRUPT_CLEAR, &[0x01]).ok()?;
        tof.write(SYSTEM_MODE_START, &[0x00]).ok()?;
        tof.write(VHV_TIMEOUT_MACROP_LOOP_BOUND, &[0x09]).ok()?;
        tof.write(0x000B, &[0x00]).ok()?;
        tof.write(SYSTEM_MODE_START, &[0x40]).ok()?;
        log::info!("tof: VL53L1X ranging");
        Some(tof)
    }

    fn ready(&self) -> bool {
        self.read_u8(GPIO_TIO_HV_STATUS)
            .map(|s| s & 0x01 == self.ready_level)
            .unwrap_or(false)
    }

    /// The latest distance, once a new one is ready: Some(None) when nothing is in range.
    pub fn poll(&self) -> Option<Option<u16>> {
        if !self.ready() {
            return None;
        }
        let status = self.read_u8(RESULT_RANGE_STATUS).ok()? & 0x1F;
        let mut mm = [0u8; 2];
        self.read(RESULT_RANGE_MM, &mut mm).ok()?;
        let _ = self.write(SYSTEM_INTERRUPT_CLEAR, &[0x01]);
        // Device status 9: a valid range (ST's table maps it to 0); anything else is noise,
        // a signal too weak, or out of range.
        Some((status == 9).then_some(u16::from_be_bytes(mm)))
    }
}
