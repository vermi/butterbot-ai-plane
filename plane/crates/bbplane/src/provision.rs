//! One-time BLE provisioning: tell the robot which Wi-Fi to join and where the plane is.
//! Uses the stock Com service with a new `LinkConfig` command; the controller never sends it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bbproto::provision::{
    CMD_LINK_WAKE, COM_RX_CHAR, COM_SERVICE, COM_TX_CHAR, ROBOT_NAME_PREFIX, link_config_packet,
    link_tune_packet,
};
use btleplug::api::{
    Central, Manager as _, Peripheral as _, RetrievePeripheralsOptions, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use uuid::Uuid;

/// `BB` replies the firmware sends on the Com TX characteristic: state Idle (0), this action,
/// payload `u8 ok`.
pub const LINK_CONFIG_ACK: u32 = 100;
pub const LINK_WAKE_ACK: u32 = 101;
pub const LINK_TUNE_ACK: u32 = 102;

pub struct Provision {
    pub ssid: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub name_filter: Option<String>,
}

pub fn default_host() -> Result<String> {
    Ok(local_ip_address::local_ip()
        .context("could not determine this machine's LAN IP; pass --host")?
        .to_string())
}

/// Store Wi-Fi + plane address on the robot. The robot then joins Wi-Fi and enters AI mode.
pub async fn run(p: Provision) -> Result<()> {
    let packet = link_config_packet(&p.ssid, &p.password, &p.host, p.port)?;
    eprintln!(
        "sending Wi-Fi \"{}\" and plane {}:{}",
        p.ssid, p.host, p.port
    );
    match send_command(&packet, LINK_CONFIG_ACK, p.name_filter.as_deref()).await? {
        Some(0) => bail!("robot rejected the configuration"),
        Some(_) => eprintln!(
            "robot stored the configuration; if it is idle it now enters AI mode and dials the plane"
        ),
        None => eprintln!("no confirmation from the robot; it may still have applied it"),
    }
    Ok(())
}

/// Ask a provisioned robot to join Wi-Fi, dial the plane and enter AI mode.
pub async fn wake(name_filter: Option<&str>) -> Result<()> {
    // Just after boot (start-up speech, event routines) the robot answers "busy"; retry briefly.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = send_command(&CMD_LINK_WAKE.to_le_bytes(), LINK_WAKE_ACK, name_filter).await?;
        if status == Some(2) && tokio::time::Instant::now() < deadline {
            eprintln!("robot is busy, retrying...");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        return wake_result(status);
    }
}

fn wake_result(status: Option<u8>) -> Result<()> {
    match status {
        Some(1) => Ok(()),
        Some(0) => bail!("robot has no Wi-Fi configuration; run `bbplane provision` first"),
        Some(2) => bail!(
            "robot is busy (RC mode, a voice command or a scenario); try again when it is idle"
        ),
        Some(c) => bail!("robot answered the wake with unknown code {c}"),
        None => Ok(()), // older firmware or a lost notification; the caller waits for the link anyway
    }
}

/// Set the robot's internal-RAM thresholds for starting Wi-Fi (bytes; 0 keeps a value).
pub async fn tune(min_free: u32, min_block: u32, name_filter: Option<&str>) -> Result<()> {
    match send_command(
        &link_tune_packet(min_free, min_block),
        LINK_TUNE_ACK,
        name_filter,
    )
    .await?
    {
        Some(1) => Ok(()),
        Some(c) => bail!("robot refused the setting (code {c})"),
        None => bail!("no confirmation from the robot"),
    }
}

/// Write one Com command over BLE and wait briefly for the matching `BB` ack.
/// Returns the ack's status byte, or None when no ack arrived.
async fn send_command(
    packet: &[u8],
    ack_action: u32,
    name_filter: Option<&str>,
) -> Result<Option<u8>> {
    let svc = Uuid::parse_str(COM_SERVICE)?;
    let rx_uuid = Uuid::parse_str(COM_RX_CHAR)?;
    let tx_uuid = Uuid::parse_str(COM_TX_CHAR)?;

    let manager = Manager::new().await?;
    let central = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter found"))?;

    // A robot already connected to this machine (e.g. paired in system Bluetooth settings)
    // stops advertising, so ask the OS for connected devices first.
    let robot = match connected_robot(&central, svc).await {
        Some(r) => r,
        None => {
            eprintln!("scanning for \"{ROBOT_NAME_PREFIX}\"... (turn the handheld controller off)");
            central.start_scan(ScanFilter::default()).await?;
            let r = find_robot(&central, name_filter, Duration::from_secs(20)).await;
            central.stop_scan().await.ok();
            r?
        }
    };

    robot.connect().await.context("connecting to the robot")?;
    let result = async {
        robot.discover_services().await?;
        let chars = robot.characteristics();
        // The robot also exposes a real Nordic UART service with the same characteristic
        // UUIDs; select by service.
        let rx = chars
            .iter()
            .find(|c| c.uuid == rx_uuid && c.service_uuid == svc)
            .ok_or_else(|| anyhow!("Com RX characteristic not found; is this a ButterBot?"))?
            .clone();
        let tx = chars
            .iter()
            .find(|c| c.uuid == tx_uuid && c.service_uuid == svc)
            .cloned()
            .ok_or_else(|| anyhow!("Com TX characteristic not found"))?;

        let mut notes = robot.notifications().await?;
        // Subscribing claims the Com service for this connection; the robot ignores writes before it.
        robot.subscribe(&tx).await?;
        robot
            .write(&rx, packet, WriteType::WithResponse)
            .await
            .context("writing to the robot")?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(n)) = tokio::time::timeout_at(deadline, notes.next()).await {
            if n.uuid != tx_uuid || n.value.len() < 12 {
                continue;
            }
            let state = u32::from_le_bytes(n.value[0..4].try_into()?);
            let action = u32::from_le_bytes(n.value[4..8].try_into()?);
            if state == 0 && action == ack_action {
                return Ok(Some(n.value.get(12).copied().unwrap_or(0)));
            }
        }
        Ok(None)
    }
    .await;
    robot.disconnect().await.ok();
    result
}

