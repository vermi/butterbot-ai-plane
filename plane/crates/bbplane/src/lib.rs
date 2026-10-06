//! ButterBot AI control plane: robot link, live view and MCP server.

pub mod audio;
pub mod config;
pub mod control;
pub mod hub;
pub mod link;
pub mod mcp;
pub mod provision;
pub mod sim;
pub mod tts;
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
