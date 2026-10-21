//! Names for the stock `BB` state/action enums (`ButterBot-Common/src/BBData.h`),
//! used to make mirrored BLE telemetry readable.

pub const STATES: &[&str] = &["idle", "listen", "scenario", "rc"];

pub const IDLE_ACTIONS: &[&str] = &[
    "none",
    "wander",
    "ramble",
    "person",
    "poke",
    "summon",
    "fall",
    "upside_down",
    "pick_up",
    "shake",
    "module_change",
    "gas",
    "intruder",
    "battery_low",
    "charging",
    "charging_full",
    "phone_notif",
    "phone_connect",
    "observe",
    "gas_configure_start",
    "gas_configure_end",
    "battery_level",
    "shut_up",
    "cant_move",
    "home",
];

/// `BB::Action::Scenario`, in enum order. Index = wire value.
pub const SCENARIOS: &[&str] = &[
    "read_notifs",
    "eight_ball",
    "fact",
    "joke",
    "pass_the_butter",
    "profanity",
    "you_pass_butter",
    "dice_roll",
    "current_time",
    "voice_control",
    "dance",
    "face_detect",
    "face_detect_forget",
    "ir_train",
    "ir_forget",
    "ir_forget_all",
    "ir_list",
    "ir_action",
    "led_module",
    "led_turn_on",
    "led_turn_off",
    "led_strobe",
    "led_breathe",
    "led_faster",
    "led_slower",
    "gas_module",
    "temp_hum_module",
    "temp_hum_scale_celsius",
    "temp_hum_scale_fahrenheit",
    "temp_hum_scale_kelvin",
    "intruder_detection_on",
    "intruder_detection_off",
    "shutdown",
    "phone_not_connected",
    "phone_list_notifs",
    "phone_whats_playing",
    "phone_next_song",
    "phone_prev_song",
    "phone_play_music",
    "phone_stop_music",
    "whats_this",
    // AI mode's own entries in the controller's menu.
    "ai_see",
    "ai_news",
    "ai_weather",
    "ai_explore",
    "ai_story",
    "ai_focus",
    "ai_activity",
    "ai_peek",
];

/// Scenarios that load on-robot speech/vision models; the firmware refuses them in AI mode
/// (their memory needs were never budgeted next to Wi-Fi). Mirrors RemoteState::startScenario.
pub const NEEDS_MODELS: &[&str] = &[
    "eight_ball",
    "dice_roll",
    "voice_control",
    "dance",
    "face_detect",
    "face_detect_forget",
    "ir_train",
    "ir_forget",
    "whats_this",
];

/// Scenarios that can run in AI mode.
pub fn remote_scenarios() -> impl Iterator<Item = &'static str> {
    SCENARIOS
        .iter()
        .copied()
        .filter(|s| !NEEDS_MODELS.contains(s))
}

pub fn scenario_id(name: &str) -> Option<u32> {
    SCENARIOS.iter().position(|s| *s == name).map(|i| i as u32)
}

/// Human-readable name of a mirrored `BB` message, e.g. `idle/battery_level`.
pub fn describe(state: u32, action: u32) -> String {
    let s = STATES.get(state as usize).copied().unwrap_or("?");
    let a = match state {
        0 => IDLE_ACTIONS.get(action as usize).copied(),
        1 => Some("listen"),
        2 => SCENARIOS.get(action as usize).copied(),
        3 => Some("feed_status"),
        _ => None,
    };
    match a {
        Some(a) => format!("{s}/{a}"),
        None => format!("{s}/{action}"),
    }
}

fn pick(names: &[&'static str], i: u8) -> &'static str {
    names.get(i as usize).copied().unwrap_or("?")
}

/// Decode the payloads of the most useful messages into JSON. Others return `None`.
pub fn decode(state: u32, action: u32, data: &[u8]) -> Option<serde_json::Value> {
    use serde_json::json;
    match (state, action, data) {
        // BatteryLevelData { u8 level; ChargeStatus u8 }
        (0, 21, [level, charge, ..]) => Some(json!({
            "level": level,
            "charge": pick(&["unplugged", "charging", "full"], *charge),
        })),
        // ShutUpData { bool muted }
        (0, 22, [muted, ..]) => Some(json!({ "muted": *muted != 0 })),
        // CantMoveData { Reason }
        (0, 23, [r, ..]) => Some(json!({ "reason": pick(&["charging", "unstable"], *r) })),
        // ListenData { Phase }
        (1, _, [p, ..]) => Some(json!({
            "phase": pick(&["listening", "not_understand", "abort"], *p),
        })),
        // TempHumModuleData { id, missing, i16 temp, u8 hum, scale }
        (2, 26, [_, missing, t0, t1, hum, ..]) => Some(json!({
            "missing": *missing != 0,
            "temperature": i16::from_le_bytes([*t0, *t1]),
            "humidity": hum,
        })),
        // DiceRollData { phase, type, count, pad, u16 result }
        (2, 7, [phase, _, count, _, r0, r1, ..]) => Some(json!({
            "phase": phase, "count": count, "result": u16::from_le_bytes([*r0, *r1]),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_enum_order() {
        assert_eq!(SCENARIOS.len(), 49);
        assert_eq!(scenario_id("whats_this"), Some(40));
        assert_eq!(scenario_id("dance"), Some(10));
        assert_eq!(IDLE_ACTIONS.len(), 25);
        assert_eq!(describe(0, 21), "idle/battery_level");
        assert_eq!(describe(2, 3), "scenario/joke");
    }

    #[test]
    fn decodes_battery() {
        let v = decode(0, 21, &[4, 1]).unwrap();
        assert_eq!(v["level"], 4);
        assert_eq!(v["charge"], "charging");
    }
}
