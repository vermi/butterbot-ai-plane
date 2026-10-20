use std::path::PathBuf;

use anyhow::Result;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // stdout belongs to the MCP protocol in `mcp` mode; always log to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
            let plane = bbplane::start(Config::load(cli.config.as_ref())?).await?;
            eprintln!(
                "robot link on {}; open http://{}/ ; MCP: http://{}/mcp",
                plane.link_addr, plane.web_addr, plane.web_addr
            );
            tokio::signal::ctrl_c().await?;
            plane.robot.stop().await.ok();
        }
        Cmd::Mcp => {
            let plane = bbplane::start(Config::load(cli.config.as_ref())?).await?;
            tracing::info!("live view on http://{}/", plane.web_addr);
            let server = BotServer::new(plane.robot.clone())
                .serve(rmcp::transport::stdio())
                .await?;
            server.waiting().await?;
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
        Cmd::Sim { plane } => loop {
            match bbplane::sim::run(&plane).await {
                Ok(()) => eprintln!("plane closed the connection; reconnecting"),
                Err(e) => eprintln!("sim: {e}; retrying"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        },
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
