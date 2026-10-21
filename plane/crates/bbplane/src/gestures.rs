//! Body language: small moves of the tracks that say something, and dancing.
//!
//! The robot turns slowly (half a turn takes four seconds at full speed), so gestures are quick
//! twitches and nudges rather than spins. They go through the usual drive path, so the same
//! safety checks apply (no driving on a cable, nothing into an obstacle).

use std::time::Duration;

use anyhow::{Result, bail};

use crate::control::Robot;

/// Gestures and what they say, for the agent.
pub const GESTURES: [(&str, &str); 10] = [
    ("wiggle", "happy: a quick left-right wiggle"),
    ("nod", "yes: a little forward-back"),
    ("shake", "no: a slow left-right"),
    ("bounce", "playful: quick hops forward and back"),
    ("twirl", "excited: turn around a good way"),
    ("shiver", "scared or cold: a fast tremble"),
    ("approach", "curious: creep forward a little"),
    ("back_off", "shy, sad or wary: back away slowly"),
    ("sulk", "grumpy: turn away"),
    (
        "dance",
        "dance, to the beat of music playing nearby if there is one",
    ),
];

/// Moves: left and right track speed and how long.
type Move = (i8, i8, u64);

fn moves(name: &str) -> Option<(Vec<Move>, &'static str)> {
    let repeat = |m: &[Move], n: usize| m.iter().copied().cycle().take(m.len() * n).collect();
    Some(match name {
        "wiggle" => (repeat(&[(100, -100, 180), (-100, 100, 180)], 3), "wiggling"),
        "nod" => (repeat(&[(70, 70, 150), (-70, -70, 150)], 2), "nodding"),
        "shake" => (
            repeat(&[(80, -80, 240), (-80, 80, 240)], 2),
            "shaking his head",
        ),
        "bounce" => (repeat(&[(100, 100, 120), (-100, -100, 120)], 3), "bouncing"),
        "twirl" => (vec![(100, -100, 2500)], "twirling"),
        "shiver" => (repeat(&[(100, -100, 70), (-100, 100, 70)], 6), "shivering"),
        "approach" => (vec![(40, 40, 700)], "creeping closer"),
        "back_off" => (vec![(-50, -50, 700)], "backing off"),
        "sulk" => (vec![(-60, 60, 1200)], "sulking"),
        _ => return None,
    })
}

/// Do gesture `name`; returns what happened.
pub async fn gesture(robot: &Robot, name: &str) -> Result<String> {
    let Some((moves, doing)) = moves(name) else {
        bail!(
            "no gesture called {name:?}; there are: {}",
            GESTURES.map(|(n, _)| n).join(", ")
        );
    };
    let total: u64 = moves.iter().map(|m| m.2).sum();
    robot.set_doing(doing, Duration::from_millis(total + 500));
    for (l, r, ms) in moves {
        let out = robot.drive_for(l, r, ms).await?;
        if let Some(why) = out.stopped_early {
            return Ok(format!("Started to {name}, then stopped ({why})."));
        }
    }
    Ok(format!("Did a {name}."))
}

/// Dance for about `seconds`, to `bpm` (else the beat of music it hears, else 110).
pub async fn dance(robot: &Robot, bpm: Option<f64>, seconds: Option<u64>) -> Result<String> {
    let heard = robot.sounds.music().flatten();
    let bpm = bpm.or(heard).unwrap_or(110.0).clamp(60.0, 180.0);
    let beat = Duration::from_secs_f64(60.0 / bpm);
    let beats = (seconds.unwrap_or(10).clamp(3, 30) as f64 / beat.as_secs_f64()).round() as usize;
    robot.set_doing("dancing!", beat * beats as u32 + Duration::from_millis(500));

    // Eight-beat phrases: twist, twist, twist, twist, step, step, shimmy, pause.
    let on = (beat.as_millis() as f64 * 0.7) as u64;
    let phrase: [Move; 8] = [
        (100, -100, on),
        (-100, 100, on),
        (100, -100, on),
        (-100, 100, on),
        (90, 90, on / 2),
        (-90, -90, on / 2),
        (100, -100, on / 3),
        (0, 0, 0),
    ];
    let start = tokio::time::Instant::now();
    for i in 0..beats {
        let (l, r, ms) = phrase[i % phrase.len()];
        if ms > 0 {
            let out = robot.drive_for(l, r, ms).await?;
            if let Some(why) = out.stopped_early {
                return Ok(format!("Danced for {i} beats, then stopped ({why})."));
            }
        }
        // On the beat, whatever the moves took.
        tokio::time::sleep_until(start + beat * (i as u32 + 1)).await;
    }
    Ok(format!(
        "Danced for {:.0} s at {bpm:.0} beats a minute{}.",
        (beat * beats as u32).as_secs_f64(),
        if heard.is_some() {
            " (the music's beat)"
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gesture_has_moves() {
        for (name, _) in GESTURES {
            if name != "dance" {
                assert!(moves(name).is_some(), "{name}");
            }
        }
        assert!(moves("cartwheel").is_none());
    }
}
