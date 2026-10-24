//! The realtime brain: one WebSocket session with an OpenAI realtime model per agent session.
//! What people say reaches it as text (the plane transcribes and filters by wake word itself);
//! its speech streams straight to the robot's speaker while it is generated.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use super::{Outcome, describe, instructions, run_tool, tools};
use crate::agent::AgentState;
use crate::control::{AgentEvent, Robot};

/// The realtime API speaks 24 kHz PCM.
const RATE: u32 = 24_000;
/// Every reply processes the whole conversation, so it slows down as that grows: past this many
/// tokens a fresh session starts at the next quiet moment (it gets a recap of what was said).
const CONTEXT_LIMIT: u64 = 20_000;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

type Sink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// A function call the model made.
struct Call {
    call_id: String,
    name: String,
    args: Value,
}

/// Run one session; returns why it ended.
pub async fn session(robot: &Robot, state: &AgentState) -> Result<String> {
    let cfg = &robot.hub.config.brain;
    let key = cfg.api_key()?;
    let mut req = format!(
        "wss://api.openai.com/v1/realtime?model={}",
        cfg.realtime_model
    )
    .into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {key}")).context("bad API key")?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("connecting to the realtime API")?;
    let (mut sink, mut stream) = ws.split();
    // Read continuously (this also answers the server's pings) and hand events over.
    let (tx, mut events) = mpsc::unbounded_channel::<Value>();
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(t) = msg
                && let Ok(v) = serde_json::from_str::<Value>(&t)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let _reader = AbortOnDrop(reader);

    // With the robot's own voice the model writes text and the robot speaks it sentence by
    // sentence; otherwise the model speaks and its audio streams to the speaker.
    let robot_voice = cfg.voice == "robot";
    let mut session = json!({
        "type": "realtime",
        "instructions": instructions(robot).await,
        "tools": tools(),
        "tool_choice": "auto",
    });
    if robot_voice {
        session["output_modalities"] = json!(["text"]);
    } else {
        session["output_modalities"] = json!(["audio"]);
        session["audio"] = json!({
            "output": { "voice": cfg.voice, "format": { "type": "audio/pcm", "rate": RATE } },
        });
    }
    send(
        &mut sink,
        json!({ "type": "session.update", "session": session }),
    )
    .await?;
    state.log(format!(
        "realtime session with {} ({})",
        cfg.realtime_model, cfg.voice
    ));

    loop {
        // Drop anything the server said while we were waiting (session.updated and the like).
        while let Ok(v) = events.try_recv() {
            if v["type"] == "error" {
                state.log(format!("realtime error: {}", v["error"]["message"]));
            }
        }
        let ev = robot.next_event(Duration::from_secs(60), None).await?;
        match &ev {
            AgentEvent::EndSession => return Ok("fresh session".into()),
            AgentEvent::TimedOut { .. } => continue,
            _ => {}
        }
        let Some((text, image)) = describe(&ev) else {
            continue;
        };
        state.log(format!("> {text}"));
        let mut pictures = Vec::new();
        add_message(&mut sink, &text, None).await?;
        if let Some(jpeg) = image.as_deref() {
            pictures.push(add_picture(&mut sink, "(what your camera sees)", jpeg).await?);
        }
        let mut context = 0;
        // Something of its own (not someone talking to it): it decides in silence first, so an
        // action is not announced ("I'm going to look around..."). If it only wanted to talk,
        // what it wrote is then said; after an action it reports in the next response, or not.
        let mut decide_silently = matches!(ev, AgentEvent::Nudge { .. });
        // Answer, running tools, until the model has nothing more to do.
        loop {
            let create = if decide_silently {
                json!({ "type": "response.create", "response": { "output_modalities": ["text"] } })
            } else {
                json!({ "type": "response.create" })
            };
            send(&mut sink, create).await?;
            let silent = std::mem::take(&mut decide_silently);
            let (calls, held, used) =
                respond(&mut events, robot, state, robot_voice, silent).await?;
            context = context.max(used);
            if calls.is_empty() {
                if silent && !held.trim().is_empty() {
                    say_decided(
                        &mut sink,
                        &mut events,
                        robot,
                        state,
                        robot_voice,
                        held.trim(),
                    )
                    .await?;
                }
                break;
            }
            if !held.trim().is_empty() {
                state.log(format!("(kept to itself: {})", held.trim()));
            }
            let mut again = false;
            for c in calls {
                state.log(format!("tool {} {}", c.name, c.args));
                let output = match run_tool(robot, cfg, &c.name, &c.args).await {
                    Outcome::Text(t) => {
                        again = true;
                        t
                    }
                    Outcome::Image(t, jpeg) => {
                        again = true;
                        pictures.push(add_picture(&mut sink, "(your camera)", &jpeg).await?);
                        t
                    }
                    Outcome::Quiet => "ok".into(),
                    Outcome::Off => return Ok("switched off".into()),
                };
                send(
                    &mut sink,
                    json!({
                        "type": "conversation.item.create",
                        "item": { "type": "function_call_output", "call_id": c.call_id, "output": output },
                    }),
                )
                .await?;
            }
            if !again {
                break;
            }
        }
        // Pictures are the bulk of the context and only matter for the answer just given (which
        // says what was in them).
        for id in pictures {
            send(
                &mut sink,
                json!({ "type": "conversation.item.delete", "item_id": id }),
            )
            .await?;
        }
        if context > CONTEXT_LIMIT && !robot.ears.window_open() {
            state.log(format!("context at {context} tokens: starting fresh"));
            return Ok("fresh session (context)".into());
        }
    }
}

