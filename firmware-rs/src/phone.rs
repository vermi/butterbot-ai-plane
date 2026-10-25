//! The phone link: a paired phone's notifications, calls, music and clock.
//!
//! - iPhone: once it has connected (from its Bluetooth settings) and the link is encrypted, the
//!   robot is a GATT client on that connection for Apple's Notification Center (ANCS), Media
//!   (AMS) and Current Time services.
//! - Android: the CircuitMess app talks a line protocol over the Nordic UART service.
//!
//! Its own thread does the (blocking) GATT work; the AI thread reads the state.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use esp32_nimble::utilities::BleUuid;
use esp32_nimble::uuid128;

use crate::ble::{gattc, Ble};
use crate::clock;
use crate::util::{self, Stack};

const MAX_NOTIFS: usize = 20;
const ANDROID_PROTOCOL: &str = "1";
/// How long a new connection may take to encrypt on its own before the robot asks.
const SECURE_AFTER_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Iphone,
    Android,
}

/// ANCS category order (the Android app's are mapped onto it).
pub const CATEGORIES: [&str; 13] = [
    "other",
    "incoming_call",
    "missed_call",
    "voicemail",
    "social",
    "schedule",
    "email",
    "news",
    "health",
    "business",
    "location",
    "entertainment",
    "outgoing_call",
];
const INCOMING_CALL: u8 = 1;
const MISSED_CALL: u8 = 2;
const VOICEMAIL: u8 = 3;
const OUTGOING_CALL: u8 = 12;

#[derive(Debug, Clone, Default)]
pub struct Notif {
    pub uid: u32,
    pub app: String,
    pub title: String,
    pub message: String,
    pub category: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default)]
pub struct Media {
    pub state: MediaState,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub app: String,
}

#[derive(Debug)]
pub enum PhoneEvent {
    Connected(bool),
    /// A phone call is ringing: who.
    Call(String),
    /// What is playing changed (the remote's home screen shows it).
    Media,
}

#[derive(Default)]
struct State {
    kind: Option<Kind>,
    conn: Option<u16>,
    notifs: VecDeque<Notif>,
    media: Media,
}

enum Cmd {
    Connected(u16),
    Encrypted(u16),
    Disconnected(u16),
    Notify(u16, u16, Vec<u8>),
    Uart(Vec<u8>),
    UartSubscribed(u16, bool),
    Media(&'static str),
}

pub struct Phone {
    st: Mutex<State>,
    cmds: Mutex<Sender<Cmd>>,
}

// Apple's services.
const ANCS: BleUuid = uuid128!("7905F431-B5CE-4E99-A40F-4B1E122D00D0");
const ANCS_NS: BleUuid = uuid128!("9FBF120D-6301-42D9-8C58-25E699A21DBD");
const ANCS_CP: BleUuid = uuid128!("69D1D8F3-45E1-49A8-9821-9BBDFDAAD9D9");
const ANCS_DS: BleUuid = uuid128!("22EAC6E9-24D6-4BB5-BE44-B36ACE7C7BFB");
const AMS: BleUuid = uuid128!("89D3502B-0F36-433A-8EF4-C502AD55F8DC");
const AMS_RC: BleUuid = uuid128!("9B3C81D8-57B1-4A8A-B8DF-0E56F7CA51C2");
const AMS_EU: BleUuid = uuid128!("2F7CABCE-808D-411F-9A0C-BB92BA96C102");
const CTS: BleUuid = BleUuid::Uuid16(0x1805);
const CTS_TIME: BleUuid = BleUuid::Uuid16(0x2A2B);

/// Handles found on the iPhone.
#[derive(Default, Clone, Copy)]
struct Apple {
    conn: u16,
    ns: u16,
    cp: u16,
    ds: u16,
    rc: u16,
    eu: u16,
    cts: u16,
}

impl Phone {
    pub fn start<E: From<PhoneEvent> + Send + 'static>(
        ble: Arc<Ble>,
        events: Sender<E>,
    ) -> Arc<Phone> {
        let (tx, rx) = channel();
        let p = Arc::new(Phone {
            st: Mutex::new(State::default()),
            cmds: Mutex::new(tx.clone()),
        });
        let me = p.clone();
        let notify_tx = Mutex::new(tx);
        gattc::on_notify(move |conn, handle, data| {
            let _ = notify_tx
                .lock()
                .unwrap()
                .send(Cmd::Notify(conn, handle, data.to_vec()));
        });
        util::spawn(c"phone", 8192, 5, Stack::Psram, move || {
            me.run(rx, &ble, &events)
        });
        p
    }

