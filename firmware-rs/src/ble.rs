//! Bluetooth LE (NimBLE): the robot is a peripheral, as the stock robot was. It advertises as
//! "CircuitMess Butter Bot" and serves
//! - the Com service: the handheld remote, and the plane's provisioning (`bbplane provision`);
//! - the Nordic UART service: the CircuitMess app on an Android phone.
//!
//! An iPhone connects from its Bluetooth settings; the robot then reads its notifications, music
//! and clock as a GATT client over the same connection (see `phone`), with the helpers in `gattc`.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use esp32_nimble::enums::{AuthReq, SecurityIOCap};
use esp32_nimble::utilities::mutex::Mutex as NimbleMutex;
use esp32_nimble::utilities::BleUuid;
use esp32_nimble::{BLECharacteristic, BLEDevice, NimbleProperties, NimbleSub};

pub const NAME: &str = "CircuitMess Butter Bot";

/// The stock robot's UUIDs, least significant byte first (as in its source).
const COM_SERVICE: [u8; 16] = [
    0x19, 0x08, 0x79, 0xbc, 0x80, 0xbd, 0x40, 0x44, 0x91, 0x98, 0x51, 0x82, 0xe7, 0xca, 0x85, 0x36,
];
const NUS_SERVICE: [u8; 16] = nus(0x01);
const NUS_RX: [u8; 16] = nus(0x02);
const NUS_TX: [u8; 16] = nus(0x03);

const fn nus(n: u8) -> [u8; 16] {
    [
        0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, n, 0x00, 0x40, 0x6E,
    ]
}

/// One notification at most: the MTU both ends ask for is 500.
const MAX_PACKET: usize = 496;

#[derive(Debug)]
pub enum BleEvent {
    Connected(u16),
    Disconnected(u16),
    /// The link is encrypted (pairing or a bonded reconnect): time to look for ANCS.
    Encrypted(u16),
    /// The remote (or the provisioning tool) subscribed to Com, or the last one left.
    ComSubscribed(bool),
    /// A write to Com: `Ctrl::Command` (u32) and its payload.
    Com(Vec<u8>),
    /// The Android app subscribed to the UART (connected) or left.
    UartSubscribed(u16, bool),
    /// Bytes from the Android app.
    Uart(Vec<u8>),
}

pub struct Ble {
    com_tx: Arc<NimbleMutex<BLECharacteristic>>,
    nus_tx: Arc<NimbleMutex<BLECharacteristic>>,
    com_subs: Arc<Mutex<HashSet<u16>>>,
}

/// The stock advertisement as it went on air: flags, the HID service as a 16-bit UUID (iPhones
/// list the robot in their Bluetooth settings for it; the stock code gave it in 128-bit form,
/// which Bluedroid shortens), appearance "generic computer", preferred connection interval; the
/// name in the scan response.
fn advertisement() -> Vec<u8> {
    let mut a = vec![0x02, 0x01, 0x06];
    a.extend_from_slice(&[0x03, 0x03, 0x12, 0x18]);
    a.extend_from_slice(&[0x03, 0x19, 0x80, 0x00]);
    a.extend_from_slice(&[0x05, 0x12, 0x06, 0x00, 0x10, 0x00]);
    a
}

fn scan_response() -> Vec<u8> {
    let mut r = vec![NAME.len() as u8 + 1, 0x09];
    r.extend_from_slice(NAME.as_bytes());
    r
}

impl Ble {
    pub fn start<E: From<BleEvent> + Send + 'static>(events: Sender<E>) -> Arc<Ble> {
        let device = BLEDevice::take();
        let _ = BLEDevice::set_device_name(NAME);
        // "Generic computer", as in the advertisement.
        unsafe { esp_idf_svc::sys::ble_svc_gap_device_appearance_set(0x0080) };
        let _ = device.set_preferred_mtu(500);
        // Just Works with bonding, like the stock robot (it has no screen or keys).
        device
            .security()
            .set_auth(AuthReq::Bond | AuthReq::Sc)
            .set_io_cap(SecurityIOCap::NoInputNoOutput);