/// A picture as its own conversation item, so it can be deleted after use; returns its id.
async fn add_picture(sink: &mut Sink, caption: &str, jpeg: &[u8]) -> Result<String> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!(
        "pic_{}_{}",
        crate::hub::now_ms(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    send(
        sink,
        json!({
            "type": "conversation.item.create",
            "item": {
                "id": id,
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": caption },
                    { "type": "input_image", "image_url": super::picture_url(jpeg) },
                ],
            },
        }),
    )
    .await?;
    Ok(id)
}

/// Say what it decided to say in a silent response: in the robot's voice, or as an out-of-band
/// spoken response (not added to the conversation, which already has it as text).
async fn say_decided(
    sink: &mut Sink,
    events: &mut mpsc::UnboundedReceiver<Value>,
    robot: &Robot,
    state: &AgentState,
    robot_voice: bool,
    text: &str,
) -> Result<()> {
    if robot_voice {
        let mut speech = super::Speech::start(robot);
        speech.push(text);
        let said = speech.finish().await?;
        if !said.trim().is_empty() {
            state.log(format!("< {}", said.trim()));
            robot.note_said(said.trim()).await;
        }
        return Ok(());
    }
    send(
        sink,
        json!({
            "type": "response.create",
            "response": {
                "conversation": "none",
                "input": [],
                "output_modalities": ["audio"],
                "tool_choice": "none",
                "instructions": format!("Say this out loud, exactly as written, in your usual voice and manner, and nothing else:\n{text}"),
            },
        }),
    )
    .await?;
    respond(events, robot, state, false, false).await?;
    Ok(())
}

/// Play one response as it streams in; returns the function calls it made, and with `silent`
/// (a text-only response, not to be heard) the text it wrote.
async fn respond(
    events: &mut mpsc::UnboundedReceiver<Value>,
    robot: &Robot,
    state: &AgentState,
    robot_voice: bool,
    silent: bool,
) -> Result<(Vec<Call>, String, u64)> {
    let mut speech = (robot_voice && !silent).then(|| super::Speech::start(robot));
    let used: u64;
    let mut held = String::new();
    let hushes = robot.hush_count();
    let mut speaker = None;
    let mut said = String::new();
    let mut calls = Vec::new();
    loop {
        let v = tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .context("the realtime API stopped answering")?
            .context("the realtime connection closed")?;
        match v["type"].as_str().unwrap_or_default() {
            "response.output_audio.delta" | "response.audio.delta" => {
                let pcm = bbproto::frame::pcm_from_le(
                    &B64.decode(v["delta"].as_str().unwrap_or_default())
                        .unwrap_or_default(),
                );
                // Cut off: the rest of this answer is not played.
                if robot.hush_count() != hushes {
                    continue;
                }
                if speaker.is_none() {
                    speaker = Some(robot.speaker_stream(RATE).await?);
                }
                if let Some(s) = speaker.as_mut() {
                    s.push(&pcm);
                }
            }
            "response.output_audio_transcript.delta" | "response.audio_transcript.delta" => {
                said.push_str(v["delta"].as_str().unwrap_or_default());
            }
            "response.output_text.delta" | "response.text.delta" => {
                if silent {
                    held.push_str(v["delta"].as_str().unwrap_or_default());
                } else if let Some(sp) = speech.as_mut() {
                    sp.push(v["delta"].as_str().unwrap_or_default());
                }
            }
            "error" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                state.log(format!("realtime error: {msg}"));
                bail!("realtime API: {msg}");
            }
            "response.done" => {
                let r = &v["response"];
                state.add_usage(&r["usage"]);
                used = r["usage"]["input_tokens"].as_u64().unwrap_or(0);
                for item in r["output"].as_array().into_iter().flatten() {
                    if item["type"] == "function_call" {
                        calls.push(Call {
                            call_id: item["call_id"].as_str().unwrap_or_default().into(),
                            name: item["name"].as_str().unwrap_or_default().into(),
                            args: serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                                .unwrap_or_default(),
                        });
                    }
                }
                if r["status"] == "failed" {
                    state.log(format!("response failed: {}", r["status_details"]));
                }
                break;
            }
            _ => {}
        }
    }
    if let Some(s) = speaker {
        s.finish().await?;
    }
    if let Some(sp) = speech {
        said = sp.finish().await?;
    }
    if !said.trim().is_empty() {
        state.log(format!("< {}", said.trim()));
        robot.note_said(said.trim()).await;
    }
    Ok((calls, held, used))
}

async fn send(sink: &mut Sink, v: Value) -> Result<()> {
    sink.send(Message::Text(v.to_string().into()))
        .await
        .context("sending to the realtime API")
}

/// A user message: text, plus a picture if given.
async fn add_message(sink: &mut Sink, text: &str, jpeg: Option<&[u8]>) -> Result<()> {
    let mut content = vec![json!({ "type": "input_text", "text": text })];
    if let Some(j) = jpeg {
        content.push(json!({ "type": "input_image", "image_url": super::picture_url(j) }));
    }
    send(
        sink,
        json!({
            "type": "conversation.item.create",
            "item": { "type": "message", "role": "user", "content": content },
        }),
    )
    .await
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