    fn post(&self, c: Cmd) {
        let _ = self.cmds.lock().unwrap().send(c);
    }

    pub fn connected(&self, conn: u16) {
        self.post(Cmd::Connected(conn));
    }

    pub fn encrypted(&self, conn: u16) {
        self.post(Cmd::Encrypted(conn));
    }

    pub fn disconnected(&self, conn: u16) {
        self.post(Cmd::Disconnected(conn));
    }

    pub fn uart(&self, data: Vec<u8>) {
        self.post(Cmd::Uart(data));
    }

    pub fn uart_subscribed(&self, conn: u16, on: bool) {
        self.post(Cmd::UartSubscribed(conn, on));
    }

    /// `play`, `pause`, `next` or `previous`.
    pub fn media(&self, action: &'static str) {
        self.post(Cmd::Media(action));
    }

    pub fn kind(&self) -> Option<Kind> {
        self.st.lock().unwrap().kind
    }

    pub fn media_info(&self) -> Media {
        self.st.lock().unwrap().media.clone()
    }

    /// Notifications worth reading out: no calls, nothing empty, no duplicates; newest first.
    pub fn readable(&self) -> Vec<Notif> {
        let st = self.st.lock().unwrap();
        let mut out: Vec<Notif> = Vec::new();
        for n in st.notifs.iter().rev() {
            if n.category == INCOMING_CALL || n.category == OUTGOING_CALL {
                continue;
            }
            if n.message.is_empty() && n.category != MISSED_CALL && n.category != VOICEMAIL {
                continue;
            }
            if out
                .iter()
                .any(|o| o.app == n.app && o.title == n.title && o.message == n.message)
            {
                continue;
            }
            out.push(n.clone());
        }
        out
    }

    // ---------------------------------------------------------------- the thread

    fn run<E: From<PhoneEvent>>(&self, rx: Receiver<Cmd>, ble: &Ble, events: &Sender<E>) {
        let mut apple: Option<Apple> = None;
        let mut fetch: VecDeque<u32> = VecDeque::new();
        let mut ds = Vec::<u8>::new();
        let mut uart = Vec::<u8>::new();
        let mut calls: HashSet<u32> = HashSet::new();
        // New connections to look at once they have had a moment to encrypt (the remote does
        // that itself, and so does a bonded iPhone): (connection, when).
        let mut fresh: Vec<(u16, u64)> = Vec::new();
        loop {
            // An iPhone that connected from its Bluetooth settings waits for the accessory to
            // ask for security: pairing starts only then.
            let now = util::millis();
            fresh.retain(|&(conn, at)| {
                if now < at {
                    return true;
                }
                if apple.is_none()
                    && gattc::encrypted(conn) == Some(false)
                    && gattc::find_service(conn, ANCS).is_some()
                {
                    log::info!("phone: an iPhone on an unencrypted link: asking it to pair");
                    let rc = gattc::secure(conn);
                    if rc != 0 {
                        log::warn!("phone: security request failed ({rc})");
                    }
                }
                false
            });
            // Notification attributes are fetched one at a time between other work.
            let cmd = match rx.recv_timeout(Duration::from_millis(if fetch.is_empty() {
                1000
            } else {
                20
            })) {
                Ok(c) => Some(c),
                Err(_) => None,
            };
            match cmd {
                Some(Cmd::Connected(conn)) => fresh.push((conn, util::millis() + SECURE_AFTER_MS)),
                Some(Cmd::Encrypted(conn)) => {
                    if apple.is_none() {
                        if let Some(a) = discover(conn) {
                            log::info!("phone: iPhone connected");
                            apple = Some(a);
                            {
                                let mut st = self.st.lock().unwrap();
                                st.kind = Some(Kind::Iphone);
                                st.conn = Some(conn);
                                st.notifs.clear();
                            }
                            let _ = events.send(PhoneEvent::Connected(true).into());
                            start_apple(&a);
                        }
                    }
                }
                Some(Cmd::Disconnected(conn)) => {
                    let gone = {
                        let mut st = self.st.lock().unwrap();
                        if st.conn == Some(conn) {
                            *st = State::default();
                            true
                        } else {
                            false
                        }
                    };
                    if gone {
                        log::info!("phone: gone");
                        apple = None;
                        fetch.clear();
                        ds.clear();
                        uart.clear();
                        calls.clear();
                        let _ = events.send(PhoneEvent::Connected(false).into());
                    }
                }
                Some(Cmd::Notify(conn, handle, data)) => {
                    let Some(a) = apple.filter(|a| a.conn == conn) else {
                        continue;
                    };
                    if handle == a.ns {
                        self.on_ancs_event(&data, &mut fetch, events);
                    } else if handle == a.ds {
                        ds.extend_from_slice(&data);
                        if let Some((n, used)) = parse_attributes(&ds) {
                            ds.drain(..used);
                            self.on_ancs_attributes(n, events);
                        }
                    } else if handle == a.eu {
                        if self.on_ams_update(&data) {
                            let _ = events.send(PhoneEvent::Media.into());
                        }
                    } else if handle == a.cts {
                        set_clock_from_cts(&data);
                    }
                }
                Some(Cmd::UartSubscribed(conn, on)) => {
                    if !on {
                        self.disconnected(conn);
                    } else {
                        let mut st = self.st.lock().unwrap();
                        st.conn = Some(conn);
                    }
                }
                Some(Cmd::Uart(data)) => {
                    uart.extend_from_slice(&data);
                    while let Some(end) = frame_end(&uart) {
                        let line: Vec<u8> = uart.drain(..end).collect();
                        let line = String::from_utf8_lossy(&line).trim().to_string();
                        self.android_line(&line, ble, events, &mut calls);
                    }
                    if uart.len() > 4096 {
                        uart.clear();
                    }
                }
                Some(Cmd::Media(action)) => {
                    let kind = self.kind();
                    match (kind, apple) {
                        (Some(Kind::Iphone), Some(a)) if a.rc != 0 => {
                            let code = match action {
                                "play" => 0,
                                "pause" => 1,
                                "next" => 3,
                                _ => 4,
                            };
                            let _ = gattc::write(a.conn, a.rc, &[code]);
                        }
                        (Some(Kind::Android), _) => {
                            let cmd = match action {
                                "play" => "mediaPlay",
                                "pause" => "mediaPause",
                                "next" => "mediaNext",
                                _ => "mediaPrev",
                            };
                            ble.uart_send(&format!("{cmd}\n"));
                        }
                        _ => {}
                    }
                }
                None => {}
            }
            // One attribute request at a time; the answer comes on the Data Source.
            if let (Some(a), Some(uid)) = (apple, fetch.front().copied()) {
                if ds.is_empty() {
                    fetch.pop_front();
                    let mut req = vec![0u8];
                    req.extend_from_slice(&uid.to_le_bytes());
                    req.push(0); // app identifier
                    req.extend_from_slice(&[1, 64, 0]); // title, up to 64 bytes
                    req.extend_from_slice(&[3, 240, 0]); // message, up to 240 bytes
                    if let Err(e) = gattc::write(a.conn, a.cp, &req) {
                        log::warn!("phone: notification {uid}: {e}");
                    }
                }
            }
        }
    }

