//! The home Wi-Fi (station). Joins with the channel last used as a hint (a fast scan), or scans
//! all channels and takes the strongest access point. Reconnects on its own while wanted.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::sys::{self, esp, EspError};
use esp_idf_svc::wifi::{
    AuthMethod, ClientConfiguration, Configuration, EspWifi, ScanMethod, ScanSortMethod,
};

use crate::store::LinkConfig;

pub struct Net {
    wifi: Mutex<EspWifi<'static>>,
    wanted: AtomicBool,
    got_ip: AtomicBool,
    ip: AtomicU32,
    /// The channel to remember after a join (0xff: nothing new).
    joined_channel: AtomicU8,
}

static NET: OnceLock<Arc<Net>> = OnceLock::new();

/// Only remember a channel joined with a good signal: a fast scan takes the first access point
/// found on it, so a weak one would stick.
const GOOD_RSSI: i8 = -70;

impl Net {
    pub fn new(modem: Modem<'static>, sysloop: EspSystemEventLoop) -> Result<Arc<Net>, EspError> {
        let wifi = EspWifi::new(modem, sysloop, None)?;
        let net = Arc::new(Net {
            wifi: Mutex::new(wifi),
            wanted: AtomicBool::new(false),
            got_ip: AtomicBool::new(false),
            ip: AtomicU32::new(0),
            joined_channel: AtomicU8::new(0xff),
        });
        let _ = NET.set(net.clone());
        unsafe {
            esp!(sys::esp_event_handler_register(
                sys::WIFI_EVENT,
                sys::ESP_EVENT_ANY_ID,
                Some(on_event),
                core::ptr::null_mut()
            ))?;
            esp!(sys::esp_event_handler_register(
                sys::IP_EVENT,
                sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32,
                Some(on_event),
                core::ptr::null_mut()
            ))?;
        }
        Ok(net)
    }

    pub fn up(&self, cfg: &LinkConfig, channel: u8) -> Result<(), EspError> {
        let mut wifi = self.wifi.lock().unwrap();
        let client = ClientConfiguration {
            ssid: cfg.ssid.as_str().try_into().unwrap_or_default(),
            password: cfg.password.as_str().try_into().unwrap_or_default(),
            auth_method: if cfg.password.is_empty() {
                AuthMethod::None
            } else {
                AuthMethod::WPA2Personal
            },
            channel: (channel != 0).then_some(channel),
            scan_method: if channel != 0 {
                ScanMethod::FastScan
            } else {
                ScanMethod::CompleteScan(ScanSortMethod::Signal)
            },
            ..Default::default()
        };
        self.got_ip.store(false, Ordering::Relaxed);
        self.wanted.store(true, Ordering::Relaxed);
        wifi.set_configuration(&Configuration::Client(client))?;
        if !wifi.is_started()? {
            wifi.start()?;
        }
        // Lowest latency: the radio never dozes between beacons.
        unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) };
        wifi.connect()?;
        log::info!(
            "wifi: joining \"{}\"{}",
            cfg.ssid,
            if channel != 0 {
                format!(" on channel {channel}")
            } else {
                String::new()
            }
        );
        Ok(())
    }

    pub fn down(&self) {
        self.wanted.store(false, Ordering::Relaxed);
        self.got_ip.store(false, Ordering::Relaxed);
        let mut wifi = self.wifi.lock().unwrap();
        if wifi.is_started().unwrap_or(false) {
            let _ = wifi.disconnect();
            let _ = wifi.stop();
            log::info!("wifi: off");
        }
    }

    pub fn is_up(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    pub fn has_ip(&self) -> bool {
        self.got_ip.load(Ordering::Relaxed)
    }

    /// The channel of a join since the last call, to store.
    pub fn take_joined_channel(&self) -> Option<u8> {
        match self.joined_channel.swap(0xff, Ordering::Relaxed) {
            0xff => None,
            c => Some(c),
        }
    }

    pub fn rssi() -> Option<i32> {
        let mut ap: sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
        (unsafe { sys::esp_wifi_sta_get_ap_info(&mut ap) } == sys::ESP_OK).then_some(ap.rssi as i32)
    }
}

unsafe extern "C" fn on_event(
    _arg: *mut core::ffi::c_void,
    base: sys::esp_event_base_t,
    id: i32,
    data: *mut core::ffi::c_void,
) {
    let Some(net) = NET.get() else {
        return;
    };
    if base == sys::WIFI_EVENT && id == sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32 {
        let reason = (*(data as *const sys::wifi_event_sta_disconnected_t)).reason;
        net.got_ip.store(false, Ordering::Relaxed);
        if net.wanted.load(Ordering::Relaxed) {
            log::info!("wifi: disconnected (reason {reason}), reconnecting");
            sys::esp_wifi_connect();
        }
    } else if base == sys::IP_EVENT && id == sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32 {
        let ev = &*(data as *const sys::ip_event_got_ip_t);
        let ip = ev.ip_info.ip.addr;
        net.ip.store(ip, Ordering::Relaxed);
        net.got_ip.store(true, Ordering::Relaxed);
        let mut ap: sys::wifi_ap_record_t = core::mem::zeroed();
        if sys::esp_wifi_sta_get_ap_info(&mut ap) == sys::ESP_OK {
            log::info!(
                "wifi: got {}.{}.{}.{} on channel {} ({} dBm)",
                ip & 0xff,
                (ip >> 8) & 0xff,
                (ip >> 16) & 0xff,
                ip >> 24,
                ap.primary,
                ap.rssi
            );
            let ch = if ap.rssi >= GOOD_RSSI { ap.primary } else { 0 };
            net.joined_channel.store(ch, Ordering::Relaxed);
        }
    }
}
