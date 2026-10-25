//! Small helpers: time, memory, threads.

use std::ffi::CStr;
use std::thread::JoinHandle;
use std::time::Duration;

use esp_idf_svc::sys;

/// Milliseconds since boot.
pub fn millis() -> u64 {
    (unsafe { sys::esp_timer_get_time() } / 1000) as u64
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Free internal (DMA-capable) RAM and its largest free block: what Wi-Fi, Bluetooth and DMA
/// drivers need, and what runs out first.
pub fn internal_ram() -> (u32, u32) {
    let caps = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_DMA;
    unsafe {
        (
            sys::heap_caps_get_free_size(caps) as u32,
            sys::heap_caps_get_largest_free_block(caps) as u32,
        )
    }
}

pub fn psram_free() -> u32 {
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM) as u32 }
}

/// Where a thread's stack lives. PSRAM stacks must never run code that writes flash (NVS, OTA).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    Internal,
    Psram,
}

/// Spawn a named thread with its own stack size, priority and stack memory.
pub fn spawn<F>(name: &'static CStr, stack: usize, prio: u8, mem: Stack, f: F) -> JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    unsafe {
        let mut cfg = sys::esp_pthread_get_default_config();
        cfg.stack_size = stack;
        cfg.prio = prio as _;
        cfg.inherit_cfg = false;
        cfg.thread_name = name.as_ptr();
        cfg.stack_alloc_caps = match mem {
            Stack::Internal => sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
            Stack::Psram => sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT,
        };
        sys::esp_pthread_set_cfg(&cfg);
    }
    let handle = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(f)
        .expect("thread spawn");
    unsafe {
        let cfg = sys::esp_pthread_get_default_config();
        sys::esp_pthread_set_cfg(&cfg);
    }
    handle
}

/// The running firmware's build id: the start of its ELF's SHA-256, as in the image header
/// (`bbplane::ota` reads it from there).
pub fn build_id() -> String {
    let desc = unsafe { &*sys::esp_app_get_description() };
    desc.app_elf_sha256[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn mac() -> String {
    let mut mac = [0u8; 6];
    unsafe {
        sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_STA);
    }
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
