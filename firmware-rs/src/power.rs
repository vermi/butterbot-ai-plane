//! Power: the hold-to-turn-on check, switching off, the power button, and the crash guard.
//!
//! The robot's power latch stays on while PWDN is an input (the bootloader leaves it so) and goes
//! off when PWDN is driven low. A software restart keeps the power on, so only a cold start asks
//! for the button to be held.

use std::sync::mpsc::Sender;

use esp_idf_svc::sys;

use crate::pins;
use crate::rtc;
use crate::util::{self, millis, sleep_ms, Stack};

/// Crash resets in a row, without AI mode reaching the plane in between, that still bring the
/// robot back on; one more and it does the power-on check (and so switches off).
const MAX_CRASH_RESTARTS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// Switched on with the button.
    PowerOn,
    /// A restart of our own (an update, the plane's restart request): the power stayed on.
    Restart,
    /// After a crash: back on, into AI mode.
    Crash,
}

pub fn boot_kind() -> Boot {
    let why = unsafe { sys::esp_reset_reason() };
    let crashed = matches!(
        why,
        sys::esp_reset_reason_t_ESP_RST_PANIC
            | sys::esp_reset_reason_t_ESP_RST_INT_WDT
            | sys::esp_reset_reason_t_ESP_RST_TASK_WDT
            | sys::esp_reset_reason_t_ESP_RST_WDT
    );
    let mut crashes = 0;
    rtc::update(|r| {
        crashes = match (crashed, r.crash_magic == rtc::MAGIC) {
            (true, true) => r.crashes + 1,
            (true, false) => 1,
            (false, _) => 0,
        };
        r.crashes = crashes;
        r.crash_magic = rtc::MAGIC;
        r.restart_into_ai = 0;
    });
    if why == sys::esp_reset_reason_t_ESP_RST_SW {
        Boot::Restart
    } else if crashed && crashes <= MAX_CRASH_RESTARTS {
        log::error!("restarted after a crash ({crashes} in a row)");
        Boot::Crash
    } else {
        Boot::PowerOn
    }
}

/// AI mode reached the plane: earlier crashes no longer count as a crash loop.
pub fn ai_mode_worked() {
    rtc::update(|r| r.crashes = 0);
}

pub fn init_button() {
    let cfg = sys::gpio_config_t {
        pin_bit_mask: 1u64 << pins::BUTTON,
        mode: sys::gpio_mode_t_GPIO_MODE_INPUT,
        pull_up_en: sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
        pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
        intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
    };
    unsafe { sys::gpio_config(&cfg) };
}

pub fn button_pressed() -> bool {
    unsafe { sys::gpio_get_level(pins::BUTTON) != 0 }
}

/// Switched on with the button: it must be held for a second in all, or the robot goes off again.
pub fn hold_check() {
    // The boot up to here takes about 306 ms of the second (measured on the C firmware).
    let deadline = (1000 - 306).max(millis() + 50);
    while millis() < deadline {
        if !button_pressed() {
            log::info!("power button released early: off");
            power_off();
        }
        sleep_ms(10);
    }
}

/// Switch the robot off. Waits for the button to be released first.
pub fn power_off() -> ! {
    while button_pressed() {
        sleep_ms(10);
    }
    unsafe {
        sys::gpio_set_level(pins::PWDN, 0);
        let cfg = sys::gpio_config_t {
            pin_bit_mask: 1u64 << pins::PWDN,
            mode: sys::gpio_mode_t_GPIO_MODE_OUTPUT,
            pull_up_en: sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
            pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
        };
        sys::gpio_config(&cfg);
    }
    loop {
        sleep_ms(1000);
    }
}

/// Restart into AI mode; the power stays on. The flag is for the C firmware, should it be the
/// one that boots (an update back to it): it would switch the robot off otherwise.
pub fn restart() -> ! {
    rtc::update(|r| r.restart_into_ai = rtc::MAGIC);
    unsafe { sys::esp_restart() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Press,
    Release,
    /// Released within `SHORT_MS` of the press.
    Short,
    /// Held for `LONG_MS` (sent while still held).
    Long,
}

const DEBOUNCE_MS: u64 = 20;
pub const SHORT_MS: u64 = 500;
pub const LONG_MS: u64 = 2000;

/// Watch the power button. The hold at power-on is not a press: it starts after a release.
pub fn watch_button<E: From<Button> + Send + 'static>(tx: Sender<E>) {
    util::spawn(c"button", 2560, 6, Stack::Psram, move || {
        let mut state = button_pressed();
        let mut ignore_until_release = state;
        let mut pressed_at = 0u64;
        let mut long_sent = false;
        loop {
            sleep_ms(DEBOUNCE_MS);
            let raw = button_pressed();
            if raw != state {
                sleep_ms(DEBOUNCE_MS);
                if button_pressed() != raw {
                    continue;
                }
                state = raw;
                if ignore_until_release {
                    ignore_until_release = state;
                    continue;
                }
                if state {
                    pressed_at = millis();
                    long_sent = false;
                    let _ = tx.send(Button::Press.into());
                } else {
                    let _ = tx.send(Button::Release.into());
                    if !long_sent && millis() - pressed_at <= SHORT_MS {
                        let _ = tx.send(Button::Short.into());
                    }
                }
            } else if state
                && !ignore_until_release
                && !long_sent
                && millis() - pressed_at >= LONG_MS
            {
                long_sent = true;
                let _ = tx.send(Button::Long.into());
            }
        }
    });
}
