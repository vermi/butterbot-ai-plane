//! Built-in brains ([agent] kind = "realtime" or "text"): the plane talks to an OpenAI model
//! itself instead of running an MCP agent, so replies can start before they are finished.
//! Both use the same events (`Robot::next_event`) and the same tools, defined here.

pub mod dream;
pub mod realtime;
pub mod sleep;
pub mod text;

use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::BrainConfig;
use crate::control::Robot;

const INSTRUCTIONS: &str = include_str!("instructions.md");
/// Pictures go to the model at most this wide: smaller is quicker to upload and read, and the
/// camera's 640x480 holds little more detail than this.
const PICTURE_WIDTH: u32 = 512;

/// A camera frame as a data URL for the model, scaled down to `PICTURE_WIDTH`.
pub fn picture_url(jpeg: &[u8]) -> String {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let shrunk = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()
        .filter(|img| img.width() > PICTURE_WIDTH)
        .and_then(|img| {
            let small = img.resize(
                PICTURE_WIDTH,
                PICTURE_WIDTH,
                image::imageops::FilterType::Triangle,
            );
            let mut out = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
                .encode_image(&small)
                .ok()
                .map(|_| out)
        });
    format!(
        "data:image/jpeg;base64,{}",
        b64(shrunk.as_deref().unwrap_or(jpeg))
    )
}

/// The brain's instructions: persona, owner, known faces and a recap of recent conversation.
pub async fn instructions(robot: &Robot) -> String {
    crate::agent::with_context(robot, INSTRUCTIONS).await
}

