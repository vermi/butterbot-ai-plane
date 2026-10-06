//! BLE provisioning: the `Ctrl::Command::LinkConfig` message written to the robot's
//! stock Com service. Layout mirrors `LinkConfigData` in `ButterBot-Common/src/CtrlData.h`.

/// Com service (custom UUID, source bytes are LSB-first).
pub const COM_SERVICE: &str = "3685cae7-8251-9891-4440-bd80bc790819";
/// Controller -> robot, write with response.
pub const COM_RX_CHAR: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
/// Robot -> controller, notify.
pub const COM_TX_CHAR: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";
/// Prefix of the robot's scan-response name.
pub const ROBOT_NAME_PREFIX: &str = "CircuitMess Butter Bot";

/// `Ctrl::Command` value appended after the stock commands (EnterRC..Scenario = 0..8).
pub const CMD_LINK_CONFIG: u32 = 9;
/// Join Wi-Fi, dial the plane and enter AI mode, using the stored configuration. No payload.
pub const CMD_LINK_WAKE: u32 = 10;
/// Set the internal-RAM thresholds the robot requires before starting Wi-Fi.
/// Payload: `LinkTuneData { u32 min_free, u32 min_block }`, zero keeps a value.
pub const CMD_LINK_TUNE: u32 = 11;

pub fn link_tune_packet(min_free: u32, min_block: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&CMD_LINK_TUNE.to_le_bytes());
    out.extend_from_slice(&min_free.to_le_bytes());
    out.extend_from_slice(&min_block.to_le_bytes());
    out
}

pub const SSID_LEN: usize = 32;
pub const PASS_LEN: usize = 64;
pub const HOST_LEN: usize = 64;
/// sizeof(LinkConfigData): three char arrays then a u16, no padding.
pub const LINK_CONFIG_LEN: usize = SSID_LEN + PASS_LEN + HOST_LEN + 2;

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("{0} is too long (max {1} bytes)")]
    TooLong(&'static str, usize),
    #[error("Wi-Fi password must be empty (open network) or 8 to 63 characters")]
    BadPassword,
    #[error("{0} must not be empty")]
    Empty(&'static str),
}

/// Full GATT write: `Ctrl{type u32 LE}` followed by `LinkConfigData`.
pub fn link_config_packet(
    ssid: &str,
    pass: &str,
    host: &str,
    port: u16,
) -> Result<Vec<u8>, ProvisionError> {
    if !pass.is_empty() && pass.len() < 8 {
        return Err(ProvisionError::BadPassword);
    }
    if ssid.is_empty() {
        return Err(ProvisionError::Empty("ssid"));
    }
    if host.is_empty() {
        return Err(ProvisionError::Empty("host"));
    }
    let mut out = Vec::with_capacity(4 + LINK_CONFIG_LEN);
    out.extend_from_slice(&CMD_LINK_CONFIG.to_le_bytes());
    put_cstr(&mut out, "ssid", ssid, SSID_LEN)?;
    put_cstr(&mut out, "password", pass, PASS_LEN)?;
    put_cstr(&mut out, "host", host, HOST_LEN)?;
    out.extend_from_slice(&port.to_le_bytes());
    Ok(out)
}

fn put_cstr(
    out: &mut Vec<u8>,
    what: &'static str,
    s: &str,
    len: usize,
) -> Result<(), ProvisionError> {
    // Leave room for the NUL terminator.
    if s.len() >= len {
        return Err(ProvisionError::TooLong(what, len - 1));
    }
    out.extend_from_slice(s.as_bytes());
    out.resize(out.len() + (len - s.len()), 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_layout() {
        let p = link_config_packet("home", "secret12", "192.168.1.10", 7531).unwrap();
        assert_eq!(p.len(), 4 + 162);
        assert_eq!(&p[0..4], &[9, 0, 0, 0]);
        assert_eq!(&p[4..8], b"home");
        assert_eq!(p[8], 0);
        assert_eq!(&p[36..42], b"secret");
        assert_eq!(&p[100..112], b"192.168.1.10");
        assert_eq!(&p[164..166], &7531u16.to_le_bytes());
    }

    #[test]
    fn rejects_overlong() {
        assert!(link_config_packet(&"x".repeat(32), "", "h", 1).is_err());
        assert!(link_config_packet(&"x".repeat(31), "", "h", 1).is_ok());
        assert!(link_config_packet("s", "short", "h", 1).is_err());
        assert!(link_config_packet("s", &"p".repeat(63), "h", 1).is_ok());
        assert!(link_config_packet("s", &"p".repeat(64), "h", 1).is_err());
    }
}
