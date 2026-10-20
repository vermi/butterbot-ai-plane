//! Runs the agent that lives in the robot ([agent] in the config): started by `bbplane serve`
//! whenever the robot is in AI mode, restarted when it exits (each start is a fresh session with
//! a recap of the recent conversation), and stopped when the robot leaves AI mode or the agent
//! is paused from the live view.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::Instant;

use crate::control::Robot;

const PERSONA: &str = include_str!("persona.md");
/// Agent output lines kept for the live view.
const LOG_LINES: usize = 300;
/// Lines of recent conversation given to a new session.
const RECAP_LINES: usize = 16;
/// The robot must have been out of AI mode this long before the agent is stopped (it drops out
/// briefly when Wi-Fi hiccups).
const OFFLINE_GRACE: Duration = Duration::from_secs(20);

/// What the live view shows and controls.
#[derive(Default)]
pub struct AgentState {
    pub paused: AtomicBool,
    /// Process group of the running agent (0: none). `codex` is a wrapper that starts the real
    /// binary as a child, so the whole group is signalled.
    group: AtomicI32,
    running: AtomicBool,
    starts: AtomicU32,
    last_exit: Mutex<Option<String>>,
    log: Mutex<VecDeque<String>>,
    /// Token counts reported by the built-in brains, summed per kind (input_audio, ...).
    usage: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl AgentState {
    /// Stop the running agent, if any (on shutdown, pause, or when the robot leaves AI mode).
    pub async fn stop(&self) {
        let pid = self.group.swap(0, Ordering::Relaxed);
        if pid <= 0 {
            return;
        }
        #[cfg(unix)]
        {
            // SAFETY: plain syscalls on a process group this process created.
            unsafe { libc::killpg(pid, libc::SIGTERM) };
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if unsafe { libc::killpg(pid, 0) } != 0 {
                    return;
                }
            }
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }

    /// Add a Realtime/Responses API `usage` object to the totals.
    pub fn add_usage(&self, usage: &Value) {
        let mut u = self.usage.lock().unwrap();
        let mut add = |k: &str, v: &Value| {
            if let Some(n) = v.as_u64() {
                *u.entry(k.to_string()).or_default() += n;
            }
        };
        add("input", &usage["input_tokens"]);
        add("output", &usage["output_tokens"]);
        for side in ["input", "output"] {
            for kind in ["text", "audio", "image", "cached"] {
                let key = if kind == "cached" {
                    "cached_tokens".to_string()
                } else {
                    format!("{kind}_tokens")
                };
                add(
                    &format!("{side}_{kind}"),
                    &usage[format!("{side}_token_details")][&key],
                );
            }
        }
    }

    pub fn log(&self, line: String) {
        let mut l = self.log.lock().unwrap();
        l.push_back(line);
        while l.len() > LOG_LINES {
            l.pop_front();
        }
    }

    pub fn status_json(&self, enabled: bool) -> Value {
        json!({
            "enabled": enabled,
            "running": self.running.load(Ordering::Relaxed),
            "paused": self.paused.load(Ordering::Relaxed),
            "sessions": self.starts.load(Ordering::Relaxed),
            "last_exit": self.last_exit.lock().unwrap().clone(),
            "usage": *self.usage.lock().unwrap(),
            "log": self.log.lock().unwrap().iter().rev().take(80).rev().cloned().collect::<Vec<_>>(),
        })
    }
}

async fn robot_ready(robot: &Robot) -> bool {
    let s = robot.hub.status.lock().await;
    s.connected && s.mode.as_deref() == Some("remote")
}

/// The MCP agent's prompt: the persona with context (see `with_context`).
async fn prompt(robot: &Robot) -> String {
    let persona = robot
        .hub
        .config
        .agent
        .persona
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| PERSONA.to_string());
    with_context(robot, &persona).await
}

/// `persona` with the owner filled in (`{owner}`), the faces it knows and a recap of recent
/// conversation.
pub async fn with_context(robot: &Robot, persona: &str) -> String {
    let cfg = &robot.hub.config;
    let owner = &cfg.life.owner;
    let mut owner_line = if owner.is_empty() {
        String::new()
    } else {
        format!(
            "- {owner} is your person. When they speak to you (`from_owner`) or are in view \
(`owner_in_view`), they come first: drop what you are doing and attend to them."
        )
    };
    let interests = &cfg.life.interests;
    if !interests.is_empty() {
        if !owner_line.is_empty() {
            owner_line.push('\n');
        }
        let who = if owner.is_empty() {
            "People here"
        } else {
            owner.as_str()
        };
        owner_line.push_str(&format!(
            "- {who} enjoy hearing about recent developments in: {}. When you start a chat, bring \
up something new in these (look it up first) rather than jokes.",
            interests.join(", ")
        ));
    }
    owner_line.push_str(&format!(
        "{}- Your activity level is {}: change it with `set_activity` when asked to be livelier, \
calmer or quieter.",
        if owner_line.is_empty() { "" } else { "\n" },
        robot.life.activity_level().name()
    ));
    let pets = &cfg.life.pets;
    if !pets.is_empty() {
        if !owner_line.is_empty() {
            owner_line.push('\n');
        }
        owner_line.push_str(&format!(
            "- Pets that live here: {}. An animal you see is most likely one of them: greet them \
by name.",
            pets.join("; ")
        ));
    }
    let mut p = persona.replace("{owner}", &owner_line);
    let known = robot.faces.known();
    if !known.is_empty() {
        p.push_str(&format!(
            "\nPeople you know by face: {}.\n",
            known.join(", ")
        ));
    }
    let facts = robot.memory.for_instructions();
    if !facts.is_empty() {
        p.push_str(&format!(
            "\nThings you remember (drop one that is no longer true with `forget` and its id):\n{facts}"
        ));
    }
    let recap: Vec<String> = robot
        .hub
        .events_since(0, Duration::ZERO)
        .await
        .into_iter()
        .filter_map(|e| match e.kind.as_str() {
            "heard" => Some(format!("Person: {}", e.data["text"].as_str().unwrap_or(""))),
            "said" => Some(format!("You: {}", e.data["text"].as_str().unwrap_or(""))),
            _ => None,
        })
        .collect();
    if !recap.is_empty() {
        p.push_str("\nRecent conversation (from before this session):\n");
        for line in &recap[recap.len().saturating_sub(RECAP_LINES)..] {
            p.push_str(line);
            p.push('\n');
        }
    }
    p
}