/// Function tools, in the format both the Realtime and Responses APIs take.
pub fn tools() -> Vec<Value> {
    let f = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        })
    };
    vec![
        f(
            "move",
            "Drive a short distance. Stops by itself; refuses when tipped over or plugged in.",
            json!({
                "direction": { "type": "string", "enum": ["forward", "backward", "left", "right"] },
                "seconds": { "type": "number", "description": "0.2 to 5 (default 1)" },
                "speed": { "type": "integer", "description": "1 to 100 (default 60)" },
            }),
            &["direction"],
        ),
        f(
            "turn",
            "Spin in place; positive degrees turn right, negative left.",
            json!({ "degrees": { "type": "number" } }),
            &["degrees"],
        ),
        f("stop", "Stop moving now.", json!({}), &[]),
        f(
            "look",
            "See what your camera sees right now.",
            json!({}),
            &[],
        ),
        f(
            "stay_quiet",
            "Say nothing this time (for example after a quiet-spell notice when nothing is worth saying).",
            json!({}),
            &[],
        ),
        f(
            "web_search",
            "Look something up on the web (weather, news, facts). Returns a short answer.",
            json!({ "query": { "type": "string" } }),
            &["query"],
        ),
        f(
            "focus",
            "Focus mode: stay quiet until called by name or the time is up.",
            json!({ "minutes": { "type": "integer", "description": "default 60" } }),
            &[],
        ),
        f("unfocus", "End focus mode early.", json!({}), &[]),
        f(
            "set_activity",
            "How much you do of your own accord, when someone asks you to be livelier, calmer or quieter: quiet (speak only when spoken to), calm, normal, lively (chat often and roam about). Focus mode is separate and temporary.",
            json!({ "level": { "type": "string", "enum": ["quiet", "calm", "normal", "lively"] } }),
            &["level"],
        ),
        f(
            "gesture",
            "Body language with your tracks: wiggle (happy), nod (yes), shake (no), bounce (playful), twirl (excited), shiver (scared), approach (curious), back_off (shy, sad), sulk (grumpy), or dance (to the beat of music you hear, if any). Use them to show how you feel, without announcing it.",
            json!({
                "name": { "type": "string", "enum": crate::gestures::GESTURES.map(|(n, _)| n) },
                "bpm": { "type": "number", "description": "dance only: beats a minute (default: the music's)" },
                "seconds": { "type": "integer", "description": "dance only: how long (default 10)" }
            }),
            &["name"],
        ),
        f(
            "feel",
            "Say how you feel now. It lasts (fading over an hour or so), comes back with every message, sets how soon you get your own ideas, and shows on your face.",
            json!({
                "emotion": { "type": "string", "enum": crate::mood::Emotion::ALL.map(|e| e.name()) },
                "intensity": { "type": "number", "description": "0.1 (a touch) to 1 (overwhelming); default 0.6" },
                "why": { "type": "string", "description": "a few words" }
            }),
            &["emotion"],
        ),
        f(
            "wander",
            "Wander a little: turn somewhere at random and drive a short hop. You get a picture of where you ended up.",
            json!({}),
            &[],
        ),
        f(
            "end_conversation",
            "The person is done talking with you (for now): after your reply you only listen to people who start with your name again. You stay on.",
            json!({}),
            &[],
        ),
        f(
            "power_off",
            "Switch yourself off (only when asked). You stay off until someone switches you on.",
            json!({}),
            &[],
        ),
        f(
            "remember_face",
            "Learn the face of the one person in front of you under a name (only with their agreement). Takes 5-12 s.",
            json!({ "name": { "type": "string" } }),
            &["name"],
        ),
        f(
            "forget_face",
            "Forget someone's face.",
            json!({ "name": { "type": "string" } }),
            &["name"],
        ),
        f(
            "who_is_here",
            "Who (known faces) is in front of your camera right now.",
            json!({}),
            &[],
        ),
        f(
            "phone",
            "The phone paired with you over Bluetooth: `status` (connected, what is playing), `notifications` (then tell the person briefly, in your own words), or music controls.",
            json!({ "action": { "type": "string", "enum": ["status", "notifications", "play", "pause", "next", "previous"] } }),
            &["action"],
        ),
        f(
            "remember",
            "Remember something worth knowing next time (you forget conversations after a while): what people like, plans, names, what happened. Not passing chit-chat.",
            json!({ "fact": { "type": "string", "description": "One short sentence, with names" } }),
            &["fact"],
        ),
        f(
            "forget",
            "Forget something you remembered that is no longer true.",
            json!({ "id": { "type": "integer" } }),
            &["id"],
        ),
        f(
            "remind",
            "Set a reminder: it comes back to you when due, and you then tell the person. Give in_minutes, or at (local time, \"HH:MM\").",
            json!({
                "text": { "type": "string", "description": "What to remind them of" },
                "in_minutes": { "type": "number" },
                "at": { "type": "string", "description": "\"HH:MM\", 24-hour local time" },
            }),
            &["text"],
        ),
        f(
            "apple_tv",
            "The Apple TV in the living room: what's playing (now_playing, status), play, pause, play_pause, next, previous, volume_up, volume_down, set_volume (value 0-100), turn_on, turn_off, apps, launch (value: app name), and remote buttons (home, menu, select, up, down, left, right).",
            json!({
                "action": { "type": "string", "enum": crate::appletv::ACTIONS },
                "value": { "type": "string", "description": "set_volume: 0-100; launch: the app" }
            }),
            &["action"],
        ),
        f(
            "remember_voice",
            "Remember the voice of the person who just spoke to you, as `name` (only if they ask or agree). After that, messages from them come with `speaker`, even when they are off camera. forget: true drops it.",
            json!({
                "name": { "type": "string" },
                "forget": { "type": "boolean" }
            }),
            &["name"],
        ),
        f(
            "intend",
            "Make a plan of your own: something you mean to do, at a time (in_minutes, or at \"HH:MM\") or some day (neither). A timed plan comes back to you when due; some-day plans come with your free moments as ideas.",
            json!({
                "text": { "type": "string", "description": "what you mean to do" },
                "in_minutes": { "type": "number" },
                "at": { "type": "string", "description": "\"HH:MM\", 24-hour local time" },
            }),
            &["text"],
        ),
        f(
            "plans",
            "List your own plans, or mark one done (or dropped) by id.",
            json!({ "done": { "type": "integer", "description": "id of a plan you did or dropped" } }),
            &[],
        ),
        f(
            "write_diary",
            "Write your diary entry for a day (default today), in your own words. Replaces that day's entry.",
            json!({
                "text": { "type": "string" },
                "day": { "type": "string", "description": "YYYY-MM-DD (default today)" },
            }),
            &["text"],
        ),
        f(
            "read_diary",
            "Read your diary: a day's entry (default the latest), and which days have one.",
            json!({ "day": { "type": "string", "description": "YYYY-MM-DD" } }),
            &[],
        ),
        f(
            "locate",
            "Where things are: for everything your camera's detectors see right now (faces, pets, objects), which way it is (degrees to turn, positive right), how far along the floor, and how many seconds of move (speed 50) get you there. Instant: no new picture. With x and y (percent across and down your last picture), where that spot on the floor is.",
            json!({
                "x": { "type": "number", "description": "percent across the picture, 0 left to 100 right" },
                "y": { "type": "number", "description": "percent down the picture, 0 top to 100 bottom" },
            }),
            &[],
        ),
        f(
            "approach",
            "Go up to something you see: a name (a person, a pet) or a kind of thing (dog, chair, bottle, person), or a spot (x, y: percent of your last picture). Turns to face it and drives up in short steps, checking with the camera, and stops about stop_cm short (default 30). Then you see what is there.",
            json!({
                "what": { "type": "string" },
                "x": { "type": "number" },
                "y": { "type": "number" },
                "stop_cm": { "type": "number" },
            }),
            &[],
        ),
        f(
            "recall",
            "Search everything you know by meaning: your memories (old ones too), your diary, and past days (who came by, what was said, your dreams). Use it when something might have come up before.",
            json!({ "query": { "type": "string", "description": "what to recall, in a few words" } }),
            &["query"],
        ),
        f(
            "reminders",
            "List the reminders that are set, or cancel one by id.",
            json!({ "cancel": { "type": "integer", "description": "id of a reminder to cancel" } }),
            &[],
        ),
        f(
            "body",
            "Your body's state: battery, whether you are plugged in, on the floor, any add-on card, and the local time.",
            json!({}),
            &[],
        ),
        f(
            "set_volume",
            "Set your speaker volume.",
            json!({ "level": { "type": "number", "description": "0.0 to 1.0" } }),
            &["level"],
        ),
    ]
}