        let server = device.get_server();
        let ev = events.clone();
        server.on_connect(move |server, desc| {
            let _ = ev.send(BleEvent::Connected(desc.conn_handle()).into());
            // More connections may come (the remote, a phone, the plane).
            if server.connected_count()
                < esp_idf_svc::sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as usize
            {
                if let Err(e) = BLEDevice::take().get_advertising().lock().start() {
                    log::warn!("advertising after a connection: {e:?}");
                }
            }
        });
        let com_subs: Arc<Mutex<HashSet<u16>>> = Arc::default();
        let ev = events.clone();
        let subs = com_subs.clone();
        server.on_disconnect(move |desc, _reason| {
            let h = desc.conn_handle();
            let was = {
                let mut s = subs.lock().unwrap();
                s.remove(&h) && s.is_empty()
            };
            if was {
                let _ = ev.send(BleEvent::ComSubscribed(false).into());
            }
            let _ = ev.send(BleEvent::Disconnected(h).into());
        });
        let ev = events.clone();
        server.on_authentication_complete(move |_, desc, result| {
            if result.is_ok() {
                let _ = ev.send(BleEvent::Encrypted(desc.conn_handle()).into());
            } else {
                log::info!("pairing with {} failed", desc.address());
            }
        });

        // Com: the remote writes commands to RX and listens on TX.
        let com = server.create_service(BleUuid::Uuid128(COM_SERVICE));
        let com_tx = com
            .lock()
            .create_characteristic(BleUuid::Uuid128(NUS_TX), NimbleProperties::NOTIFY);
        let com_rx = com.lock().create_characteristic(
            BleUuid::Uuid128(NUS_RX),
            NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
        );
        let ev = events.clone();
        let subs = com_subs.clone();
        com_tx.lock().on_subscribe(move |_, desc, sub| {
            let h = desc.conn_handle();
            let on = sub.contains(NimbleSub::NOTIFY);
            let mut s = subs.lock().unwrap();
            let before = s.len();
            if on {
                s.insert(h);
            } else {
                s.remove(&h);
            }
            // Every new subscriber gets the remote's welcome (status, battery, home screen).
            if on || (before > 0 && s.is_empty()) {
                let _ = ev.send(BleEvent::ComSubscribed(on).into());
            }
        });
        let ev = events.clone();
        com_rx.lock().on_write(move |args| {
            let _ = ev.send(BleEvent::Com(args.recv_data().to_vec()).into());
        });

        // The CircuitMess app on Android: a text protocol over the Nordic UART service.
        let nus = server.create_service(BleUuid::Uuid128(NUS_SERVICE));
        let nus_tx = nus
            .lock()
            .create_characteristic(BleUuid::Uuid128(NUS_TX), NimbleProperties::NOTIFY);
        let nus_rx = nus.lock().create_characteristic(
            BleUuid::Uuid128(NUS_RX),
            NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
        );
        let ev = events.clone();
        nus_tx.lock().on_subscribe(move |_, desc, sub| {
            let _ = ev.send(
                BleEvent::UartSubscribed(desc.conn_handle(), sub.contains(NimbleSub::NOTIFY))
                    .into(),
            );
        });
        let ev = events;
        nus_rx.lock().on_write(move |args| {
            let _ = ev.send(BleEvent::Uart(args.recv_data().to_vec()).into());
        });