/// Supervise the agent. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let cfg = robot.hub.config.agent.clone();
    if !cfg.enabled {
        return;
    }
    if cfg.kind != crate::config::AgentKind::Command {
        return run_brain(robot, cfg.kind).await;
    }
    if cfg.command.is_empty() {
        return;
    }
    let state = robot.agent.clone();
    let mut backoff = Duration::from_secs(5);
    loop {
        if state.paused.load(Ordering::Relaxed) || !robot_ready(&robot).await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let prompt = prompt(&robot).await;
        let args: Vec<String> = cfg
            .command
            .iter()
            .map(|a| a.replace("{prompt}", &prompt))
            .collect();
        let mut child = match tokio::process::Command::new(&args[0])
            .args(&args[1..])
            // Our own log settings are not for the agent (Codex, for one, would log every request).
            .env_remove("RUST_LOG")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not start {}: {e}", args[0]);
                tracing::error!("agent: {msg}");
                *state.last_exit.lock().unwrap() = Some(msg);
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let session = state.starts.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!("agent session {session} started ({})", args[0]);
        state.log(format!("--- session {session} ---"));
        state.running.store(true, Ordering::Relaxed);
        if let Some(pid) = child.id() {
            state.group.store(pid as i32, Ordering::Relaxed);
        }
        robot.life.agent_running.store(true, Ordering::Relaxed);
        robot.life.session_started();
        let started = Instant::now();

        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let state = state.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "bbplane::agent::output", "{line}");
                    state.log(line);
                }
            });
        }

        // Run until it exits, is paused, or the robot has left AI mode for a while.
        let mut offline_since: Option<Instant> = None;
        let exit = loop {
            tokio::select! {
                status = child.wait() => break status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()),
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            if state.paused.load(Ordering::Relaxed) {
                state.stop().await;
                let _ = child.wait().await;
                break "paused from the live view".into();
            }
            if robot_ready(&robot).await {
                offline_since = None;
            } else if offline_since.get_or_insert_with(Instant::now).elapsed() > OFFLINE_GRACE {
                state.stop().await;
                let _ = child.wait().await;
                break "robot left AI mode".into();
            }
        };
        // It may have exited on its own, leaving children behind.
        state.stop().await;
        tracing::info!("agent session {session} ended: {exit}");
        state.log(format!("--- session {session} ended: {exit} ---"));
        *state.last_exit.lock().unwrap() = Some(exit);
        state.running.store(false, Ordering::Relaxed);
        robot.life.agent_running.store(false, Ordering::Relaxed);

        // A session that dies right away (bad command, no network) must not restart in a tight
        // loop and burn tokens; a normal end (fresh session) restarts at once.
        if started.elapsed() < Duration::from_secs(60) {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(300));
        } else {
            backoff = Duration::from_secs(5);
        }
    }
}

/// Supervise a built-in brain (see `crate::brain`): same rules as for an agent process.
async fn run_brain(robot: Robot, kind: crate::config::AgentKind) {
    let state = robot.agent.clone();
    let mut backoff = Duration::from_secs(5);
    loop {
        if state.paused.load(Ordering::Relaxed) || !robot_ready(&robot).await {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let session = state.starts.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!("brain session {session} started ({kind:?})");
        state.log(format!("--- session {session} ---"));
        state.running.store(true, Ordering::Relaxed);
        robot.life.agent_running.store(true, Ordering::Relaxed);
        robot.life.session_started();
        let started = Instant::now();
        // In its own task, so a panic ends this session, not the supervisor.
        let mut run = {
            let (robot, state) = (robot.clone(), state.clone());
            tokio::spawn(async move {
                match kind {
                    crate::config::AgentKind::Text => {
                        crate::brain::text::session(&robot, &state).await
                    }
                    _ => crate::brain::realtime::session(&robot, &state).await,
                }
            })
        };
        let exit = tokio::select! {
            r = &mut run => match r {
                Ok(Ok(why)) => why,
                Ok(Err(e)) => format!("error: {e:#}"),
                Err(e) => format!("crashed: {e}"),
            },
            why = until_stopped(&robot, &state) => {
                run.abort();
                why
            }
        };
        tracing::info!("brain session {session} ended: {exit}");
        state.log(format!("--- session {session} ended: {exit} ---"));
        *state.last_exit.lock().unwrap() = Some(exit);
        state.running.store(false, Ordering::Relaxed);
        robot.life.agent_running.store(false, Ordering::Relaxed);
        if started.elapsed() < Duration::from_secs(60) {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(300));
        } else {
            backoff = Duration::from_secs(5);
        }
    }
}

/// Resolves when the agent should stop: paused, or the robot out of AI mode for a while.
async fn until_stopped(robot: &Robot, state: &AgentState) -> String {
    let mut offline_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if state.paused.load(Ordering::Relaxed) {
            return "paused from the live view".into();
        }
        if robot_ready(robot).await {
            offline_since = None;
        } else if offline_since.get_or_insert_with(Instant::now).elapsed() > OFFLINE_GRACE {
            return "robot left AI mode".into();
        }
    }
}