/// What a tool call produced.
pub enum Outcome {
    /// Text for the model.
    Text(String),
    /// Text plus a picture for the model.
    Image(String, Bytes),
    /// Say nothing now.
    Quiet,
    /// The robot is switching off: end the session.
    Off,
}

/// Run one tool call from the model.
pub async fn run_tool(robot: &Robot, cfg: &BrainConfig, name: &str, args: &Value) -> Outcome {
    let res: anyhow::Result<Outcome> = async {
        Ok(match name {
            "move" => {
                let s = args["speed"].as_u64().unwrap_or(60).clamp(1, 100) as i8;
                let (l, r) = match args["direction"].as_str().unwrap_or("forward") {
                    "backward" => (-s, -s),
                    "left" => (-s, s),
                    "right" => (s, -s),
                    _ => (s, s),
                };
                let ms = (args["seconds"].as_f64().unwrap_or(1.0).clamp(0.2, 5.0) * 1000.0) as u64;
                Outcome::Text(serde_json::to_string(&robot.drive_for(l, r, ms).await?)?)
            }
            "turn" => {
                let d = args["degrees"]
                    .as_f64()
                    .unwrap_or(90.0)
                    .clamp(-720.0, 720.0);
                Outcome::Text(serde_json::to_string(&robot.turn(d, 70).await?)?)
            }
            "stop" => {
                robot.stop().await?;
                Outcome::Text("stopped".into())
            }
            "look" => match robot.snapshot(None, 12).await {
                Ok(f) => Outcome::Image("This is what you see now.".into(), f.jpeg),
                Err(e) => Outcome::Text(format!("cannot see: {e:#}")),
            },
            "stay_quiet" => Outcome::Quiet,
            "set_activity" => match args["level"]
                .as_str()
                .and_then(crate::life::Activity::parse)
            {
                Some(a) => {
                    robot.set_activity(a);
                    Outcome::Text(format!("Activity is now {}.", a.name()))
                }
                None => Outcome::Text("level must be quiet, calm, normal or lively".into()),
            },
            "gesture" => {
                let name = args["name"].as_str().unwrap_or_default();
                let r = if name == "dance" {
                    crate::gestures::dance(
                        robot,
                        args["bpm"].as_f64(),
                        args["seconds"].as_u64(),
                    )
                    .await
                } else {
                    crate::gestures::gesture(robot, name).await
                };
                Outcome::Text(r.unwrap_or_else(|e| format!("could not: {e:#}")))
            }
            "feel" => match args["emotion"]
                .as_str()
                .and_then(crate::mood::Emotion::parse)
            {
                Some(e) => {
                    let intensity = args["intensity"].as_f64().unwrap_or(0.6) as f32;
                    robot
                        .mood
                        .feel(e, intensity, args["why"].as_str().unwrap_or_default());
                    Outcome::Text(robot.mood.for_agent().to_string())
                }
                None => Outcome::Text(format!(
                    "emotion must be one of: {}",
                    crate::mood::Emotion::ALL.map(|e| e.name()).join(", ")
                )),
            },
            "wander" => {
                let what = robot.wander().await?;
                // A frame taken after it stopped.
                tokio::time::sleep(Duration::from_millis(800)).await;
                match robot.snapshot(None, 12).await {
                    Ok(f) => Outcome::Image(what, f.jpeg),
                    Err(_) => Outcome::Text(what),
                }
            }
            "web_search" => Outcome::Text(
                text::web_search(cfg, args["query"].as_str().unwrap_or_default()).await?,
            ),
            "focus" => {
                let m = args["minutes"]
                    .as_u64()
                    .unwrap_or(robot.hub.config.life.default_focus_minutes)
                    .clamp(1, 24 * 60);
                robot.focus(Duration::from_secs(m * 60));
                Outcome::Text(format!("Focus mode for {m} minutes."))
            }
            "unfocus" => {
                robot.unfocus();
                Outcome::Text("Focus mode off.".into())
            }
            "end_conversation" => {
                robot.ears.end_conversation();
                Outcome::Text("Conversation ended; say a short bye.".into())
            }
            "power_off" => {
                robot.power_off().await?;
                Outcome::Off
            }
            "remember_face" => {
                let r = robot
                    .faces
                    .remember(robot, args["name"].as_str().unwrap_or_default())
                    .await?;
                Outcome::Text(format!(
                    "Remembered {} ({} samples).",
                    r.name, r.samples_added
                ))
            }
            "forget_face" => {
                let n = args["name"].as_str().unwrap_or_default();
                Outcome::Text(if robot.faces.forget(n).await? {
                    format!("Forgot {n}.")
                } else {
                    format!("Nobody called {n} is enrolled.")
                })
            }
            "who_is_here" => {
                let seen = robot.faces.who_is_here(robot).await?;
                Outcome::Text(serde_json::to_string(&seen)?)
            }
            "phone" => {
                let action = args["action"].as_str().unwrap_or("status");
                Outcome::Text(robot.phone(action).await?.to_string())
            }
            "remember" => {
                let fact = robot
                    .memory
                    .remember(args["fact"].as_str().unwrap_or_default())?;
                Outcome::Text(format!("remembered (id {})", fact.id))
            }
            "forget" => {
                let gone = robot
                    .memory
                    .forget(args["id"].as_u64().unwrap_or(0) as u32)?;
                Outcome::Text(if gone { "forgotten" } else { "no such memory" }.into())
            }
            "remind" => {
                let r = robot.reminders.add(
                    args["text"].as_str().unwrap_or_default(),
                    args["in_minutes"].as_f64(),
                    args["at"].as_str(),
                )?;
                Outcome::Text(format!(
                    "Set for {} (it is {} now), id {}.",
                    r.due_local(),
                    chrono::Local::now().format("%H:%M"),
                    r.id
                ))
            }
            "apple_tv" => {
                let r = robot
                    .appletv
                    .act(
                        robot,
                        args["action"].as_str().unwrap_or_default(),
                        args["value"].as_str(),
                    )
                    .await;
                Outcome::Text(match r {
                    Ok(v) => v.to_string(),
                    Err(e) => format!("could not: {e:#}"),
                })
            }
            "remember_voice" => {
                let name = args["name"].as_str().unwrap_or_default();
                if args["forget"].as_bool() == Some(true) {
                    let gone = robot.voices.forget(name)?;
                    Outcome::Text(if gone { "forgotten" } else { "I did not know that voice" }.into())
                } else {
                    let n = robot.voices.remember_last(name)?;
                    Outcome::Text(format!("I'll know {name}'s voice ({n} sample(s); it gets better as they talk to me)."))
                }
            }
            "intend" => {
                let p = robot.intentions.add(
                    args["text"].as_str().unwrap_or_default(),
                    args["in_minutes"].as_f64(),
                    args["at"].as_str(),
                )?;
                Outcome::Text(format!("Planned ({}), id {}.", p.when(), p.id))
            }
            "plans" => {
                if let Some(id) = args["done"].as_u64() {
                    let gone = robot.intentions.done(id as u32)?;
                    Outcome::Text(if gone { "done" } else { "no such plan" }.into())
                } else {
                    let list: Vec<Value> = robot
                        .intentions
                        .list()
                        .iter()
                        .map(|p| json!({ "id": p.id, "when": p.when(), "text": p.text }))
                        .collect();
                    Outcome::Text(json!({ "plans": list }).to_string())
                }
            }
            "write_diary" => {
                let day = diary_day(args["day"].as_str())?;
                robot
                    .diary
                    .write(day, args["text"].as_str().unwrap_or_default())?;
                Outcome::Text(format!("Written for {day}."))
            }
            "locate" => {
                let v = match (args["x"].as_f64(), args["y"].as_f64()) {
                    (Some(x), Some(y)) => robot.locate_point(x / 100.0, y / 100.0),
                    _ => json!({ "in_view": robot.locate_all() }),
                };
                if v["in_view"].as_array().is_some_and(|a| a.is_empty()) {
                    Outcome::Text("Your detectors don't see anything right now. Look first, or locate a spot with x and y.".into())
                } else {
                    Outcome::Text(v.to_string())
                }
            }
            "approach" => {
                let stop = args["stop_cm"].as_f64().unwrap_or(30.0).clamp(10.0, 200.0);
                let what = match (args["what"].as_str(), args["x"].as_f64(), args["y"].as_f64()) {
                    (Some(w), ..) if !w.trim().is_empty() => robot.approach(w, stop).await?,
                    (_, Some(x), Some(y)) => {
                        robot.approach_point(x / 100.0, y / 100.0, stop).await?
                    }
                    _ => return Ok(Outcome::Text("approach needs what, or x and y".into())),
                };
                match robot.snapshot(None, 12).await {
                    Ok(f) => Outcome::Image(what, f.jpeg),
                    Err(_) => Outcome::Text(what),
                }
            }
            "recall" => {
                let query = args["query"].as_str().unwrap_or_default();
                match robot.recall.search(robot, query, 6, 0.2, &[]).await {
                    Ok(hits) if hits.is_empty() => {
                        Outcome::Text("Nothing comes to mind.".into())
                    }
                    Ok(hits) => Outcome::Text(crate::recall::hits_json(&hits).to_string()),
                    Err(e) => Outcome::Text(format!("could not recall: {e:#}")),
                }
            }
            "read_diary" => {
                let days = robot.diary.days();
                let day = match args["day"].as_str() {
                    Some(d) => Some(diary_day(Some(d))?),
                    None => days.first().copied(),
                };
                let entry = day.and_then(|d| robot.diary.read(d));
                Outcome::Text(
                    json!({
                        "day": day.map(|d| d.to_string()),
                        "entry": entry,
                        "days_with_entries": days.iter().take(30).map(|d| d.to_string()).collect::<Vec<_>>(),
                    })
                    .to_string(),
                )
            }
            "reminders" => {
                if let Some(id) = args["cancel"].as_u64() {
                    let gone = robot.reminders.cancel(id as u32)?;
                    Outcome::Text(
                        if gone {
                            "cancelled"
                        } else {
                            "no such reminder"
                        }
                        .into(),
                    )
                } else {
                    let list: Vec<Value> = robot
                        .reminders
                        .list()
                        .iter()
                        .map(|r| json!({ "id": r.id, "at": r.due_local(), "text": r.text }))
                        .collect();
                    Outcome::Text(json!({ "reminders": list }).to_string())
                }
            }
            "body" => {
                let s = robot.hub.status.lock().await.clone();
                let battery = s.battery.unwrap_or_default();
                let cards = robot
                    .modules()
                    .await
                    .map(|m| m["modules"].clone())
                    .unwrap_or_default();
                Outcome::Text(
                    json!({
                        "battery_percent": battery["pct"],
                        "plugged_in": battery["charging"].as_str().map(|c| c != "unplugged"),
                        "charging": battery["charging"],
                        "on_the_floor": robot.on_the_floor(),
                        "add_on_cards": cards,
                        "local_time": chrono::Local::now().format("%A %H:%M").to_string(),
                    })
                    .to_string(),
                )
            }
            "set_volume" => {
                robot
                    .set_volume(args["level"].as_f64().unwrap_or(0.7) as f32)
                    .await?;
                Outcome::Text("ok".into())
            }
            other => Outcome::Text(format!("unknown tool {other}")),
        })
    }
    .await;
    res.unwrap_or_else(|e| Outcome::Text(format!("failed: {e:#}")))
}