    fn add(&self, n: Notif) {
        let mut st = self.st.lock().unwrap();
        if let Some(old) = st.notifs.iter_mut().find(|o| o.uid == n.uid) {
            *old = n;
            return;
        }
        while st.notifs.len() >= MAX_NOTIFS {
            st.notifs.pop_front();
        }
        st.notifs.push_back(n);
    }

    fn remove(&self, uid: u32) {
        self.st.lock().unwrap().notifs.retain(|n| n.uid != uid);
    }

    /// Notification Source: event id, flags, category, count, uid.
    fn on_ancs_event<E: From<PhoneEvent>>(
        &self,
        d: &[u8],
        fetch: &mut VecDeque<u32>,
        _events: &Sender<E>,
    ) {
        if d.len() < 8 {
            return;
        }
        let uid = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
        match d[0] {
            0 | 1 => {
                // Keep what is known until the attributes arrive.
                self.add(Notif {
                    uid,
                    category: d[2].min(12),
                    ..Default::default()
                });
                if !fetch.contains(&uid) {
                    fetch.push_back(uid);
                }
            }
            2 => {
                self.remove(uid);
                fetch.retain(|u| *u != uid);
            }
            _ => {}
        }
    }

    fn on_ancs_attributes<E: From<PhoneEvent>>(&self, mut n: Notif, events: &Sender<E>) {
        let category = {
            let st = self.st.lock().unwrap();
            st.notifs
                .iter()
                .find(|o| o.uid == n.uid)
                .map(|o| o.category)
        };
        let Some(category) = category else {
            return; // removed meanwhile
        };
        n.category = category;
        if category == INCOMING_CALL {
            let from = if n.title.is_empty() {
                n.message.clone()
            } else {
                n.title.clone()
            };
            let _ = events.send(PhoneEvent::Call(from).into());
        }
        self.add(n);
    }