async fn connected_robot(
    central: &impl Central<Peripheral = Peripheral>,
    svc: Uuid,
) -> Option<Peripheral> {
    // 0x1812 (HID) is what the robot advertises; macOS lists it as a connected HID device.
    let hid = Uuid::from_u128(0x0000_1812_0000_1000_8000_0080_5f9b_34fb);
    let found = central
        .retrieve_peripherals(RetrievePeripheralsOptions {
            identifiers: None,
            services: Some(vec![svc, hid]),
        })
        .await
        .ok()?;
    for p in found {
        let name = p
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|pp| pp.local_name)
            .unwrap_or_default();
        if name.is_empty() || name.starts_with(ROBOT_NAME_PREFIX) {
            eprintln!("using already-connected robot {} ({})", name, p.id());
            return Some(p);
        }
    }
    None
}

async fn find_robot(
    central: &impl Central<Peripheral = Peripheral>,
    filter: Option<&str>,
    timeout: Duration,
) -> Result<Peripheral> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        for p in central.peripherals().await? {
            let Some(props) = p.properties().await? else {
                continue;
            };
            let Some(name) = props.local_name else {
                continue;
            };
            if !name.starts_with(ROBOT_NAME_PREFIX) {
                continue;
            }
            if let Some(f) = filter
                && !props.address.to_string().eq_ignore_ascii_case(f)
                && !p.id().to_string().eq_ignore_ascii_case(f)
            {
                continue;
            }
            eprintln!("found {name} ({})", p.id());
            return Ok(p);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("no ButterBot found; is it powered on and not connected to the controller?")
}
