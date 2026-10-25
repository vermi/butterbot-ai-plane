//! The clock: local time (the robot has no time zone; it keeps local time as if it were UTC),
//! set by the plane, the phone or the BM8563 real-time clock, which keeps it while switched off.

use std::sync::{Mutex, OnceLock};

use esp_idf_svc::sys;

use crate::i2c::{Bus, Dev};
use crate::pins;

static RTC: OnceLock<Mutex<Dev>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0f)
}

fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Read the RTC at boot and set the clock from it.
pub fn init(bus: &Bus) {
    let Ok(dev) = bus.device(pins::ADDR_RTC, 400_000) else {
        return;
    };
    let mut r = [0u8; 7];
    if dev.read_reg(0x02, &mut r).is_ok() && r[0] & 0x80 == 0 {
        let year = 2000 + bcd(r[6]) as u16 + if r[5] & 0x80 != 0 { 100 } else { 0 };
        let t = Local {
            year,
            month: bcd(r[5] & 0x1f),
            day: bcd(r[3] & 0x3f),
            weekday: r[4] & 0x07,
            hour: bcd(r[2] & 0x3f),
            minute: bcd(r[1] & 0x7f),
            second: bcd(r[0] & 0x7f),
        };
        if year >= 2024 {
            set_system(t);
            log::info!(
                "clock: {:04}-{:02}-{:02} {:02}:{:02} from the RTC",
                t.year,
                t.month,
                t.day,
                t.hour,
                t.minute
            );
        }
    }
    let _ = RTC.set(Mutex::new(dev));
}

fn set_system(t: Local) {
    let secs = days(t.year as i64, t.month as i64, t.day as i64) * 86400
        + t.hour as i64 * 3600
        + t.minute as i64 * 60
        + t.second as i64;
    let tv = sys::timeval {
        tv_sec: secs as _,
        tv_usec: 0,
    };
    unsafe { sys::settimeofday(&tv, core::ptr::null()) };
}

/// Set the local time (and the RTC), unless it is already within a few seconds.
pub fn set_local(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) {
    if !(2024..2200).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return;
    }
    let want = days(year as i64, month as i64, day as i64) * 86400
        + hour as i64 * 3600
        + minute as i64 * 60
        + second as i64;
    if now().is_some() && (epoch() - want).abs() <= 5 {
        return;
    }
    set_epoch(want);
}

/// Local time as seconds since 1970 (as if it were UTC).
pub fn set_epoch(secs: i64) {
    let (y, m, d) = civil(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    let t = Local {
        year: y as u16,
        month: m as u8,
        day: d as u8,
        weekday: ((secs.div_euclid(86400) + 4).rem_euclid(7)) as u8,
        hour: (rem / 3600) as u8,
        minute: (rem % 3600 / 60) as u8,
        second: (rem % 60) as u8,
    };
    set_system(t);
    if let Some(rtc) = RTC.get() {
        let r = [
            to_bcd(t.second),
            to_bcd(t.minute),
            to_bcd(t.hour),
            to_bcd(t.day),
            t.weekday,
            to_bcd(t.month) | if t.year >= 2100 { 0x80 } else { 0 },
            to_bcd((t.year % 100) as u8),
        ];
        let _ = rtc.lock().unwrap().write_reg(0x02, &r);
    }
}

fn epoch() -> i64 {
    let mut tv = sys::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe { sys::gettimeofday(&mut tv, core::ptr::null_mut()) };
    tv.tv_sec as i64
}

/// The local time, once it has been set (from the RTC, the plane or a phone).
pub fn now() -> Option<Local> {
    let secs = epoch();
    if secs < days(2024, 1, 1) * 86400 {
        return None;
    }
    let (y, m, d) = civil(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    Some(Local {
        year: y as u16,
        month: m as u8,
        day: d as u8,
        weekday: ((secs.div_euclid(86400) + 4).rem_euclid(7)) as u8,
        hour: (rem / 3600) as u8,
        minute: (rem % 3600 / 60) as u8,
        second: (rem % 60) as u8,
    })
}
