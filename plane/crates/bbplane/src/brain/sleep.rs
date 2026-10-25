//! What its naps are for (see `crate::life`), once a charge: tidy its memory (merge memories that
//! say the same thing, or that a newer one updates), keep what is worth knowing from the day that
//! it did not think to remember, and bring the recall index up to date (`crate::recall`). The
//! agent stays asleep; it hears what it kept when it wakes up unplugged.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::text::{API, client, output_text};
use crate::control::Robot;

/// Most merges, retirements and new memories in one nap.
const MAX_MERGES: usize = 10;
const MAX_RETIRED: usize = 5;
/// Most plans dropped in one nap, and how old a plan must be to be dropped.
const MAX_DROPPED: usize = 5;
const PLAN_SETTLE_MS: i64 = 2 * 3600 * 1000;
const MAX_KEPT: usize = 3;

/// The nap's work; returns what it kept from the day.
pub async fn upkeep(robot: &Robot) -> Vec<String> {
    if let Err(e) = robot.recall.sync(robot).await {
        tracing::warn!("sleep: recall: {e:#}");
    }
    let merged = match consolidate(robot).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("sleep: merging memories: {e:#}");
            0
        }
    };
    let plans = match tidy_plans(robot).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("sleep: tidying plans: {e:#}");
            0
        }
    };
    let kept = match keep_the_day(robot).await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("sleep: the day: {e:#}");
            Vec::new()
        }
    };
    tracing::info!(
        "sleep: merged {merged} memories, {plans} plan changes, kept {} from the day",
        kept.len()
    );
    if (merged > 0 || !kept.is_empty())
        && let Err(e) = robot.recall.sync(robot).await
    {
        tracing::warn!("sleep: recall: {e:#}");
    }
    kept
}

