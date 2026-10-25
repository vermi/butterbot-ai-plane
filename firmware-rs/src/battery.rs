//! Battery level from the voltage divider on GPIO6, calibrated at boot against the board's 2.5 V
//! reference (switched in by an expander pin), smoothed, and mapped to seven levels with
//! hysteresis. The charger state comes from the base board.
//!
//! Same numbers as the stock firmware: the readings are not true millivolts, the limits are what
//! makes 0-100 % come out right.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use esp_idf_svc::sys::{self, esp, EspError};

use crate::base::Charge;
use crate::pins::{self, exp};
use crate::tca::Tca;
use crate::util::{self, sleep_ms, Stack};

const VOLT_EMPTY: f32 = 3550.0;
const VOLT_FULL: f32 = 4100.0;
const FACTOR: f32 = 4.0;
const OFFSET: f32 = 50.0;
const EMA: f32 = 0.2;
const CAL_READS: usize = 10;
const CAL_EXPECTED: f32 = 2500.0;
const SAMPLE_MS: u64 = 100;

/// Level limits in percent; level i spans LIMITS[i]..LIMITS[i + 1].
const LIMITS: [i32; 8] = [0, 4, 15, 30, 50, 70, 90, 100];
const MARGIN: i32 = 3;
const LEVELS: usize = LIMITS.len() - 1;

pub const LEVEL_CRITICAL: u8 = 0;

struct Adc {
    unit: sys::adc_oneshot_unit_handle_t,
    chan: sys::adc_channel_t,
    cali: sys::adc_cali_handle_t,
}

unsafe impl Send for Adc {}

impl Adc {
    fn new(gpio: i32) -> Result<Adc, EspError> {
        let mut unit_id: sys::adc_unit_t = 0;
        let mut chan: sys::adc_channel_t = 0;
        esp!(unsafe { sys::adc_oneshot_io_to_channel(gpio, &mut unit_id, &mut chan) })?;
        let mut init: sys::adc_oneshot_unit_init_cfg_t = unsafe { core::mem::zeroed() };
        init.unit_id = unit_id;
        let mut unit = core::ptr::null_mut();
        esp!(unsafe { sys::adc_oneshot_new_unit(&init, &mut unit) })?;
        let cfg = sys::adc_oneshot_chan_cfg_t {
            atten: sys::adc_atten_t_ADC_ATTEN_DB_2_5,
            bitwidth: sys::adc_bitwidth_t_ADC_BITWIDTH_12,
        };
        esp!(unsafe { sys::adc_oneshot_config_channel(unit, chan, &cfg) })?;
        let mut cal: sys::adc_cali_curve_fitting_config_t = unsafe { core::mem::zeroed() };
        cal.unit_id = unit_id;
        cal.chan = chan;
        cal.atten = cfg.atten;
        cal.bitwidth = cfg.bitwidth;
        let mut cali = core::ptr::null_mut();
        if unsafe { sys::adc_cali_create_scheme_curve_fitting(&cal, &mut cali) } != sys::ESP_OK {
            log::warn!("battery: ADC calibration unavailable");
            cali = core::ptr::null_mut();
        }
        Ok(Adc { unit, chan, cali })
    }

