//! The text brain: a text model (terra by default) writes the reply, and each sentence is spoken
//! as soon as it is complete, while the rest is still being written. Cheaper than realtime audio.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};

use super::{Outcome, describe, instructions, run_tool, tools};
use crate::agent::AgentState;
use crate::config::BrainConfig;
use crate::control::{AgentEvent, Robot};

pub(super) const API: &str = "https://api.openai.com/v1";
pub(super) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default()
}

/// A short web-backed answer, for the `web_search` tool.
pub async fn web_search(cfg: &BrainConfig, query: &str) -> Result<String> {
    let key = cfg.api_key()?;
    let r: Value = client()
        .post(format!("{API}/responses"))
        .bearer_auth(key)
        .json(&json!({
            "model": cfg.search_model,
            "input": format!("Answer in two short spoken sentences, no links or markdown: {query}"),
            "tools": [{ "type": "web_search" }],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = r["error"]["message"].as_str() {
        bail!("web search: {e}");
    }
    Ok(output_text(&r))
}

/// All output text of a Responses API result.
pub(super) fn output_text(r: &Value) -> String {
    r["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "message")
        .flat_map(|i| i["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(String::from))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run one session; returns why it ended.
pub async fn session(robot: &Robot, state: &AgentState) -> Result<String> {
    let cfg = &robot.hub.config.brain;
    let key = cfg.api_key()?;
    let http = client();
    let instructions = instructions(robot).await;
    let mut tools = tools();
    tools.retain(|t| t["name"] != "web_search");
    tools.push(json!({ "type": "web_search" }));
    let mut previous: Option<String> = None;
    state.log(format!(
        "text session with {} ({} voice)",
        cfg.text_model, cfg.voice
    ));
    loop {
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
        let mut content = vec![json!({ "type": "input_text", "text": text })];
        if let Some(j) = &image {
            content.push(image_part(j));
        }
        let mut input = vec![json!({ "role": "user", "content": content })];
        loop {
            let body = json!({
                "model": cfg.text_model,
                "instructions": instructions,
                "input": input,
                "previous_response_id": previous,
                "tools": tools,
                "reasoning": { "effort": cfg.text_reasoning },
                "stream": true,
            });
            let (id, calls) = respond(&http, &key, body, robot, state).await?;
            previous = id;
            if calls.is_empty() {
                break;
            }
            input.clear();
            let mut again = false;
            for (call_id, name, args) in calls {
                state.log(format!("tool {name} {args}"));
                let output = match run_tool(robot, cfg, &name, &args).await {
                    Outcome::Text(t) => {
                        again = true;
                        t
                    }
                    Outcome::Image(t, jpeg) => {
                        again = true;
                        input.push(json!({ "role": "user", "content": [
                            { "type": "input_text", "text": "(your camera)" }, image_part(&jpeg)
                        ]}));
                        t
                    }
                    Outcome::Quiet => "ok".into(),
                    Outcome::Off => return Ok("switched off".into()),
                };
                input.push(
                    json!({ "type": "function_call_output", "call_id": call_id, "output": output }),
                );
            }
            if !again {
                // Record the outputs without asking for more.
                break;
            }
        }
    }
}

fn image_part(jpeg: &[u8]) -> Value {
    json!({ "type": "input_image", "image_url": super::picture_url(jpeg) })
}

/// Stream one response, speaking sentences as they complete. Returns the response id and its
/// function calls.
async fn respond(
    http: &reqwest::Client,
    key: &str,
    body: Value,
    robot: &Robot,
    state: &AgentState,
) -> Result<(Option<String>, Vec<(String, String, Value)>)> {
    let resp = http
        .post(format!("{API}/responses"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("calling the text model")?;
    if !resp.status().is_success() {
        let v: Value = resp.json().await.unwrap_or_default();
        bail!(
            "text model: {}",
            v["error"]["message"].as_str().unwrap_or("request failed")
        );
    }
    let mut speech = super::Speech::start(robot);
    let mut id = None;
    let mut calls = Vec::new();
    let mut buf = Vec::new();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<Value>(data) else {
                continue;
            };
            match v["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => {
                    speech.push(v["delta"].as_str().unwrap_or_default());
                }
                "response.completed" | "response.incomplete" => {
                    let r = &v["response"];
                    id = r["id"].as_str().map(String::from);
                    state.add_usage(&r["usage"]);
                    for item in r["output"].as_array().into_iter().flatten() {
                        if item["type"] == "function_call" {
                            calls.push((
                                item["call_id"].as_str().unwrap_or_default().to_string(),
                                item["name"].as_str().unwrap_or_default().to_string(),
                                serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                                    .unwrap_or_default(),
                            ));
                        }
                    }
                }
                "error" | "response.failed" => {
                    let msg = v["error"]["message"]
                        .as_str()
                        .or(v["response"]["error"]["message"].as_str())
                        .unwrap_or("failed")
                        .to_string();
                    state.log(format!("text model error: {msg}"));
                }
                _ => {}
            }
        }
    }
    let said = speech.finish().await?;
    if !said.trim().is_empty() {
        state.log(format!("< {}", said.trim()));
        robot.note_said(said.trim()).await;
    }
    Ok((id, calls))
}