fn memories(robot: &Robot) -> String {
    robot
        .memory
        .facts()
        .iter()
        .map(|f| format!("{} | {} | {}", f.id, f.date, f.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Merge memories that say the same thing, and retire passing ones to the archive; returns how
/// many changes.
async fn consolidate(robot: &Robot) -> Result<usize> {
    if robot.memory.facts().len() < 4 {
        return Ok(0);
    }
    let answer = ask_json(
        robot,
        &format!(
            "These are a home robot's memories, one a line: id | date remembered | text.\n\n{}\n\n\
             Find groups of memories that say the same thing, or where a newer one updates or \
             corrects an older one. Merge each group into one short memory that keeps every name, \
             date and detail that still holds (where they disagree, the newer one wins). Leave all \
             other memories alone; when unsure, leave them.\n\n\
             Also list the memories not worth knowing every day: a passing moment, something it \
             did once, how things were for a minute (its connection, being handled, where it \
             stopped), or advice to itself it would follow anyway. They go to its archive, where \
             it can still look them up. Keep anything about the people, the pets, the home, \
             preferences, plans and things it learned. Answer in JSON: \
             {{\"merges\": [{{\"ids\": [1, 2], \"text\": \"...\"}}], \"retire\": [3]}}, with \
             empty lists if there is nothing to do.",
            memories(robot)
        ),
    )
    .await?;
    let mut n = 0;
    for m in answer["merges"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_MERGES)
    {
        let ids: Vec<u32> = m["ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_u64().map(|i| i as u32))
            .collect();
        let text = m["text"].as_str().unwrap_or_default();
        if let Some(f) = robot.memory.merge(&ids, text)? {
            tracing::info!("sleep: merged {ids:?} into \"{}\"", f.text);
            n += 1;
        }
    }
    // (Ids of merged memories are gone by now, so a merged one cannot be retired too.)
    let retire: Vec<u32> = answer["retire"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_u64().map(|i| i as u32))
        .take(MAX_RETIRED)
        .collect();
    if !retire.is_empty() {
        let k = robot.memory.retire(&retire)?;
        tracing::info!("sleep: retired {k} passing memories {retire:?} to the archive");
        n += k;
    }
    Ok(n)
}

/// Merge plans made more than once, and drop the ones that are done or no longer make sense
/// (what it remembers says so); returns how many changes.
async fn tidy_plans(robot: &Robot) -> Result<usize> {
    let plans = robot.intentions.list();
    if plans.len() < 2 {
        return Ok(0);
    }
    let lines: String = plans
        .iter()
        .map(|p| {
            format!(
                "{} | made {} | {} | {}",
                p.id,
                crate::reminders::local(p.set_ms).format("%Y-%m-%d"),
                p.when(),
                p.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let answer = ask_json(
        robot,
        &format!(
            "A home robot's own plans, one a line (id | made | when | text):\n{lines}\n\n\
             Its memories (what it knows and has done):\n{}\n\n\
             Tidy the plans. Merge plans that mean the same thing into one, worded plainly. Drop \
             plans its memories show are done, and ones that no longer make sense. Leave the \
             others alone; when unsure, leave them. Answer in JSON: \
             {{\"merges\": [{{\"ids\": [1, 2], \"text\": \"...\"}}], \"drop\": [3]}}, with \
             empty lists if there is nothing to do.",
            memories(robot)
        ),
    )
    .await?;
    let mut n = 0;
    for m in answer["merges"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_MERGES)
    {
        let ids: Vec<u32> = m["ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_u64().map(|i| i as u32))
            .collect();
        let text = crate::memory::clean_text(m["text"].as_str().unwrap_or_default());
        if let Some(p) = robot.intentions.merge(&ids, text)? {
            tracing::info!("sleep: merged plans {ids:?} into \"{}\"", p.text);
            n += 1;
        }
    }
    // Not one made in the last couple of hours (it may have just meant it).
    let now = crate::hub::now_ms() as i64;
    let settled: Vec<u32> = robot
        .intentions
        .list()
        .iter()
        .filter(|p| now - p.set_ms >= PLAN_SETTLE_MS)
        .map(|p| p.id)
        .collect();
    for id in answer["drop"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_u64().map(|i| i as u32))
        .filter(|id| settled.contains(id))
        .take(MAX_DROPPED)
    {
        if robot.intentions.done(id)? {
            tracing::info!("sleep: dropped plan {id}");
            n += 1;
        }
    }
    Ok(n)
}

/// Keep what is worth knowing from the day; returns the new memories.
async fn keep_the_day(robot: &Robot) -> Result<Vec<String>> {
    let now = chrono::Local::now();
    let mut days = vec![now.date_naive()];
    // A morning nap: yesterday evening is still fresh.
    if chrono::Timelike::hour(&now) < 12
        && let Some(y) = now.date_naive().pred_opt()
    {
        days.insert(0, y);
    }
    let digest: String = days
        .iter()
        .map(|d| format!("{d}:\n{}", robot.diary.digest(*d)))
        .collect::<Vec<_>>()
        .join("\n\n");
    if !digest.contains("Conversation") && !digest.contains("People") {
        return Ok(Vec::new());
    }
    let answer = ask_json(
        robot,
        &format!(
            "A home robot is going over its day while it naps. Its memories so far, one a line \
             (id | date | text):\n{}\n\nWhat happened:\n{digest}\n\n\
             Pick at most {MAX_KEPT} things worth remembering for weeks that its memories do not \
             have yet: about the people here (names, what they like, their plans, what is going on \
             in their lives), the pets, the home, or something it learned. Nothing passing \
             (chit-chat, the weather, what it just did, how it felt). Each one short and plain, as \
             a fact (\"Justin is learning the guitar\"). Often there is nothing. Answer in JSON: \
             {{\"remember\": [\"...\"]}}.",
            memories(robot)
        ),
    )
    .await?;
    let mut kept = Vec::new();
    for t in answer["remember"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str())
        .take(MAX_KEPT)
    {
        let before = robot.memory.facts().len();
        let f = robot.memory.remember(t)?;
        if robot.memory.facts().len() != before {
            tracing::info!("sleep: kept \"{}\"", f.text);
            kept.push(f.text);
        }
    }
    Ok(kept)
}

async fn ask_json(robot: &Robot, prompt: &str) -> Result<Value> {
    let cfg = &robot.hub.config.brain;
    let key = cfg.api_key()?;
    let r: Value = client()
        .post(format!("{API}/responses"))
        .bearer_auth(key)
        .json(&json!({
            "model": cfg.text_model,
            "reasoning": { "effort": cfg.text_reasoning },
            "input": prompt,
            "text": { "format": { "type": "json_object" } },
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = r["error"]["message"].as_str() {
        bail!("{e}");
    }
    let text = output_text(&r);
    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
    Ok(serde_json::from_str(
        text.get(start..end).unwrap_or_default(),
    )?)
}