        let adv = device.get_advertising();
        {
            let mut a = adv.lock();
            if let Err(e) = a.set_raw_data(&advertisement()) {
                log::error!("advertisement: {e:?}");
            }
            if let Err(e) = a.set_raw_scan_response_data(&scan_response()) {
                log::error!("scan response: {e:?}");
            }
            if let Err(e) = a.start() {
                log::error!("advertising: {e:?}");
            }
        }
        gattc::listen();
        log::info!("advertising as \"{NAME}\"");
        Arc::new(Ble {
            com_tx,
            nus_tx,
            com_subs,
        })
    }

    /// Bluetooth connections (remote, phone, plane).
    pub fn connections(&self) -> usize {
        BLEDevice::take().get_server().connected_count()
    }

    pub fn advertising(&self) -> bool {
        BLEDevice::take().get_advertising().lock().is_advertising()
    }

    /// Advertise while a connection is free: a phone finds the robot only then, and the
    /// remote is usually connected.
    pub fn keep_advertising(&self) {
        let device = BLEDevice::take();
        if device.get_server().connected_count()
            >= esp_idf_svc::sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as usize
        {
            return;
        }
        let mut adv = device.get_advertising().lock();
        if !adv.is_advertising() {
            match adv.start() {
                Ok(()) => log::warn!("advertising had stopped; started it again"),
                Err(e) => log::warn!("advertising: {e:?}"),
            }
        }
    }

    /// The remote (or another Com client) is listening.
    pub fn remote_connected(&self) -> bool {
        !self.com_subs.lock().unwrap().is_empty()
    }

    /// A `BB` message to the remote: state, action, size (u32 each), then the data.
    pub fn com_send(&self, state: u32, action: u32, data: &[u8]) {
        if !self.remote_connected() {
            return;
        }
        if 12 + data.len() > MAX_PACKET {
            log::warn!("message for the remote too large ({} bytes)", data.len());
            return;
        }
        let mut p = Vec::with_capacity(12 + data.len());
        p.extend_from_slice(&state.to_le_bytes());
        p.extend_from_slice(&action.to_le_bytes());
        p.extend_from_slice(&(data.len() as u32).to_le_bytes());
        p.extend_from_slice(data);
        self.com_tx.lock().set_value(&p).notify();
    }

    /// Text to the Android app (in notifications of at most 180 bytes).
    pub fn uart_send(&self, text: &str) {
        for chunk in text.as_bytes().chunks(180) {
            self.nus_tx.lock().set_value(chunk).notify();
        }
    }
}

/// A GATT client on connections the peer opened (the iPhone's): blocking calls for a worker
/// thread, notifications through a callback.
pub mod gattc {
    use std::ffi::c_void;
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use esp32_nimble::utilities::BleUuid;
    use esp_idf_svc::sys;

    const TIMEOUT: Duration = Duration::from_secs(10);

    pub struct Chr {
        pub uuid: BleUuid,
        pub def_handle: u16,
        pub val_handle: u16,
    }

    type NotifyFn = Box<dyn Fn(u16, u16, &[u8]) + Send>;
    static NOTIFY: OnceLock<Mutex<Option<NotifyFn>>> = OnceLock::new();
    static mut LISTENER: core::mem::MaybeUninit<sys::ble_gap_event_listener> =
        core::mem::MaybeUninit::zeroed();

