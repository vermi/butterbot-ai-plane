//! Firmware updates over Wi-Fi, from the plane (`bbplane ota`).
//!
//! The robot restarts into update mode: right after boot, before anything else starts, it joins
//! Wi-Fi, downloads the image over HTTP into the app slot not running, checks its size and
//! SHA-256 (and ESP-IDF checks the image), makes it the boot slot, tells the plane how it went
//! (`GET <url>?result=ok`, or `?result=failed&error=...`) and restarts into AI mode on it. On any
//! failure it restarts into AI mode on the firmware it had. Update mode never runs longer than
//! six minutes, and a long press of the button switches the robot off meanwhile.
//!
//! A new firmware only stays once it has reached the plane (`confirm`); a reset before that boots
//! the previous one (the bootloader's rollback).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use esp_idf_svc::sys;
use sha2::{Digest, Sha256};

use crate::net::Net;
use crate::power;
use crate::rtc::{self, Job, JOB_MAGIC};
use crate::store::Store;
use crate::util::{self, millis, sleep_ms, Stack};

const JOIN_TIMEOUT_MS: u64 = 30_000;
const MAX_MS: u64 = 6 * 60 * 1000;

fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Keep the job and restart into update mode. Stop the motors first.
pub fn restart_into_update(url: &str, size: u32, sha256: &str) -> ! {
    rtc::update(|r| {
        r.job = Job {
            magic: JOB_MAGIC,
            size,
            url: [0; 192],
            sha256: [0; 65],
        };
        let n = url.len().min(191);
        r.job.url[..n].copy_from_slice(&url.as_bytes()[..n]);
        let n = sha256.len().min(64);
        r.job.sha256[..n].copy_from_slice(&sha256.as_bytes()[..n]);
    });
    log::warn!("restarting into update mode");
    power::restart();
}

/// The pending update, if this start is the restart into update mode. Taken once: a crash in
/// update mode boots normally.
pub fn take_pending() -> Option<(String, u32, String)> {
    let why = unsafe { sys::esp_reset_reason() };
    let job = rtc::read().job;
    rtc::update(|r| r.job.magic = 0);
    if why != sys::esp_reset_reason_t_ESP_RST_SW || job.magic != JOB_MAGIC {
        return None;
    }
    Some((text(&job.url), job.size, text(&job.sha256)))
}

/// Run update mode; ends in a restart.
pub fn run(net: &Net, store: &Store, url: &str, size: u32, sha256: &str) -> ! {
    log::warn!("update mode: {size} bytes from {url}");
    // The way out if it gets stuck, and the button (a long press switches the robot off).
    util::spawn(c"update_guard", 2560, 10, Stack::Internal, || {
        let started = millis();
        let mut pressed_for = 0;
        loop {
            sleep_ms(100);
            pressed_for = if power::button_pressed() {
                pressed_for + 100
            } else {
                0
            };
            if pressed_for >= 1500 {
                log::warn!("update mode: switched off");
                power::power_off();
            }
            if millis() - started > MAX_MS {
                log::error!("update mode took too long, giving up");
                power::restart();
            }
        }
    });

    let result = join(net, store).and_then(|()| download(url, size, sha256));
    match &result {
        Ok(()) => log::warn!("update done"),
        Err(e) => log::error!("update failed: {e}"),
    }
    if net.has_ip() {
        report(url, result.as_ref().err().map(|s| s.as_str()));
    }
    net.down();
    sleep_ms(200);
    power::restart();
}

fn join(net: &Net, store: &Store) -> Result<(), String> {
    let cfg = store.link_config().ok_or("no Wi-Fi settings")?;
    net.up(&cfg, store.channel())
        .map_err(|_| "Wi-Fi would not start")?;
    let deadline = millis() + JOIN_TIMEOUT_MS;
    while !net.has_ip() {
        if millis() > deadline {
            return Err("could not join Wi-Fi".into());
        }
        sleep_ms(50);
    }
    Ok(())
}

/// `http://host:port/path` -> (host:port, path)
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..slash];
    let path = if slash < rest.len() {
        &rest[slash..]
    } else {
        "/"
    };
    Some((host, path))
}

fn get(url: &str) -> Result<(TcpStream, Option<u64>), String> {
    let (host, path) = split_url(url).ok_or("bad update url")?;
    let addr = host
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or("could not reach the plane's update server")?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|_| "could not reach the plane's update server")?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = s.set_nodelay(true);
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_| "could not reach the plane's update server")?;
    // Headers, byte by byte (they are short).
    let mut head = Vec::with_capacity(256);
    let mut b = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 4096 || s.read(&mut b).map_err(|_| "the download broke off")? == 0 {
            return Err("the server's answer broke off".into());
        }
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.split(' ').nth(1).unwrap_or("");
    if status != "200" {
        return Err("the server refused the download".into());
    }
    let len = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())?
    });
    Ok((s, len))
}