    /// AMS Entity Update: entity, attribute, flags, value. Returns whether the media changed.
    fn on_ams_update(&self, d: &[u8]) -> bool {
        if d.len() < 3 {
            return false;
        }
        let value = String::from_utf8_lossy(&d[3..]).into_owned();
        let mut st = self.st.lock().unwrap();
        let m = &mut st.media;
        match (d[0], d[1]) {
            // Player: PlaybackInfo "state,rate,elapsed".
            (0, 1) => {
                m.state = match value.split(',').next() {
                    Some("1") | Some("2") | Some("3") => MediaState::Playing,
                    Some("0") => MediaState::Paused,
                    _ => MediaState::Stopped,
                }
            }
            (0, 0) => m.app = value,
            (2, 0) => m.artist = value,
            (2, 1) => m.album = value,
            (2, 2) => m.title = value,
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------------- Android

    fn android_line<E: From<PhoneEvent>>(
        &self,
        line: &str,
        ble: &Ble,
        events: &Sender<E>,
        calls: &mut HashSet<u32>,
    ) {
        let f = split(line);
        let Some(cmd) = f.first().map(|s| s.as_str()) else {
            return;
        };
        let arg = |i: usize| f.get(i).cloned().unwrap_or_default();
        let num = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let connected = self.kind() == Some(Kind::Android);
        match cmd {
            "hello" => {
                ble.uart_send(&format!("version;{ANDROID_PROTOCOL};v2.1\n"));
                if arg(1) == ANDROID_PROTOCOL && !connected {
                    log::info!("phone: Android app connected");
                    self.st.lock().unwrap().kind = Some(Kind::Android);
                    let _ = events.send(PhoneEvent::Connected(true).into());
                    ble.uart_send("time\nnotifList\n");
                }
            }
            _ if !connected => {}
            "time" => {
                let ts = arg(1).parse::<i64>().unwrap_or(0);
                let tz = arg(2).parse::<i64>().unwrap_or(0);
                if ts != 0 {
                    clock::set_epoch(ts + tz * 60);
                }
            }
            "notifAdd" | "notifModify" if f.len() >= 7 => {
                self.add(Notif {
                    uid: num(1) as u32,
                    title: arg(2),
                    message: arg(3),
                    app: arg(4),
                    category: android_category(num(6)),
                });
            }
            "notifDel" => self.remove(num(1) as u32),
            "callIncoming" if f.len() >= 4 => {
                let uid = num(1) as u32;
                calls.insert(uid);
                let from = if arg(2).is_empty() { arg(3) } else { arg(2) };
                self.add(Notif {
                    uid,
                    title: arg(2),
                    message: arg(3),
                    category: INCOMING_CALL,
                    ..Default::default()
                });
                let _ = events.send(PhoneEvent::Call(from).into());
            }
            "callIncomingStop" => {
                let uid = num(1) as u32;
                calls.remove(&uid);
                self.remove(uid);
            }
            "mediaState" => {
                self.st.lock().unwrap().media.state = match num(1) {
                    1 => MediaState::Playing,
                    2 => MediaState::Paused,
                    _ => MediaState::Stopped,
                };
                let _ = events.send(PhoneEvent::Media.into());
            }
            "mediaInfo" if f.len() >= 5 => {
                {
                    let mut st = self.st.lock().unwrap();
                    st.media.title = arg(1);
                    st.media.artist = arg(2);
                    st.media.album = arg(3);
                    st.media.app = arg(4);
                }
                let _ = events.send(PhoneEvent::Media.into());
            }
            _ => {}
        }
    }
}

/// The app's categories onto ANCS's.
fn android_category(v: u64) -> u8 {
    match v {
        1 | 2 => 4,
        3 => 5,
        4 => MISSED_CALL,
        5 => 7,
        6 => 10,
        7 => 11,
        _ => 0,
    }
}

/// Fields split at ';', where a field may be `<length>:<bytes>` (and then hold ';' or '\n').
fn split(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let mut n = 0usize;
        while i < b.len() && b[i].is_ascii_digit() {
            n = n * 10 + (b[i] - b'0') as usize;
            i += 1;
        }
        if i > start && i < b.len() && b[i] == b':' {
            let from = i + 1;
            let to = (from + n).min(b.len());
            out.push(String::from_utf8_lossy(&b[from..to]).into_owned());
            i = to + 1;
            continue;
        }
        i = start;
        while i < b.len() && b[i] != b';' {
            i += 1;
        }
        out.push(String::from_utf8_lossy(&b[start..i]).into_owned());
        i += 1;
    }
    if s.ends_with(';') {
        out.push(String::new());
    }
    out
}

/// Where the first complete line ends (just past its '\n'), honouring length-prefixed fields.
fn frame_end(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        let start = i;
        let mut n = 0usize;
        while i < b.len() && b[i].is_ascii_digit() {
            n = n * 10 + (b[i] - b'0') as usize;
            i += 1;
        }
        if i > start && i < b.len() && b[i] == b':' {
            i += 1 + n;
            if i > b.len() {
                return None;
            }
        } else {
            i = start;
            while i < b.len() && b[i] != b';' && b[i] != b'\n' {
                i += 1;
            }
        }
        match b.get(i) {
            None => return None,
            Some(b'\n') => return Some(i + 1),
            Some(_) => i += 1,
        }
    }
}