    /// Notifications and indications from peers: (connection, attribute handle, value).
    pub fn on_notify(f: impl Fn(u16, u16, &[u8]) + Send + 'static) {
        *NOTIFY.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Box::new(f));
    }

    pub(super) fn listen() {
        unsafe {
            sys::ble_gap_event_listener_register(
                core::ptr::addr_of_mut!(LISTENER) as *mut _,
                Some(gap_event),
                core::ptr::null_mut(),
            );
        }
    }

    fn flat(om: *mut sys::os_mbuf) -> Vec<u8> {
        let mut buf = vec![0u8; 512];
        let mut len = 0u16;
        unsafe {
            sys::ble_hs_mbuf_to_flat(
                om,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u16,
                &mut len,
            )
        };
        buf.truncate(len as usize);
        buf
    }

    unsafe extern "C" fn gap_event(event: *mut sys::ble_gap_event, _arg: *mut c_void) -> i32 {
        let e = &*event;
        if e.type_ as u32 == sys::BLE_GAP_EVENT_NOTIFY_RX {
            let n = &e.__bindgen_anon_1.notify_rx;
            let data = flat(n.om);
            if let Some(f) = NOTIFY.get().and_then(|m| m.lock().ok()) {
                if let Some(f) = f.as_ref() {
                    f(n.conn_handle, n.attr_handle, &data);
                }
            }
        }
        0
    }

    /// Whether the link is encrypted; None once it is gone.
    pub fn encrypted(conn: u16) -> Option<bool> {
        let mut d: sys::ble_gap_conn_desc = unsafe { core::mem::zeroed() };
        if unsafe { sys::ble_gap_conn_find(conn, &mut d) } != 0 {
            return None;
        }
        Some(d.sec_state.encrypted() != 0)
    }

    /// Ask the peer to pair, or to encrypt with the keys it already has.
    pub fn secure(conn: u16) -> i32 {
        unsafe { sys::ble_gap_security_initiate(conn) }
    }

    fn uuid_ptr(u: &sys::ble_uuid_any_t) -> *const sys::ble_uuid_t {
        u as *const sys::ble_uuid_any_t as *const sys::ble_uuid_t
    }

    /// The handle range of the peer's service `uuid`.
    pub fn find_service(conn: u16, uuid: BleUuid) -> Option<(u16, u16)> {
        let (tx, rx) = sync_channel::<Option<(u16, u16)>>(4);
        let any: sys::ble_uuid_any_t = uuid.into();
        unsafe extern "C" fn cb(
            _conn: u16,
            err: *const sys::ble_gatt_error,
            svc: *const sys::ble_gatt_svc,
            arg: *mut c_void,
        ) -> i32 {
            let tx = &*(arg as *const SyncSender<Option<(u16, u16)>>);
            if (*err).status == 0 && !svc.is_null() {
                let _ = tx.try_send(Some(((*svc).start_handle, (*svc).end_handle)));
            } else {
                let _ = tx.try_send(None);
                drop(Box::from_raw(arg as *mut SyncSender<Option<(u16, u16)>>));
            }
            0
        }
        let arg = Box::into_raw(Box::new(tx)) as *mut c_void;
        let rc = unsafe { sys::ble_gattc_disc_svc_by_uuid(conn, uuid_ptr(&any), Some(cb), arg) };
        if rc != 0 {
            unsafe { drop(Box::from_raw(arg as *mut SyncSender<Option<(u16, u16)>>)) };
            return None;
        }
        let mut found = None;
        while let Ok(Some(r)) = rx.recv_timeout(TIMEOUT) {
            found.get_or_insert(r);
        }
        found
    }

    /// The characteristics in a service's handle range.
    pub fn characteristics(conn: u16, range: (u16, u16)) -> Vec<Chr> {
        type Tx = SyncSender<Option<Chr>>;
        let (tx, rx) = sync_channel::<Option<Chr>>(32);
        unsafe extern "C" fn cb(
            _conn: u16,
            err: *const sys::ble_gatt_error,
            chr: *const sys::ble_gatt_chr,
            arg: *mut c_void,
        ) -> i32 {
            let tx = &*(arg as *const Tx);
            if (*err).status == 0 && !chr.is_null() {
                let c = &*chr;
                let _ = tx.send(Some(Chr {
                    uuid: BleUuid::from(c.uuid),
                    def_handle: c.def_handle,
                    val_handle: c.val_handle,
                }));
            } else {
                let _ = tx.send(None);
                drop(Box::from_raw(arg as *mut Tx));
            }
            0
        }
        let arg = Box::into_raw(Box::new(tx)) as *mut c_void;
        let rc = unsafe { sys::ble_gattc_disc_all_chrs(conn, range.0, range.1, Some(cb), arg) };
        if rc != 0 {
            unsafe { drop(Box::from_raw(arg as *mut Tx)) };
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Ok(Some(c)) = rx.recv_timeout(TIMEOUT) {
            out.push(c);
        }
        out
    }

    /// The client configuration descriptor of the characteristic whose value is at `val`
    /// (descriptors sit between it and `end`).
    pub fn cccd(conn: u16, val: u16, end: u16) -> Option<u16> {
        type Tx = SyncSender<Option<(u16, BleUuid)>>;
        let (tx, rx) = sync_channel::<Option<(u16, BleUuid)>>(16);
        unsafe extern "C" fn cb(
            _conn: u16,
            err: *const sys::ble_gatt_error,
            _chr_val: u16,
            dsc: *const sys::ble_gatt_dsc,
            arg: *mut c_void,
        ) -> i32 {
            let tx = &*(arg as *const Tx);
            if (*err).status == 0 && !dsc.is_null() {
                let _ = tx.send(Some(((*dsc).handle, BleUuid::from((*dsc).uuid))));
            } else {
                let _ = tx.send(None);
                drop(Box::from_raw(arg as *mut Tx));
            }
            0
        }
        let arg = Box::into_raw(Box::new(tx)) as *mut c_void;
        let rc = unsafe { sys::ble_gattc_disc_all_dscs(conn, val, end, Some(cb), arg) };
        if rc != 0 {
            unsafe { drop(Box::from_raw(arg as *mut Tx)) };
            return None;
        }
        let mut found = None;
        while let Ok(Some((h, u))) = rx.recv_timeout(TIMEOUT) {
            if u == BleUuid::Uuid16(0x2902) {
                found.get_or_insert(h);
            }
        }
        found
    }

    fn done_cb_rx() -> (SyncSender<i32>, std::sync::mpsc::Receiver<i32>) {
        sync_channel::<i32>(1)
    }

    /// Write with response.
    pub fn write(conn: u16, handle: u16, data: &[u8]) -> Result<(), i32> {
        type Tx = SyncSender<i32>;
        let (tx, rx) = done_cb_rx();
        unsafe extern "C" fn cb(
            _conn: u16,
            err: *const sys::ble_gatt_error,
            _attr: *mut sys::ble_gatt_attr,
            arg: *mut c_void,
        ) -> i32 {
            let tx = Box::from_raw(arg as *mut Tx);
            let _ = tx.send((*err).status as i32);
            0
        }
        let arg = Box::into_raw(Box::new(tx)) as *mut c_void;
        let rc = unsafe {
            sys::ble_gattc_write_flat(
                conn,
                handle,
                data.as_ptr() as *const c_void,
                data.len() as u16,
                Some(cb),
                arg,
            )
        };
        if rc != 0 {
            unsafe { drop(Box::from_raw(arg as *mut Tx)) };
            return Err(rc);
        }
        match rx.recv_timeout(TIMEOUT) {
            Ok(0) => Ok(()),
            Ok(e) => Err(e),
            Err(_) => Err(-1),
        }
    }

    pub fn read(conn: u16, handle: u16) -> Result<Vec<u8>, i32> {
        type Tx = SyncSender<Result<Vec<u8>, i32>>;
        let (tx, rx) = sync_channel::<Result<Vec<u8>, i32>>(1);
        unsafe extern "C" fn cb(
            _conn: u16,
            err: *const sys::ble_gatt_error,
            attr: *mut sys::ble_gatt_attr,
            arg: *mut c_void,
        ) -> i32 {
            let tx = Box::from_raw(arg as *mut Tx);
            let status = (*err).status as i32;
            let _ = tx.send(if status == 0 && !attr.is_null() {
                Ok(super::gattc::flat((*attr).om))
            } else {
                Err(status)
            });
            0
        }
        let arg = Box::into_raw(Box::new(tx)) as *mut c_void;
        let rc = unsafe { sys::ble_gattc_read(conn, handle, Some(cb), arg) };
        if rc != 0 {
            unsafe { drop(Box::from_raw(arg as *mut Tx)) };
            return Err(rc);
        }
        rx.recv_timeout(TIMEOUT).unwrap_or(Err(-1))
    }

    /// Turn on notifications (1) or indications (2) for a characteristic.
    pub fn subscribe(conn: u16, cccd: u16, mode: u16) -> Result<(), i32> {
        write(conn, cccd, &mode.to_le_bytes())
    }
}
