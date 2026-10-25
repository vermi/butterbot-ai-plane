//! Logging: to the serial console, and to the plane too, which writes them to its log
//! (`[level] target: message`): the firmware's own messages (info and up), and the warnings and
//! errors of ESP-IDF and its drivers (`esp_log` lines, and what they print with `ets_printf`,
//! from interrupts too).

use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};

use bbproto::FromRobot;
use esp_idf_svc::log::EspIdfLogger;
use log::{Level, LevelFilter, Metadata, Record};

use crate::link::Link;
use crate::util::{self, sleep_ms, Stack};
use esp_idf_svc::sys;

struct Logger {
    console: EspIdfLogger<()>,
}

static LOGGER: Logger = Logger { console: EspIdfLogger::new(()) };
static LINK: OnceLock<Arc<Link>> = OnceLock::new();

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(LevelFilter::Info);
}

pub fn forward_to(link: Arc<Link>) {
    let _ = LINK.set(link.clone());
    let (tx, rx) = sync_channel::<String>(32);
    let _ = C_LINES.set(Mutex::new(tx));
    unsafe {
        sys::esp_log_set_vprintf(Some(vprintf_hook));
        // Channel 1 stays the UART; channel 2 sees the same characters.
        sys::esp_rom_install_channel_putc(2, Some(rom_putc));
    }
    util::spawn(c"log_fwd", 4096, 3, Stack::Psram, move || forward(&link, rx));
}

// ---------------------------------------------------------------- ESP-IDF's own output

static C_LINES: OnceLock<Mutex<SyncSender<String>>> = OnceLock::new();

const RING: usize = 2048;
static RING_BUF: [AtomicU8; RING] = [const { AtomicU8::new(0) }; RING];
static RING_WR: AtomicUsize = AtomicUsize::new(0);

/// `ets_printf` output, a character at a time (also from interrupts: no locks here).
unsafe extern "C" fn rom_putc(c: c_char) {
    let i = RING_WR.fetch_add(1, Ordering::Relaxed);
    RING_BUF[i % RING].store(c as u8, Ordering::Relaxed);
}

unsafe extern "C" {
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ap: sys::va_list) -> c_int;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
}

/// `esp_log` output (task context): to the console as before, warnings and errors also queued.
unsafe extern "C" fn vprintf_hook(fmt: *const c_char, ap: sys::va_list) -> c_int {
    let mut buf = [0u8; 256];
    let n = vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, ap);
    let len = (n.max(0) as usize).min(buf.len() - 1);
    write(1, buf.as_ptr() as *const c_void, len);
    let line = &buf[..len];
    if line.starts_with(b"\x1b[0;31mE") || line.starts_with(b"\x1b[0;33mW") || line.starts_with(b"E (") || line.starts_with(b"W (") {
        if let Some(tx) = C_LINES.get() {
            if let Ok(tx) = tx.try_lock() {
                let _ = tx.try_send(String::from_utf8_lossy(line).into_owned());
            }
        }
    }
    n
}

fn clean(line: &str) -> String {
    // Without the colour codes and the "E (1234) " prefix.
    let mut s = line.replace("\x1b[0m", "");
    if let Some(i) = s.find('m') {
        if s.starts_with("\x1b[") {
            s = s[i + 1..].to_string();
        }
    }
    let s = match s.find(") ") {
        Some(i) if s.starts_with("E (") || s.starts_with("W (") => s[i + 2..].to_string(),
        _ => s,
    };
    s.trim_end().to_string()
}

fn forward(link: &Link, rx: Receiver<String>) {
    let mut rd = 0usize;
    let mut partial = Vec::new();
    loop {
        sleep_ms(100);
        let mut lines: Vec<(&'static str, String)> = Vec::new();
        while let Ok(l) = rx.try_recv() {
            let level = if l.contains("E (") || l.starts_with("\x1b[0;31m") { "error" } else { "warn" };
            lines.push((level, clean(&l)));
        }
        let wr = RING_WR.load(Ordering::Relaxed);
        if wr - rd > RING {
            rd = wr - RING;
        }
        while rd < wr {
            let c = RING_BUF[rd % RING].load(Ordering::Relaxed);
            rd += 1;
            if c == b'\n' {
                let l = String::from_utf8_lossy(&partial).trim_end().to_string();
                partial.clear();
                if !l.is_empty() {
                    lines.push(("warn", l));
                }
            } else if partial.len() < 200 {
                partial.push(c);
            }
        }
        if !link.is_online() {
            continue;
        }
        for (level, msg) in lines.into_iter().take(20) {
            link.send(&FromRobot::Log { level: level.into(), msg: format!("idf: {msg}") });
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= Level::Info
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        self.console.log(r);
        let target = r.target();
        // The link's own messages would loop through it.
        let Some(module) = target.strip_prefix("butterbot_ai::") else {
            return;
        };
        if module.starts_with("link") || module.starts_with("logger") {
            return;
        }
        if let Some(link) = LINK.get() {
            if link.is_online() {
                let level = match r.level() {
                    Level::Error => "error",
                    Level::Warn => "warn",
                    _ => "info",
                };
                link.send(&FromRobot::Log {
                    level: level.into(),
                    msg: format!("{module}: {}", r.args()),
                });
            }
        }
    }

    fn flush(&self) {}
}
