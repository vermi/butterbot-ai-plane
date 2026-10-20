//! Run `bbplane serve` in the background from login on: a launchd agent on macOS, a systemd user
//! service on Linux. It restarts if it crashes, so the robot has its plane whenever it is put in
//! AI mode.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const LABEL: &str = "bbplane";

/// What to run: this executable with `serve`, and the config file if one was given.
fn program(config: Option<&Path>) -> Result<Vec<String>> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("finding this program")?;
    if exe.components().any(|c| c.as_os_str() == "debug") {
        eprintln!("note: this is a debug build; `cargo build --release` runs much faster");
    }
    let mut args = vec![exe.display().to_string()];
    if let Some(c) = config {
        let c = c
            .canonicalize()
            .with_context(|| format!("finding {}", c.display()))?;
        args.extend(["--config".into(), c.display().to_string()]);
    }
    args.push("serve".into());
    Ok(args)
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        // SAFETY: getuid has no preconditions.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    pub fn install(args: &[String], log: &Path) -> Result<String> {
        let path = plist_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, super::plist(args, log))
            .with_context(|| format!("writing {}", path.display()))?;
        // Replace a running copy (an older build, other settings). bootout returns before it has
        // stopped (it shuts down cleanly, agent and all), and bootstrap fails until then.
        let service = format!("{}/{LABEL}", domain());
        if run(Command::new("launchctl").args(["bootout", &service])).is_ok() {
            for _ in 0..60 {
                if run(Command::new("launchctl").args(["print", &service])).is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        run(Command::new("launchctl")
            .args(["bootstrap", &domain()])
            .arg(&path))?;
        Ok(format!(
            "installed {} and started it; logs go to {}",
            path.display(),
            log.display()
        ))
    }

    pub fn uninstall() -> Result<String> {
        let path = plist_path()?;
        let _ = run(Command::new("launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]));
        if path.exists() {
            std::fs::remove_file(&path)?;
            Ok(format!("stopped and removed {}", path.display()))
        } else {
            Ok("not installed".into())
        }
    }

    pub fn status() -> Result<String> {
        if !plist_path()?.exists() {
            return Ok("not installed".into());
        }
        let Ok(out) =
            run(Command::new("launchctl").args(["print", &format!("{}/{LABEL}", domain())]))
        else {
            return Ok("installed, not loaded (it starts at the next login)".into());
        };
        let field = |name: &str| {
            out.lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(" = ")))
                .unwrap_or("?")
                .to_string()
        };
        Ok(format!(
            "installed, {} (pid {})",
            field("state"),
            field("pid")
        ))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .map_or_else(|| home().map(|h| h.join(".config")), Ok)?;
        Ok(base.join("systemd/user").join(format!("{LABEL}.service")))
    }

    pub fn install(args: &[String], _log: &Path) -> Result<String> {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, super::unit(args))
            .with_context(|| format!("writing {}", path.display()))?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", LABEL]))?;
        run(Command::new("systemctl").args(["--user", "restart", LABEL]))?;
        Ok(format!(
            "installed {} and started it; logs: journalctl --user -u {LABEL}",
            path.display()
        ))
    }

    pub fn uninstall() -> Result<String> {
        let path = unit_path()?;
        let _ = run(Command::new("systemctl").args(["--user", "disable", "--now", LABEL]));
        if path.exists() {
            std::fs::remove_file(&path)?;
            let _ = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
            Ok(format!("stopped and removed {}", path.display()))
        } else {
            Ok("not installed".into())
        }
    }

    pub fn status() -> Result<String> {
        if !unit_path()?.exists() {
            return Ok("not installed".into());
        }
        let state = Command::new("systemctl")
            .args(["--user", "is-active", LABEL])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        Ok(format!("installed, {state}"))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;

    pub fn install(_args: &[String], _log: &Path) -> Result<String> {
        bail!("starting at login is set up for macOS and Linux only")
    }
    pub fn uninstall() -> Result<String> {
        bail!("starting at login is set up for macOS and Linux only")
    }
    pub fn status() -> Result<String> {
        Ok("not supported on this system".into())
    }
}

/// Start `bbplane serve` now and at every login (replacing an installed one).
pub fn install(config: Option<&Path>, data_dir: &Path) -> Result<String> {
    std::fs::create_dir_all(data_dir)?;
    platform::install(&program(config)?, &data_dir.join("bbplane.log"))
}

pub fn uninstall() -> Result<String> {
    platform::uninstall()
}

/// The service's log only grows (launchd appends to it): start it over once it is large.
pub fn trim_log(data_dir: &Path) {
    const MAX: u64 = 20 << 20;
    let log = data_dir.join("bbplane.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > MAX)
        && let Ok(f) = std::fs::OpenOptions::new().write(true).open(&log)
    {
        let _ = f.set_len(0);
    }
}

pub fn status() -> Result<String> {
    platform::status()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd agent. `Interactive` keeps macOS from throttling it in the background (App Nap),
/// which would make the robot's audio stutter.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist(args: &[String], log: &Path) -> String {
    let args: String = args
        .iter()
        .map(|a| format!("\n    <string>{}</string>", xml(a)))
        .collect();
    let log = xml(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// The systemd user unit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unit(args: &[String]) -> String {
    let exec: Vec<String> = args
        .iter()
        .map(|a| format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "[Unit]\nDescription=ButterBot AI control plane\nAfter=network-online.target\n\n\
[Service]\nExecStart={}\nRestart=on-failure\nRestartSec=10\n\n\
[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_paths() {
        let p = plist(
            &["/Users/a & b/bbplane".into(), "serve".into()],
            Path::new("/tmp/log<1>"),
        );
        assert!(p.contains("<string>/Users/a &amp; b/bbplane</string>"));
        assert!(p.contains("<string>serve</string>"));
        assert!(p.contains("/tmp/log&lt;1&gt;"));
        assert!(p.contains("<string>Interactive</string>"));
    }

    #[test]
    fn unit_quotes_arguments() {
        let u = unit(&["/opt/my bbplane".into(), "serve".into()]);
        assert!(u.contains("ExecStart=\"/opt/my bbplane\" \"serve\"\n"));
    }
}
