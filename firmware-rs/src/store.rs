//! Settings in NVS, in the C firmware's layout (namespace `BB_Link`), so an update keeps them:
//! the link configuration (`config`: `LinkConfigData`, 162 bytes, provisioned over BLE by
//! `bbplane provision`) and the Wi-Fi channel last joined on (`channel`).

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

const NAMESPACE: &str = "BB_Link";
const CONFIG: &str = "config";
const CHANNEL: &str = "channel";
/// `LinkConfigData`: char SSID[32], password[64], host[64]; uint16_t port.
const CONFIG_LEN: usize = 162;

#[derive(Clone, PartialEq, Eq)]
pub struct LinkConfig {
    pub ssid: String,
    pub password: String,
    pub host: String,
    pub port: u16,
}

impl std::fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\" -> {}:{}", self.ssid, self.host, self.port)
    }
}

fn cstr(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0)?;
    String::from_utf8(b[..end].to_vec()).ok()
}

impl LinkConfig {
    pub fn parse(b: &[u8]) -> Option<LinkConfig> {
        if b.len() != CONFIG_LEN {
            return None;
        }
        let c = LinkConfig {
            ssid: cstr(&b[0..32])?,
            password: cstr(&b[32..96])?,
            host: cstr(&b[96..160])?,
            port: u16::from_le_bytes([b[160], b[161]]),
        };
        c.valid().then_some(c)
    }

    pub fn to_bytes(&self) -> [u8; CONFIG_LEN] {
        let mut b = [0u8; CONFIG_LEN];
        let put = |b: &mut [u8], s: &str| {
            let n = s.len().min(b.len() - 1);
            b[..n].copy_from_slice(&s.as_bytes()[..n]);
        };
        put(&mut b[0..32], &self.ssid);
        put(&mut b[32..96], &self.password);
        put(&mut b[96..160], &self.host);
        b[160..162].copy_from_slice(&self.port.to_le_bytes());
        b
    }

    pub fn valid(&self) -> bool {
        // WPA needs 8..63 characters; empty means an open network.
        !self.ssid.is_empty()
            && self.ssid.len() < 32
            && !self.host.is_empty()
            && self.host.len() < 64
            && self.password.len() < 64
            && (self.password.is_empty() || self.password.len() >= 8)
            && self.port != 0
    }
}

pub struct Store {
    nvs: EspNvs<NvsDefault>,
}

impl Store {
    pub fn new(part: EspDefaultNvsPartition) -> Option<Store> {
        match EspNvs::new(part, NAMESPACE, true) {
            Ok(nvs) => Some(Store { nvs }),
            Err(e) => {
                log::error!("nvs: {e}");
                None
            }
        }
    }

    pub fn link_config(&self) -> Option<LinkConfig> {
        let mut buf = [0u8; CONFIG_LEN];
        let b = self.nvs.get_blob(CONFIG, &mut buf).ok()??;
        LinkConfig::parse(b)
    }

    /// Writes flash: call from a thread with an internal-RAM stack.
    pub fn set_link_config(&self, c: &LinkConfig) -> bool {
        self.nvs.set_blob(CONFIG, &c.to_bytes()).is_ok()
    }

    pub fn channel(&self) -> u8 {
        self.nvs.get_u8(CHANNEL).ok().flatten().unwrap_or(0)
    }

    pub fn set_channel(&self, ch: u8) {
        if self.channel() != ch {
            let _ = self.nvs.set_u8(CHANNEL, ch);
        }
    }
}