    /// Millivolts at the divider tap (raw counts if uncalibrated).
    fn read(&self) -> Option<f32> {
        let mut raw = 0;
        if unsafe { sys::adc_oneshot_read(self.unit, self.chan, &mut raw) } != sys::ESP_OK {
            return None;
        }
        if self.cali.is_null() {
            return Some(raw as f32);
        }
        let mut mv = 0;
        if unsafe { sys::adc_cali_raw_to_voltage(self.cali, raw, &mut mv) } != sys::ESP_OK {
            return None;
        }
        Some(mv as f32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub pct: f32,
    pub level: u8,
    pub charge: Charge,
}

struct State {
    adc: Adc,
    ema: Option<f32>,
    offset: f32,
    pct: f32,
    level: usize,
    charge: Charge,
}

pub struct Battery {
    st: Mutex<State>,
}

#[derive(Debug, Clone, Copy)]
pub enum BatteryEvent {
    Changed(Reading),
}

fn find_level(v: i32) -> usize {
    (0..LEVELS)
        .find(|&i| v >= LIMITS[i] && v <= LIMITS[i + 1])
        .unwrap_or(LEVELS)
}

fn update_level(level: usize, v: i32) -> usize {
    let lb = LIMITS[level] - if level > 0 { MARGIN } else { 0 };
    let ub = LIMITS[level + 1] + MARGIN;
    if v < lb || v > ub {
        find_level(v)
    } else {
        level
    }
}

impl Battery {
    pub fn new(tca: &Tca, charge: Charge) -> Result<Battery, EspError> {
        let adc = Adc::new(pins::BATTERY)?;
        // Calibrate against the 2.5 V reference.
        tca.write(exp::CALIB_EN, true);
        sleep_ms(100);
        for _ in 0..CAL_READS {
            adc.read();
            sleep_ms(10);
        }
        let mut total = 0.0;
        for _ in 0..CAL_READS {
            total += adc.read().unwrap_or(0.0) * FACTOR;
            sleep_ms(10);
        }
        let reading = total / CAL_READS as f32;
        tca.write(exp::CALIB_EN, false);
        sleep_ms(100);
        let offset = OFFSET + (CAL_EXPECTED - reading);
        log::info!("battery: calibration read {reading:.0} mV of {CAL_EXPECTED:.0}");
        let b = Battery {
            st: Mutex::new(State {
                adc,
                ema: None,
                offset,
                pct: 0.0,
                level: 0,
                charge,
            }),
        };
        b.sample(true);
        Ok(b)
    }

    pub fn start<E: From<BatteryEvent> + Send + 'static>(self: &Arc<Self>, tx: Sender<E>) {
        let b = self.clone();
        util::spawn(c"battery", 3072, 4, Stack::Psram, move || loop {
            sleep_ms(SAMPLE_MS);
            if let Some(r) = b.sample(false) {
                let _ = tx.send(BatteryEvent::Changed(r).into());
            }
        });
    }

    /// One reading; returns it when the level changed (or `fresh`: restart the smoothing).
    /// While charging the voltage says little; the level stays where it was.
    fn sample(&self, fresh: bool) -> Option<Reading> {
        let mut st = self.st.lock().unwrap();
        if st.charge != Charge::Unplugged && !fresh {
            return None;
        }
        let mv = st.adc.read()?;
        let ema = match st.ema {
            Some(e) if !fresh => EMA * mv + (1.0 - EMA) * e,
            _ => mv,
        };
        st.ema = Some(ema);
        let volts = (ema * FACTOR + st.offset).clamp(VOLT_EMPTY, VOLT_FULL);
        st.pct = (volts - VOLT_EMPTY) / (VOLT_FULL - VOLT_EMPTY) * 100.0;
        let old = st.level.min(LEVELS - 1);
        st.level = if fresh {
            find_level(st.pct as i32)
        } else {
            update_level(st.level.min(LEVELS - 1), st.pct as i32)
        };
        let level = st.level.min(LEVELS - 1);
        (fresh || level != old).then(|| Reading {
            pct: st.pct,
            level: level as u8,
            charge: st.charge,
        })
    }

    /// The charger changed (base board event). Returns the new reading if it did.
    pub fn set_charge(&self, charge: Charge) -> Option<Reading> {
        {
            let mut st = self.st.lock().unwrap();
            if st.charge == charge {
                return None;
            }
            st.charge = charge;
        }
        self.sample(true).or_else(|| Some(self.reading()))
    }

    pub fn reading(&self) -> Reading {
        let st = self.st.lock().unwrap();
        Reading {
            pct: st.pct,
            level: st.level.min(LEVELS - 1) as u8,
            charge: st.charge,
        }
    }
}
