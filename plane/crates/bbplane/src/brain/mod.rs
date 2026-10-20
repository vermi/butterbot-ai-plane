//! Built-in brains ([agent] kind = "realtime" or "text"): the plane talks to an OpenAI model
//! itself instead of running an MCP agent, so replies can start before they are finished.
//! Both use the same events (`Robot::next_event`) and the same tools, defined here.

pub mod realtime;
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
                        "on_the_floor": s
                            .proximity
                            .as_ref()
                            .and_then(|p| p["bottom"].as_str())
                            .map(|b| b == "covered"),
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
            // Faces only say who is in front of the camera, not who spoke.
            let prefix = match who.as_slice() {
                [] => String::new(),
                [one] => format!("({one} is in view, so probably the one talking) "),
                many => format!(
                    "({} are in view; you can't tell which of them is talking) ",
                    many.join(", ")
                ),
            };
            Some((format!("{prefix}{text}"), image.clone()))
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
                Some("battery") => {
                    let percent = info["percent"].as_u64().unwrap_or(0);
                    if info["plugged_in"] == true {
                        format!(
                            "[You were just plugged in to charge (battery {percent}%). Say thanks, briefly.]"
                        )
                    } else {
                        format!(
                            "[Your battery is at {percent}% and you are not plugged in. Ask to be charged, briefly.]"
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
                _ => format!(
                    "[Quiet for {} minutes. In view: {in_view}. Idea: {}. Do something, or call stay_quiet.]",
                    info["quiet_for_minutes"].as_u64().unwrap_or(0),
                    info["suggestion"].as_str().unwrap_or("rest")
                ),
            };
            Some((text, image.clone()))
        }
        _ => None,
    }
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