/// What happened, as the text the model gets; with the picture, if any.
pub fn describe(ev: &crate::control::AgentEvent) -> Option<(String, Option<Bytes>)> {
    use crate::control::AgentEvent;
    match ev {
        AgentEvent::Command { info, image } => {
            let text = info["text"].as_str().unwrap_or_default();
            let mut who: Vec<String> = info["in_view"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            who.dedup();
            // A known voice says who spoke; faces only say who is in front of the camera.
            let prefix = match (info["speaker"].as_str(), who.as_slice()) {
                (Some(name), _) => format!("({name}, by voice) "),
                (None, []) => String::new(),
                (None, [one]) => format!("({one} is in view, so probably the one talking) "),
                (None, many) => format!(
                    "({} are in view; you can't tell which of them is talking) ",
                    many.join(", ")
                ),
            };
            let woke = info["woke_from_nap_minutes"]
                .as_u64()
                .map(|m| {
                    format!("(This woke you: you were napping on the charger for {m} minutes.) ")
                })
                .unwrap_or_default();
            Some((
                format!(
                    "{woke}{prefix}{text}{}{}",
                    recall_note(info),
                    mood_note(info)
                ),
                image.clone(),
            ))
        }
        AgentEvent::Nudge { info, image } => {
            let in_view = info["in_view"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "nobody you can see".into());
            let text = match info["nudge"].as_str() {
                Some("arrived") => format!(
                    "[{} just came into view. In view: {in_view}.]",
                    info["name"].as_str().unwrap_or("someone")
                ),
                Some("poked") => "[Someone poked you with the remote. React out loud, briefly; they may want to chat.]".to_string(),
                Some("call") => format!(
                    "[The phone paired with you is ringing: {} is calling. Tell your person, briefly.]",
                    info["from"].as_str().unwrap_or("someone")
                ),
                Some("reminder") => format!(
                    "[Reminder (set at {}): {}. Tell the person now, briefly.]",
                    info["set_at"].as_str().unwrap_or("earlier"),
                    info["text"].as_str().unwrap_or_default()
                ),
                Some("battery") => format!(
                    "[Your battery is at {}% and you are not plugged in. Ask to be charged, briefly.]",
                    info["percent"].as_u64().unwrap_or(0)
                ),
                Some("power") => {
                    let percent = info["percent"].as_u64().unwrap_or(0);
                    if info["plugged_in"] == true {
                        format!(
                            "[You were just plugged in (battery {percent}%). You can't drive on the cable. Once things are quiet you'll doze off until you're unplugged; your name still wakes you. React if you feel like it, in a few words, or say nothing.]"
                        )
                    } else {
                        let nap = match info["napped_minutes"].as_u64() {
                            Some(m) => {
                                let dreams = info["dreams"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|d| d.as_str())
                                            .collect::<Vec<_>>()
                                            .join(" / ")
                                    })
                                    .filter(|d| !d.is_empty())
                                    .map(|d| format!(" You dreamt: {d}"))
                                    .unwrap_or_default();
                                let kept = info["remembered"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|d| d.as_str())
                                            .collect::<Vec<_>>()
                                            .join("; ")
                                    })
                                    .filter(|d| !d.is_empty())
                                    .map(|d| format!(" Sleeping on the day, you kept in mind: {d}."))
                                    .unwrap_or_default();
                                format!(
                                    " You napped for {m} minutes on the charger.{dreams}{kept}"
                                )
                            }
                            None => String::new(),
                        };
                        format!(
                            "[You were just unplugged (battery {percent}%): you're free to move again.{nap} Go about your business; mention a dream only if you feel like it.]"
                        )
                    }
                }
                Some("handled") => {
                    let what = match info["how"].as_str().unwrap_or_default() {
                        "pickup" => "Someone just picked you up",
                        "putdown" => "Someone just put you down",
                        "shake" => "Someone is shaking you",
                        "fall" => "You just fell or were dropped",
                        "upside_down" => "You are upside down",
                        _ => "Someone is handling you",
                    };
                    format!("[{what}. In view: {in_view}. React out loud, briefly.]")
                }
                Some("heard") => {
                    let what = info["what"].as_str().unwrap_or("something");
                    let pets = info["pets"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    match what {
                        w if crate::sounds::ALARMS.contains(&w) => {
                            format!("[You hear an alarm ({w})! Tell your person right away.]")
                        }
                        "Doorbell" | "Ding-dong" | "Knock" => format!(
                            "[You heard {} Someone may be at the door: tell your person, briefly.]",
                            if what == "Knock" {
                                "a knock."
                            } else {
                                "the doorbell."
                            }
                        ),
                        "Bark" | "Meow" if !pets.is_empty() => format!(
                            "[You just heard a {}. Pets here: {pets}. React if you like, or call stay_quiet.]",
                            what.to_lowercase()
                        ),
                        _ => format!(
                            "[You just heard: {what}. React if you like, or call stay_quiet.]"
                        ),
                    }
                }
                Some("noticed") => {
                    let what = info["what"].as_str().unwrap_or("something");
                    // At the robot's resolution a dog can come out as a cat.
                    let pets = info["pets"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    let seen = if crate::objects::is_animal(what) && !pets.is_empty() {
                        format!(
                            "An animal (the detector guesses a {what}; pets here: {pets}; look at the picture)"
                        )
                    } else if crate::objects::is_animal(what) {
                        format!("An animal (maybe a {what}; look at the picture to tell)")
                    } else {
                        format!("Something that looks like a {what}")
                    };
                    format!(
                        "[{seen} just came into view. In view: {in_view}. React if you like, or call stay_quiet.]"
                    )
                }
                Some("diary") => format!(
                    "[Evening: time to write your diary for {}. What happened that day:\n{}\nWrite it with write_diary, in your own words and for yourself: what stood out, how you felt, what you wonder about. If something comes to mind for tomorrow, make it a plan (intend). Nobody needs to hear it, so don't say anything.]",
                    info["day"].as_str().unwrap_or("today"),
                    info["what_happened"].as_str().unwrap_or("(nothing noted)")
                ),
                Some("reflect") => "[That conversation is over: think back on it, the way you would after talking with a friend. Usually something is worth keeping: who it was and what they wanted, how they seemed, something new about them, the home or their plans (remember), or something you said you'd do, want to follow up on or got curious about (intend). Keep one or two such things. Only if it really was nothing (a hello, a misheard word), call stay_quiet. Don't say anything either way.]".to_string(),
                Some("morning") => format!(
                    "[A new day{}. What would you like to do or find out today? Make one to three plans of your own with intend: things to explore, check on, learn about or bring up with someone (with a time, or for some day). Don't say anything.]",
                    info["plans_now"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .map(|n| format!(" (you have {n} plans already: see plans)"))
                        .unwrap_or_default()
                ),
                Some("plan") => format!(
                    "[Your own plan is due (you made it {}): {}. Do it, put it off (intend again), or let it go (plans, done).]",
                    info["planned_at"].as_str().unwrap_or("earlier"),
                    info["text"].as_str().unwrap_or_default()
                ),
                Some("music") => format!(
                    "[Music: {}{}. In view: {in_view}. {}.]",
                    info["what"].as_str().unwrap_or("music is playing"),
                    info["bpm"]
                        .as_u64()
                        .map(|b| format!(" (about {b} beats a minute)"))
                        .unwrap_or_default(),
                    info["suggestion"].as_str().unwrap_or("Enjoy it")
                ),
                _ => {
                    let plans = info["your_plans"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .filter(|p| !p.is_empty())
                        .map(|p| format!(" Your some-day plans: {p}."))
                        .unwrap_or_default();
                    format!(
                        "[Time to yourself (nobody has talked with you for {} minutes). In view: {in_view}. This time is yours, for as long as you like: do whatever you are curious about, one thing after another (look, move, look closer, remember, make a plan, try something), until you're done. Or rest (stay_quiet). An idea, if you want one: {}.{plans}]",
                        info["quiet_for_minutes"].as_u64().unwrap_or(0),
                        info["suggestion"].as_str().unwrap_or("rest")
                    )
                }
            };
            let cable = if info["plugged_in"].is_string() {
                " (You are on a cable: you cannot drive.)"
            } else {
                ""
            };
            Some((
                format!("{text}{cable}{}{}", recall_note(info), mood_note(info)),
                image.clone(),
            ))
        }
        _ => None,
    }
}

/// What it brings to mind (`crate::recall`), as a short note after the message.
fn recall_note(info: &Value) -> String {
    let Some(hits) = info["brings_to_mind"].as_array().filter(|h| !h.is_empty()) else {
        return String::new();
    };
    let items: Vec<String> = hits
        .iter()
        .map(|h| {
            let text: String = h["text"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(240)
                .collect();
            format!(
                "[{}, {}] {}",
                h["when"].as_str().unwrap_or("?"),
                h["from"].as_str().unwrap_or("?"),
                text.replace('\n', " / ")
            )
        })
        .collect();
    format!(" (This brings to mind: {}.)", items.join("; "))
}

/// How it feels, from an event's `mood`, as a short note after the message.
fn mood_note(info: &Value) -> String {
    let m = &info["mood"];
    let Some(feeling) = m["feeling"].as_str() else {
        return String::new();
    };
    let strength = match m["intensity"].as_f64().unwrap_or(0.0) {
        i if i >= 0.7 => "very ",
        i if i < 0.35 => "a little ",
        _ => "",
    };
    let why = m["why"]
        .as_str()
        .filter(|w| !w.is_empty())
        .map(|w| format!(", because {w}"))
        .unwrap_or_default();
    format!(" (You feel {strength}{feeling}{why}.)")
}

/// OpenAI's speech endpoint returns 24 kHz PCM.
const TTS_RATE: u32 = 24_000;

/// Speaks a reply sentence by sentence while it is still being written: in the robot's own voice
/// (`[brain] voice = "robot"`) or with OpenAI speech.
pub struct Speech {
    tx: mpsc::UnboundedSender<String>,
    task: tokio::task::JoinHandle<Result<()>>,
    pending: String,
    said: String,
    robot: Robot,
    hushes: u64,
}

impl Speech {
    pub fn start(robot: &Robot) -> Speech {
        let (tx, rx) = mpsc::unbounded_channel();
        let hushes = robot.hush_count();
        Speech {
            tx,
            task: tokio::spawn(speak(robot.clone(), rx, hushes)),
            pending: String::new(),
            said: String::new(),
            robot: robot.clone(),
            hushes,
        }
    }

    /// More of the reply; every complete sentence goes out right away.
    pub fn push(&mut self, delta: &str) {
        self.pending.push_str(delta);
        self.said.push_str(delta);
        while let Some(end) = sentence_end(&self.pending) {
            let s: String = self.pending.drain(..end).collect();
            let _ = self.tx.send(s.trim().to_string());
        }
    }

    /// The reply is complete: speak the rest and wait. Returns everything that was said.
    pub async fn finish(self) -> Result<String> {
        if !self.pending.trim().is_empty() {
            let _ = self.tx.send(self.pending.trim().to_string());
        }
        drop(self.tx);
        self.task.await??;
        let said = self.said.trim().to_string();
        if self.robot.hush_count() != self.hushes {
            return Ok(format!("{said} (cut off)"));
        }
        Ok(said)
    }
}

/// Where the first complete sentence ends (after its punctuation), if one is complete.
fn sentence_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(1)).find_map(|i| {
        (matches!(b[i], b'.' | b'!' | b'?' | b'\n') && b[i + 1].is_ascii_whitespace())
            .then_some(i + 1)
    })
}

