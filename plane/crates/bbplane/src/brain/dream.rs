//! Dreams, while it naps on the charger (see `crate::life`): now and then the plane has the text
//! model mix bits of its memories, the day and its plans into a short dream. The agent stays
//! asleep and nothing is said. The dream goes into the day's diary notes, and it hears about it
//! when it is unplugged.

use anyhow::{Result, bail};
use serde_json::json;

use super::text::{API, client, output_text};
use crate::control::Robot;

/// Longest dream kept (the diary keeps notes this long).
const MAX_CHARS: usize = 200;

pub async fn dream(robot: &Robot) {
    match make(robot).await {
        Ok(d) if !d.is_empty() => {
            tracing::info!("dreamt: {d}");
            robot.diary.note("dream", &d);
            robot.life.dreamt(d);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("dream: {e:#}"),
    }
}

async fn make(robot: &Robot) -> Result<String> {
    let cfg = &robot.hub.config.brain;
    let key = cfg.api_key()?;
    let prompt = format!(
        "You are the dreaming mind of Butterbot, a small robot that looks like the Butter Robot \
         (a boxy head with one camera eye on a stalk, two rubber tracks, arms that don't move). He \
         lives in a home with people{pets}, and is napping on his charger now. Write the dream he \
         is having: two short sentences at most, under {MAX_CHARS} characters, first person, present \
         tense, plain text. Dreamlike: mix and twist a few of these fragments of his life into \
         something odd, funny or a little wistful, the way dreams do. Don't explain it.\n\n{}",
        material(robot),
        pets = robot
            .hub
            .config
            .life
            .pets
            .first()
            .map(|p| format!(" and {p}"))
            .unwrap_or_default(),
    );
    let r: serde_json::Value = client()
        .post(format!("{API}/responses"))
        .bearer_auth(key)
        .json(&json!({
            "model": cfg.text_model,
            "reasoning": { "effort": cfg.text_reasoning },
            "input": prompt,
            "max_output_tokens": 400,
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = r["error"]["message"].as_str() {
        bail!("{e}");
    }
    let text = output_text(&r).trim().trim_matches('"').to_string();
    Ok(text.chars().take(MAX_CHARS).collect())
}

/// A handful of things from its life to dream about: some memories, what happened today, its
/// plans, and how it feels.
fn material(robot: &Robot) -> String {
    let mut rng = crate::hub::now_ms() | 1;
    let mut pick = |n: usize| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % n.max(1) as u64) as usize
    };
    let mut facts = robot.memory.facts();
    let mut lines = Vec::new();
    for _ in 0..4.min(facts.len()) {
        let f = facts.swap_remove(pick(facts.len()));
        lines.push(format!("- he remembers: {}", f.text));
    }
    let today = robot.diary.digest(chrono::Local::now().date_naive());
    let today: String = today
        .chars()
        .rev()
        .take(600)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !today.trim().is_empty() {
        lines.push(format!("- today:\n{today}"));
    }
    for p in robot.intentions.list().iter().take(3) {
        lines.push(format!("- he means to: {}", p.text));
    }
    let mood = robot.mood.now();
    lines.push(format!("- he feels {}", mood.emotion.name()));
    lines.join("\n")
}
