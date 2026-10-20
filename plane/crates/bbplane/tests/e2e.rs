//! End to end: plane + simulated robot, driven through MCP JSON-RPC and the HTTP live view.

use std::time::Duration;

use bbplane::config::Config;
use bbplane::mcp::BotServer;
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct McpClient {
    w: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    r: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    id: u64,
}

impl McpClient {
    async fn send(&mut self, v: Value) {
        let mut s = serde_json::to_string(&v).unwrap();
        s.push('\n');
        self.w.write_all(s.as_bytes()).await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(20), self.r.read_line(&mut line))
                .await
                .expect("mcp response timeout")
                .unwrap();
            assert!(n > 0, "mcp server closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{method} failed: {v}");
                return v["result"].clone();
            }
        }
    }

    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self
            .call("tools/call", json!({"name": name, "arguments": args}))
            .await;
        assert_ne!(r["isError"], true, "tool {name} errored: {r}");
        r
    }
}

fn text_of(r: &Value) -> String {
    r["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn http_get(addr: std::net::SocketAddr, path: &str, max: usize) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while out.len() < max {
        match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => panic!("{e}"),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn plane_with_simulated_robot() {
    let data = std::env::temp_dir().join(format!("bbplane-e2e-{}", std::process::id()));
    let config = Config {
        link_bind: "127.0.0.1".into(),
        link_port: 0,
        web_port: 0,
        data_dir: Some(data.clone()),
        // No speech model downloads in tests.
        stt: bbplane::config::SttConfig {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let plane = bbplane::start(config).await.unwrap();
    let link = plane.link_addr.to_string();
    tokio::spawn(async move { bbplane::sim::run(&link).await });

    for _ in 0..50 {
        if plane.robot.hub.status.lock().await.fw.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        plane.robot.hub.status.lock().await.fw.as_deref(),
        Some("bbsim")
    );

    // MCP over an in-memory pipe, speaking raw JSON-RPC like a real client.
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let server = BotServer::new(plane.robot.clone());
    tokio::spawn(async move {
        let s = server.serve(server_io).await.unwrap();
        s.waiting().await.unwrap();
    });
    let (r, w) = tokio::io::split(client_io);
    let mut c = McpClient {
        w,
        r: BufReader::new(r),
        id: 0,
    };

    let init = c
        .call(
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}),
        )
        .await;
    assert!(init["instructions"].as_str().unwrap().contains("ButterBot"));
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await;

    let tools = c.call("tools/list", json!({})).await;
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "status",
        "move",
        "turn",
        "drive",
        "stop",
        "look",
        "snapshot",
        "listen",
        "say",
        "events",
        "run_scenario",
    ] {
        assert!(names.contains(&want), "missing tool {want} in {names:?}");
    }

    // A control tool enters AI mode automatically and starts the streams.
    let r = c
        .tool(
            "move",
            json!({"direction":"forward","speed":50,"duration_ms":700}),
        )
        .await;
    let out: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert!(out["stopped_early"].is_null(), "{out}");
    let ms = out["completed_ms"].as_u64().unwrap();
    assert!((650..1200).contains(&ms), "drove {ms} ms");
    assert_eq!(plane.robot.mode().await.as_deref(), Some("remote"));

    let r = c.tool("look", json!({})).await;
    let img = r["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .expect("image content");
    assert_eq!(img["mimeType"], "image/jpeg");
    assert!(text_of(&r).contains("320x240"), "{}", text_of(&r));

    let r = c.tool("snapshot", json!({"resolution":"qxga"})).await;
    assert!(text_of(&r).contains("2048x1536"), "{}", text_of(&r));

    let r = c.tool("listen", json!({"seconds":1.0})).await;
    let t = text_of(&r);
    assert!(t.contains("\"likely_sound\": true"), "{t}");
    assert!(
        r["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "audio")
    );

    let r = c.tool("say", json!({"text":"hello there"})).await;
    assert!(text_of(&r).contains("robot"));

    let r = c.tool("turn", json!({"degrees":-45,"speed":100})).await;
    let out: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert!(out["completed_ms"].as_u64().unwrap() >= 950, "{out}");

    let r = c.tool("run_scenario", json!({"name":"joke"})).await;
    assert!(text_of(&r).contains("\"done\": true"), "{}", text_of(&r));
    let r = c
        .call(
            "tools/call",
            json!({"name":"run_scenario","arguments":{"name":"nope"}}),
        )
        .await;
    assert_eq!(r["isError"], true);
    let r = c
        .call(
            "tools/call",
            json!({"name":"ir_send","arguments":{"index":0}}),
        )
        .await;
    assert_eq!(r["isError"], true);
    assert!(text_of(&r).contains("IR module"));

    let r = c.tool("events", json!({"since":0})).await;
    let ev: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let kinds: Vec<&str> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for want in ["link", "hello", "mode", "battery", "speaker", "bb"] {
        assert!(kinds.contains(&want), "no {want} event in {kinds:?}");
    }

    let res = c
        .call("resources/read", json!({"uri":"butterbot://camera/latest"}))
        .await;
    assert_eq!(res["contents"][0]["mimeType"], "image/jpeg");

    // Live view: the MJPEG stream yields JPEG parts.
    let body = http_get(plane.web_addr, "/video.mjpeg", 20_000).await;
    let s = String::from_utf8_lossy(&body);
    assert!(
        s.contains("multipart/x-mixed-replace"),
        "{}",
        &s[..s.len().min(300)]
    );
    assert!(
        body.windows(2).any(|w| w == [0xff, 0xd8]),
        "no JPEG in stream"
    );
    let status = http_get(plane.web_addr, "/api/status", 100_000).await;
    assert!(String::from_utf8_lossy(&status).contains("\"connected\":true"));

    c.tool("exit_ai_mode", json!({})).await;
    assert_eq!(plane.robot.mode().await.as_deref(), Some("idle"));
    let _ = std::fs::remove_dir_all(data);
}

#[tokio::test(flavor = "multi_thread")]
async fn works_over_a_lossy_link() {
    // Tools must keep working (just slower) on a very bad link. The firmware once lost ~35% of
    // datagrams by dropping them itself when its two Wi-Fi TX buffers were full; a real link now
    // loses well under 1%. At 20% loss only small frames (few datagrams) get through whole.
    let config = Config {
        link_bind: "127.0.0.1".into(),
        link_port: 0,
        web_port: 0,
        ble_wake: false,
        announce_connect: false,
        camera: bbplane::config::CameraConfig {
            res: bbproto::ctrl::Res::R128,
            ..Default::default()
        },
        data_dir: Some(std::env::temp_dir().join(format!("bbplane-lossy-{}", std::process::id()))),
        // No speech model downloads in tests.
        stt: bbplane::config::SttConfig {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let plane = bbplane::start(config).await.unwrap();
    let link = plane.link_addr.to_string();
    tokio::spawn(async move { bbplane::sim::run_lossy(&link, 0.2).await });
    for _ in 0..100 {
        if plane.robot.hub.is_connected().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = &plane.robot;
    r.enter_remote()
        .await
        .expect("enter remote over lossy link");
    let f = r
        .snapshot(Some(bbproto::ctrl::Res::R128), 12)
        .await
        .expect("snapshot");
    assert_eq!((f.width, f.height), (128, 128));
    r.say("hello", Some("robot"), false).await.expect("say");
    let out = r.drive_for(40, 40, 600).await.expect("drive");
    assert!(out.completed_ms >= 500, "{out:?}");
    // Live frames keep arriving despite loss (incomplete ones are dropped).
    let before = r.hub.status.lock().await.video_frames;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(r.hub.status.lock().await.video_frames > before);
    r.exit_remote().await.expect("exit");
}
