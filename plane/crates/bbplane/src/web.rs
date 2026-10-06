//! Live view for humans (MJPEG video, mic audio, push-to-talk, drive pad) plus a small
//! REST API and the HTTP MCP endpoint at `/mcp`.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bbproto::{Frame, FrameType, ToRobot};
use futures::{SinkExt, StreamExt};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::control::Robot;
use crate::mcp::BotServer;

const INDEX: &str = include_str!("web/index.html");
const BOUNDARY: &str = "bbframe";

pub fn router(robot: Robot) -> Router {
    let mcp_robot = robot.clone();
    let mcp: StreamableHttpService<BotServer, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(BotServer::new(mcp_robot.clone())),
        Default::default(),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/video.mjpeg", get(mjpeg))
        .route("/frame.jpg", get(frame))
        .route("/ws/audio", get(audio_ws))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .route("/api/mode", post(mode))
        .route("/api/drive", post(drive))
        .route("/api/stop", post(stop))
        .route("/api/say", post(say))
        .route("/api/camera", post(camera))
        .nest_service("/mcp", mcp)
        .with_state(robot)
}

fn err(e: anyhow::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

async fn status(State(r): State<Robot>) -> Json<Value> {
    Json(r.status_json().await)
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<u64>,
    wait_ms: Option<u64>,
}

async fn events(State(r): State<Robot>, Query(q): Query<EventsQuery>) -> Json<Value> {
    let evs = r
        .hub
        .events_since(
            q.since.unwrap_or(0),
            Duration::from_millis(q.wait_ms.unwrap_or(0).min(25_000)),
        )
        .await;
    Json(json!({ "events": evs }))
}

#[derive(Deserialize)]
struct ModeBody {
    remote: bool,
}

async fn mode(State(r): State<Robot>, Json(b): Json<ModeBody>) -> Response {
    let res = if b.remote {
        r.enter_remote().await
    } else {
        r.exit_remote().await
    };
    match res {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct DriveBody {
    l: i8,
    r: i8,
    /// The pad resends every ~200 ms while held, so a short watchdog is enough.
    ms: Option<u16>,
}

async fn drive(State(r): State<Robot>, Json(b): Json<DriveBody>) -> Response {
    if let Err(e) = r.ensure_remote().await {
        return err(e);
    }
    let msg = ToRobot::Drive {
        l: b.l.clamp(-100, 100),
        r: b.r.clamp(-100, 100),
        ms: b.ms.unwrap_or(1000).min(1500),
        // Direct control from the live view: no safety checks, like the handheld RC controller.
        safe: false,
    };
    match r.hub.send(msg).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

async fn stop(State(r): State<Robot>) -> Response {
    match r.stop().await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct SayBody {
    text: String,
    engine: Option<String>,
}

async fn say(State(r): State<Robot>, Json(b): Json<SayBody>) -> Response {
    match r.say(&b.text, b.engine.as_deref(), false).await {
        Ok(engine) => Json(json!({ "ok": true, "engine": engine })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct CameraBody {
    on: bool,
    res: Option<bbproto::ctrl::Res>,
    fps: Option<u8>,
    quality: Option<u8>,
    #[serde(flatten)]
    opts: bbproto::ctrl::CamOpts,
}

async fn camera(State(r): State<Robot>, Json(b): Json<CameraBody>) -> Response {
    if let Err(e) = r.ensure_remote().await {
        return err(e);
    }
    let c = &r.hub.config.camera;
    match r
        .set_camera(
            b.on,
            b.res.unwrap_or(c.res),
            b.fps.unwrap_or(c.fps),
            b.quality.unwrap_or(c.quality),
            b.opts.or(c.opts),
        )
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

async fn frame(State(r): State<Robot>) -> Response {
    match r.hub.latest_frame().await {
        Some(f) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            f.jpeg.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no frame yet").into_response(),
    }
}

async fn mjpeg(State(r): State<Robot>) -> Response {
    let mut rx = r.hub.video.subscribe();
    let first = r.hub.latest_frame().await;
    let stream = async_stream(move |tx| async move {
        if let Some(f) = first
            && tx.send(part(&f.jpeg)).await.is_err()
        {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(f) => {
                    if tx.send(part(&f.jpeg)).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            format!("multipart/x-mixed-replace; boundary={BOUNDARY}"),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn part(jpeg: &[u8]) -> Result<Bytes, std::io::Error> {
    let mut v = Vec::with_capacity(jpeg.len() + 100);
    v.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        )
        .as_bytes(),
    );
    v.extend_from_slice(jpeg);
    v.extend_from_slice(b"\r\n");
    Ok(Bytes::from(v))
}

/// Minimal channel-backed stream so the MJPEG body can be produced by a task.
fn async_stream<F, Fut>(f: F) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tokio::spawn(f(tx));
    tokio_stream_from(rx)
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// Binary messages to the browser: mic PCM (s16le 16 kHz mono).
/// From the browser: text `{"ptt":true|false}` and binary PCM for the robot speaker.
async fn audio_ws(State(r): State<Robot>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |sock| audio_session(r, sock))
}

async fn audio_session(r: Robot, sock: WebSocket) {
    let (mut tx, mut rx) = sock.split();
    let mut mic = r.hub.mic.subscribe();
    let hub = r.hub.clone();
    let sender = tokio::spawn(async move {
        loop {
            match mic.recv().await {
                Ok(chunk) => {
                    let b = bbproto::frame::pcm_to_le(&chunk);
                    if tx.send(Message::Binary(b)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let mut talking = false;
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => {
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                if let Some(on) = v.get("ptt").and_then(Value::as_bool) {
                    if on && !talking {
                        if r.ensure_remote().await.is_ok() {
                            let id = hub.next_id();
                            talking = hub.request(ToRobot::SpkBegin { id }).await.is_ok();
                        }
                    } else if !on && talking {
                        talking = false;
                        let _ = hub.send(ToRobot::SpkEnd).await;
                    }
                }
            }
            Message::Binary(b) if talking => {
                // One datagram per 512 samples (1 KiB).
                for part in b.chunks(1024) {
                    let _ = hub
                        .send_frame(Frame::new(FrameType::SpkPcm, Bytes::copy_from_slice(part)))
                        .await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    if talking {
        let _ = hub.send(ToRobot::SpkEnd).await;
    }
    sender.abort();
}

pub async fn serve(robot: Robot, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    tracing::info!(
        "live view and MCP on http://{}/ (MCP at /mcp)",
        listener.local_addr()?
    );
    axum::serve(listener, router(robot)).await?;
    Ok(())
}