fn download(url: &str, size: u32, sha256: &str) -> Result<(), String> {
    let slot = unsafe { sys::esp_ota_get_next_update_partition(core::ptr::null()) };
    if slot.is_null() {
        return Err("no update slot".into());
    }
    let slot_size = unsafe { (*slot).size };
    if size == 0 || size > slot_size {
        return Err("image does not fit the update slot".into());
    }
    // Erase first (it takes a while), so the download then runs at full speed.
    let t = millis();
    let mut handle: sys::esp_ota_handle_t = 0;
    if unsafe { sys::esp_ota_begin(slot, size as usize, &mut handle) } != sys::ESP_OK {
        return Err("could not erase the update slot".into());
    }
    log::warn!("erased {size} bytes in {} ms", millis() - t);
    let t = millis();
    let result = (|| {
        let (mut s, len) = get(url)?;
        if len.is_some_and(|l| l != size as u64) {
            return Err("the server sent a different size".to_string());
        }
        // Flash is written with the cache off: this buffer is on an internal-RAM stack.
        let mut buf = [0u8; 4096];
        let mut sha = Sha256::new();
        let mut done = 0u32;
        let mut logged = 0u32;
        while done < size {
            let want = (size - done).min(buf.len() as u32) as usize;
            let n = s
                .read(&mut buf[..want])
                .map_err(|_| "the download broke off")?;
            if n == 0 {
                return Err("the download broke off".into());
            }
            sha.update(&buf[..n]);
            if unsafe { sys::esp_ota_write(handle, buf.as_ptr() as *const _, n) } != sys::ESP_OK {
                return Err("writing flash failed".into());
            }
            done += n as u32;
            if done - logged >= size / 10 {
                logged = done;
                log::warn!(
                    "{}% after {} ms",
                    done as u64 * 100 / size as u64,
                    millis() - t
                );
            }
        }
        let digest: String = sha.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !digest.eq_ignore_ascii_case(sha256) {
            return Err("the image's checksum does not match".into());
        }
        Ok(())
    })();
    if let Err(e) = result {
        unsafe { sys::esp_ota_abort(handle) };
        return Err(e);
    }
    if unsafe { sys::esp_ota_end(handle) } != sys::ESP_OK {
        return Err("the image is not valid firmware".into());
    }
    if unsafe { sys::esp_ota_set_boot_partition(slot) } != sys::ESP_OK {
        return Err("could not select the new firmware".into());
    }
    log::warn!("new firmware written in {} ms", millis() - t);
    Ok(())
}

fn report(url: &str, fail: Option<&str>) {
    let mut q = String::from(match fail {
        None => "?result=ok",
        Some(_) => "?result=failed&error=",
    });
    for c in fail.unwrap_or("").bytes() {
        if c.is_ascii_alphanumeric() || b"-._".contains(&c) {
            q.push(c as char);
        } else {
            q.push_str(&format!("%{c:02X}"));
        }
    }
    let _ = get(&format!("{url}{q}"));
}

/// A new firmware that has not reached the plane yet.
pub fn on_probation() -> bool {
    unsafe {
        let running = sys::esp_ota_get_running_partition();
        let mut state: sys::esp_ota_img_states_t = 0;
        !running.is_null()
            && sys::esp_ota_get_state_partition(running, &mut state) == sys::ESP_OK
            && state == sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
    }
}

/// A new firmware has this long to reach the plane; then it restarts, and the bootloader goes
/// back to the previous one (also when this one hangs without crashing).
const PROBATION_MS: u64 = 3 * 60 * 1000;

pub fn start_probation() {
    if !on_probation() {
        return;
    }
    log::warn!(
        "new firmware on probation: it must reach the plane within {} s",
        PROBATION_MS / 1000
    );
    util::spawn(c"probation", 2048, 20, Stack::Internal, || {
        sleep_ms(PROBATION_MS);
        if on_probation() {
            log::error!("new firmware never reached the plane: back to the previous one");
            power::restart();
        }
    });
}

/// Keep the running firmware if it was on probation (it reached the plane).
pub fn confirm() {
    if on_probation() {
        unsafe { sys::esp_ota_mark_app_valid_cancel_rollback() };
        log::warn!("new firmware reached the plane: keeping it");
    }
}