/// A Data Source answer to Get Notification Attributes (app, title, message), once complete:
/// the notification and the bytes it took.
fn parse_attributes(d: &[u8]) -> Option<(Notif, usize)> {
    if d.is_empty() || d[0] != 0 {
        return Some((Notif::default(), d.len())); // not ours: drop it
    }
    if d.len() < 5 {
        return None;
    }
    let mut n = Notif {
        uid: u32::from_le_bytes([d[1], d[2], d[3], d[4]]),
        ..Default::default()
    };
    let mut i = 5;
    for _ in 0..3 {
        if i + 3 > d.len() {
            return None;
        }
        let id = d[i];
        let len = u16::from_le_bytes([d[i + 1], d[i + 2]]) as usize;
        if i + 3 + len > d.len() {
            return None;
        }
        let v = String::from_utf8_lossy(&d[i + 3..i + 3 + len]).into_owned();
        match id {
            0 => n.app = v,
            1 => n.title = v,
            3 => n.message = v,
            _ => {}
        }
        i += 3 + len;
    }
    Some((n, i))
}

/// Find Apple's services on the peer; None if it is not an iPhone.
fn discover(conn: u16) -> Option<Apple> {
    let range = gattc::find_service(conn, ANCS)?;
    let mut a = Apple {
        conn,
        ..Default::default()
    };
    let chrs = gattc::characteristics(conn, range);
    for (i, c) in chrs.iter().enumerate() {
        let end = chrs.get(i + 1).map(|n| n.def_handle - 1).unwrap_or(range.1);
        if c.uuid == ANCS_NS {
            a.ns = c.val_handle;
            if let Some(d) = gattc::cccd(conn, c.val_handle, end) {
                let _ = gattc::subscribe(conn, d, 1);
            }
        } else if c.uuid == ANCS_DS {
            a.ds = c.val_handle;
            if let Some(d) = gattc::cccd(conn, c.val_handle, end) {
                let _ = gattc::subscribe(conn, d, 1);
            }
        } else if c.uuid == ANCS_CP {
            a.cp = c.val_handle;
        }
    }
    if a.ns == 0 || a.cp == 0 || a.ds == 0 {
        return None;
    }
    if let Some(range) = gattc::find_service(conn, AMS) {
        let chrs = gattc::characteristics(conn, range);
        for (i, c) in chrs.iter().enumerate() {
            let end = chrs.get(i + 1).map(|n| n.def_handle - 1).unwrap_or(range.1);
            if c.uuid == AMS_RC {
                a.rc = c.val_handle;
            } else if c.uuid == AMS_EU {
                a.eu = c.val_handle;
                if let Some(d) = gattc::cccd(conn, c.val_handle, end) {
                    let _ = gattc::subscribe(conn, d, 1);
                }
            }
        }
    }
    if let Some(range) = gattc::find_service(conn, CTS) {
        let chrs = gattc::characteristics(conn, range);
        for (i, c) in chrs.iter().enumerate() {
            if c.uuid == CTS_TIME {
                let end = chrs.get(i + 1).map(|n| n.def_handle - 1).unwrap_or(range.1);
                a.cts = c.val_handle;
                if let Ok(t) = gattc::read(conn, c.val_handle) {
                    set_clock_from_cts(&t);
                }
                if let Some(d) = gattc::cccd(conn, c.val_handle, end) {
                    let _ = gattc::subscribe(conn, d, 1);
                }
            }
        }
    }
    Some(a)
}

/// Ask AMS for the player state and the track.
fn start_apple(a: &Apple) {
    if a.eu != 0 {
        let _ = gattc::write(a.conn, a.eu, &[0, 0, 1]);
        let _ = gattc::write(a.conn, a.eu, &[2, 0, 1, 2]);
    }
}

/// Current Time: year (u16), month, day, hours, minutes, seconds (local time).
fn set_clock_from_cts(t: &[u8]) {
    if t.len() >= 7 {
        clock::set_local(
            u16::from_le_bytes([t[0], t[1]]),
            t[2],
            t[3],
            t[4],
            t[5],
            t[6],
        );
    }
}
