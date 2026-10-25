//! ButterBot AI firmware: the robot's side of `bbplane`. See README.md.
#![allow(unexpected_cfgs)] // esp_app_desc! checks ESP-IDF cfgs set by esp-idf-sys

mod ai;
mod audio;
mod base;
mod battery;
mod camera;
mod i2c;
mod led;
mod link;
mod logger;
mod motion;
mod net;
mod ota;
mod pins;
mod power;
mod rtc;
mod store;
mod tca;
mod util;

use std::sync::{mpsc, Arc};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::power::Boot;

// The app description in the image: name, version, build time (and the build id, see util).
esp_idf_svc::sys::esp_app_desc! {}

pub const FIRMWARE: &str = "butterbot-ai 0.6";
const CAPS: &[&str] = &["camera", "mic", "speaker", "drive", "ota"];

fn main() {
    esp_idf_svc::sys::link_patches();
    logger::init();

    rtc::check_layout();
    let boot = power::boot_kind();
    power::init_button();
    log::info!("{FIRMWARE} build {} ({boot:?})", util::build_id());

    let peripherals = Peripherals::take().expect("peripherals");
    let sysloop = EspSystemEventLoop::take().expect("event loop");
    let nvs = EspDefaultNvsPartition::take().expect("nvs");
    let store = store::Store::new(nvs).expect("nvs namespace");

    // An update from the plane: update mode only (Wi-Fi and the download), then a restart.
    if let Some((url, size, sha256)) = ota::take_pending() {
        let _led = i2c::Bus::new(0, pins::I2C_MAIN_SDA, pins::I2C_MAIN_SCL)
            .ok()
            .and_then(|bus| tca::Tca::new(&bus).ok().map(|t| (bus, t)))
            .map(|(bus, tca)| (bus, led::Led::start(Arc::new(tca), ai::UPDATING)));
        let net = net::Net::new(peripherals.modem, sysloop).expect("wifi");
        ota::run(&net, &store, &url, size, &sha256);
    }

    if boot == Boot::PowerOn {
        power::hold_check();
    }
    ota::start_probation();

    let bus = i2c::Bus::new(0, pins::I2C_MAIN_SDA, pins::I2C_MAIN_SCL).expect("i2c");
    let tca = Arc::new(tca::Tca::new(&bus).expect("io expander"));
    let led = led::Led::start(tca.clone(), ai::BOOT);

    let base = match base::Base::new(&bus) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            // A new firmware that cannot drive the base board goes back to the previous one.
            if ota::on_probation() {
                log::error!("{e}: back to the previous firmware");
                power::restart();
            }
            log::error!("{e}: switching off");
            util::sleep_ms(2000);
            power::power_off();
        }
    };
    let battery = Arc::new(battery::Battery::new(&tca, base.charge()).expect("battery adc"));
    let motion = match motion::Motion::new(&bus) {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            log::error!("accelerometer: {e}");
            None
        }
    };
    base.set_scanning(true);

    let net = net::Net::new(peripherals.modem, sysloop).expect("wifi");
    let link = link::Link::new(FIRMWARE, CAPS);

    logger::forward_to(link.clone());

    let (tx, rx) = mpsc::channel::<ai::Event>();
    let speaker = match audio::Speaker::start(tca.clone(), tx.clone()) {
        Ok(s) => Some(s),
        Err(e) => {
            log::error!("speaker: {e}");
            None
        }
    };
    if let Some(s) = speaker.clone() {
        link.set_speaker_sink(Some(Box::new(move |pcm| s.push(pcm))));
    }
    let mic = speaker
        .clone()
        .and_then(|s| match audio::Mic::start(link.clone(), s) {
            Ok(m) => Some(m),
            Err(e) => {
                log::error!("microphone: {e}");
                None
            }
        });
    let camera = camera::Camera::start(link.clone());
    base.start(tx.clone());
    battery.start(tx.clone());
    if let Some(m) = &motion {
        m.start(tx.clone());
    }
    power::watch_button(tx.clone());
    link.start(net.clone(), tx.clone());
    base.request_proximity();

    let (free, block) = util::internal_ram();
    log::info!(
        "internal RAM free {free}, largest block {block}; PSRAM free {}",
        util::psram_free()
    );

    ai::Robot::new(ai::Parts {
        link,
        net,
        base,
        battery,
        motion,
        led,
        camera,
        mic,
        speaker,
        store,
    })
    .run(rx, boot);
}