/// Speak sentences in order as they arrive, until someone cuts the robot off (`hushes` changes).
async fn speak(
    robot: Robot,
    mut sentences: mpsc::UnboundedReceiver<String>,
    hushes: u64,
) -> Result<()> {
    let cfg = &robot.hub.config.brain;
    if cfg.voice == "robot" {
        while let Some(s) = sentences.recv().await {
            if !s.is_empty() && robot.hush_count() == hushes {
                robot.say_quietly(&s).await?;
            }
        }
        return Ok(());
    }
    let key = cfg.api_key()?;
    let http = text::client();
    let mut stream = None;
    while let Some(s) = sentences.recv().await {
        if s.is_empty() || robot.hush_count() != hushes {
            continue;
        }
        let resp = http
            .post(format!("{}/audio/speech", text::API))
            .bearer_auth(&key)
            .json(&json!({
                "model": cfg.tts_model,
                "voice": cfg.voice,
                "input": s,
                "response_format": "pcm",
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("speech: {}", resp.text().await.unwrap_or_default());
        }
        if stream.is_none() {
            stream = Some(robot.speaker_stream(TTS_RATE).await?);
        }
        let mut odd: Option<u8> = None;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let mut bytes = chunk?.to_vec();
            if let Some(b) = odd.take() {
                bytes.insert(0, b);
            }
            if bytes.len() % 2 == 1 {
                odd = bytes.pop();
            }
            if robot.hush_count() != hushes {
                break;
            }
            if let Some(st) = stream.as_mut() {
                st.push(&bbproto::frame::pcm_from_le(&bytes));
            }
        }
    }
    if let Some(st) = stream {
        st.finish().await?;
    }
    Ok(())
}

/// A diary day: "YYYY-MM-DD", or today.
fn diary_day(day: Option<&str>) -> Result<chrono::NaiveDate> {
    match day {
        Some(d) => chrono::NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("day must look like 2026-10-20")),
        None => Ok(chrono::Local::now().date_naive()),
    }
}

#[cfg(test)]
mod tests {
    use super::{picture_url, sentence_end};

    #[test]
    fn pictures_are_scaled_down_for_the_model() {
        use base64::Engine;
        let img = image::RgbImage::from_pixel(640, 480, image::Rgb([90, 120, 200]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&img)
            .unwrap();
        let url = picture_url(&jpeg);
        let b64 = url.strip_prefix("data:image/jpeg;base64,").unwrap();
        let back = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let small = image::load_from_memory(&back).unwrap();
        assert_eq!((small.width(), small.height()), (512, 384));
    }

    #[test]
    fn splits_sentences() {
        assert_eq!(sentence_end("Hello there! How"), Some(12));
        assert_eq!(sentence_end("It is 3.5 degrees. Nice"), Some(18));
        assert_eq!(sentence_end("No end yet"), None);
        assert_eq!(
            sentence_end("Done."),
            None,
            "the last one waits for the stream to end"
        );
    }
}
