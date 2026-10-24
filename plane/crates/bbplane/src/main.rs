use std::path::PathBuf;

use anyhow::{Context, Result};
use bbplane::{config::Config, mcp::BotServer, provision};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(name = "bbplane", version, about = "ButterBot AI control plane")]
struct Cli {
    /// Config file (default: platform config dir, bbplane/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the plane: robot link, live view, REST API and MCP over HTTP at /mcp.
    Serve,
    /// Run the plane and serve MCP over stdio (for MCP clients that launch a command).
    Mcp,
    /// Send Wi-Fi credentials and the plane address to the robot over Bluetooth.
    Provision {
        #[arg(long)]
        ssid: String,
        #[arg(long, default_value = "")]
        password: String,
        /// Address the robot should dial (default: this machine's LAN IP).
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = bbproto::LINK_PORT)]
        port: u16,
        /// Only provision the robot with this BLE address/id.
        #[arg(long)]
        robot: Option<String>,
    },
    /// Wake a provisioned robot over Bluetooth: it joins Wi-Fi, dials the plane and enters AI mode.
    Wake {
        /// Only wake the robot with this BLE address/id.
        #[arg(long)]
        robot: Option<String>,
    },
    /// Install new robot firmware over Wi-Fi (the robot must be in AI mode, with a plane running).
    /// It restarts into it, and keeps it once it has reached the plane again.
    Ota {
        /// The app image, e.g. firmware/build/ButterBot-Firmware.bin.
        image: PathBuf,
    },
    /// Restart the robot (it comes back in AI mode), e.g. when it seems stuck.
    Restart,
    /// Set the free internal RAM (bytes) the robot requires before starting Wi-Fi.
    /// The robot logs the actual numbers on its serial console.
    Tune {
        #[arg(long, default_value_t = 0)]
        min_free: u32,
        #[arg(long, default_value_t = 0)]
        min_block: u32,
        #[arg(long)]
        robot: Option<String>,
    },
    /// Fake robot for testing without hardware.
    Sim {
        #[arg(long, default_value = "127.0.0.1:7531")]
        plane: String,
    },
    /// Print the effective configuration and its path.
    Config,
    /// Run `serve` in the background from login on (launchd on macOS, systemd on Linux).
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start it now and at every login (replaces an installed one, e.g. after a rebuild).
    Install,
    /// Stop it and remove it from login.
    Uninstall,
    /// Whether it is installed and running.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // stdout belongs to the MCP protocol in `mcp` mode; always log to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // Plain text in a log file (the login service), colour in a terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,rmcp=warn,whisper_rs=warn".into()),
        )
        .init();

    match cli.cmd {
        // Bluetooth commands run again in a child process on macOS, so a permission abort can be
        // explained (see provision::use_child).
        Cmd::Provision { .. } | Cmd::Wake { .. } | Cmd::Tune { .. } if provision::use_child() => {
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            if let Err(e) = provision::run_child(&args).await {
                // The child printed its own error already; only explain a macOS kill.
                if e.to_string() == provision::BLUETOOTH_BLOCKED {
                    eprintln!("{e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Serve => {
            let config = Config::load(cli.config.as_ref())?;
            bbplane::service::trim_log(&config.data_dir());
            let plane = bbplane::start(config).await?;
            eprintln!(
                "robot link on {}; open http://{}/ ; MCP: http://{}/mcp",
                plane.link_addr, plane.web_addr, plane.web_addr
            );
            shutdown_signal().await?;
            plane.robot.agent.stop().await;
            plane.robot.stop().await.ok();
            // Returning drops the runtime and with it the agent process (kill_on_drop).
        }
        Cmd::Mcp => {
            let plane = bbplane::start(Config::load(cli.config.as_ref())?).await?;
            tracing::info!("live view on http://{}/", plane.web_addr);
            let server = BotServer::new(plane.robot.clone())
                .serve(rmcp::transport::stdio())
                .await?;
            server.waiting().await?;
            plane.robot.agent.stop().await;
            plane.robot.stop().await.ok();
        }
        Cmd::Provision {
            ssid,
            password,
            host,
            port,
            robot,
        } => {
            let host = match host {
                Some(h) => h,
                None => provision::default_host()?,
            };
            provision::run(provision::Provision {
                ssid,
                password,
                host,
                port,
                name_filter: robot,
            })
            .await?;
        }
        Cmd::Wake { robot } => {
            provision::wake(robot.as_deref()).await?;
            eprintln!("wake sent; the robot joins Wi-Fi and dials the plane in a few seconds");
        }
        Cmd::Tune {
            min_free,
            min_block,
            robot,
        } => {
            provision::tune(min_free, min_block, robot.as_deref()).await?;
            eprintln!("thresholds stored on the robot");
        }
        Cmd::Restart => {
            let config = Config::load(cli.config.as_ref())?;
            let r = reqwest::Client::new()
                .post(format!("{}/api/restart", web_base(&config)))
                .send()
                .await
                .context("is the plane running? (bbplane service status)")?;
            if !r.status().is_success() {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                anyhow::bail!(
                    "{}",
                    v["error"].as_str().unwrap_or("the robot did not restart")
                );
            }
            eprintln!("restarting; the robot is back in AI mode in a few seconds");
        }
        Cmd::Ota { image } => {
            let config = Config::load(cli.config.as_ref())?;
            ota_cli(&config, &image).await?;
        }
        Cmd::Sim { plane } => loop {
            match bbplane::sim::run(&plane).await {
                Ok(()) => eprintln!("plane closed the connection; reconnecting"),
                Err(e) => eprintln!("sim: {e}; retrying"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        },
        Cmd::Service { action } => {
            let msg = match action {
                ServiceCmd::Install => {
                    let data = Config::load(cli.config.as_ref())?.data_dir();
                    bbplane::service::install(cli.config.as_deref(), &data)?
                }
                ServiceCmd::Uninstall => bbplane::service::uninstall()?,
                ServiceCmd::Status => bbplane::service::status()?,
            };
            eprintln!("{msg}");
        }
        Cmd::Config => {
            let c = Config::load(cli.config.as_ref())?;
            eprintln!(
                "# {}",
                cli.config
                    .or_else(Config::default_path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            print!("{}", toml::to_string_pretty(&c)?);
        }
    }
    Ok(())
}

/// The running plane's live view and API, as reached from this machine.
fn web_base(config: &Config) -> String {
    let host = match config.web_bind.as_str() {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{}", config.web_port)
}

/// Hand the image to the running plane and follow the update.
async fn ota_cli(config: &Config, image: &std::path::Path) -> Result<()> {
    let bytes = std::fs::read(image).with_context(|| format!("reading {}", image.display()))?;
    // Checked here too, so a wrong file is reported before anything is sent.
    let img = bbplane::ota::Image::parse(bytes.clone().into())?;
    eprintln!(
        "{} ({} KB, build {})",
        img.version,
        bytes.len() / 1024,
        img.build
    );
    let base = format!("{}/api/ota", web_base(config));
    let http = reqwest::Client::new();
    let r = http
        .post(&base)
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .context("is the plane running? (bbplane service status)")?;
    if !r.status().is_success() {
        let v: serde_json::Value = r.json().await.unwrap_or_default();
        anyhow::bail!(
            "{}",
            v["error"]
                .as_str()
                .unwrap_or("the plane refused the update")
        );
    }
    let mut shown = String::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let v: serde_json::Value = http.get(&base).send().await?.json().await?;
        let state = v["state"].as_str().unwrap_or_default();
        let line = match state {
            "downloading" => {
                let (done, total) = (
                    v["done"].as_u64().unwrap_or(0),
                    v["total"].as_u64().unwrap_or(1).max(1),
                );
                format!("downloading {}%", done * 100 / total)
            }
            "done" => {
                eprintln!(
                    "done: the robot runs build {}",
                    v["build"].as_str().unwrap_or_default()
                );
                return Ok(());
            }
            "failed" => anyhow::bail!("{}", v["error"].as_str().unwrap_or("failed")),
            s => s.to_string(),
        };
        if line != shown {
            eprintln!("{line}");
            shown = line;
        }
    }
}

/// Ctrl-C, or SIGTERM (`kill`, launchd): either way shut down cleanly, so the agent stops too.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
