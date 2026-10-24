//! ButterBot AI control plane: robot link, live view and MCP server.

pub mod agent;
pub mod appletv;
pub mod audio;
pub mod brain;
pub mod config;
pub mod control;
pub mod diary;
pub mod ears;
pub mod face;
pub mod geometry;
pub mod gestures;
pub mod home;
pub mod hub;
pub mod intentions;
pub mod life;
pub mod light;
pub mod link;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod mood;
pub mod objects;
pub mod ota;
pub mod peek;
pub mod provision;
pub mod recall;
pub mod reminders;
pub mod service;
pub mod sim;
pub mod sounds;
pub mod stt;
pub mod summon;
pub mod tts;
pub mod voices;
pub mod web;

use std::sync::Arc;

use anyhow::{Context, Result};

/// Running plane: robot link listener and web/MCP-over-HTTP server.
pub struct Plane {
    pub robot: control::Robot,
    pub link_addr: std::net::SocketAddr,
    pub web_addr: std::net::SocketAddr,
}

/// Bind both listeners and spawn their tasks.
pub async fn start(config: config::Config) -> Result<Plane> {
    // Both rustls backends end up compiled in (through different dependencies), and rustls then
    // refuses to pick one: choose ring for the whole process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let link = tokio::net::UdpSocket::bind((config.link_bind.as_str(), config.link_port))
        .await
        .with_context(|| {
            format!(
                "binding robot link udp port {} (is another bbplane running?)",
                config.link_port
            )
        })?;
    let web = tokio::net::TcpListener::bind((config.web_bind.as_str(), config.web_port))
        .await
        .with_context(|| format!("binding web port {}", config.web_port))?;
    let link_addr = link.local_addr()?;
    let web_addr = web.local_addr()?;
    let hub = hub::Hub::new(config);
    let robot = control::Robot::new(hub.clone());
    tokio::spawn({
        let hub: Arc<hub::Hub> = hub.clone();
        async move {
            if let Err(e) = link::serve(hub, link).await {
                tracing::error!("robot link stopped: {e}");
            }
        }
    });
    robot.load_activity();
    tokio::spawn(face::run(robot.clone()));
    tokio::spawn(objects::run(robot.clone()));
    tokio::spawn(sounds::run(robot.clone()));
    tokio::spawn(ears::run(robot.clone()));
    tokio::spawn(agent::run(robot.clone()));
    tokio::spawn(control::on_link_up(robot.clone()));
    tokio::spawn(control::on_body(robot.clone()));
    tokio::spawn(control::overdue_reminders(robot.clone()));
    tokio::spawn(light::run(robot.clone()));
    tokio::spawn(home::run(robot.clone()));
    tokio::spawn(peek::run(robot.clone()));
    tokio::spawn(diary::run(robot.clone()));
    tokio::spawn(recall::run(robot.clone()));
    tokio::spawn(appletv::run(robot.clone()));
    tokio::spawn({
        let robot = robot.clone();
        async move {
            if let Err(e) = web::serve(robot, web).await {
                tracing::error!("web server stopped: {e}");
            }
        }
    });
    Ok(Plane {
        robot,
        link_addr,
        web_addr,
    })
}
