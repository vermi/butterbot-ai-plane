//! Laptop-side text-to-speech: runs an external command that writes a WAV file,
//! then the audio is streamed to the robot speaker. The default `robot` engine is the robot's
//! own voice: on the robot (older firmware, `tts` capability) or `robot_voice` here.

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::TtsConfig;

/// Per-platform default command. `{out}` = WAV path to write, `{text}` = the text.
pub fn preset() -> Vec<String> {
    let v: &[&str] = if cfg!(target_os = "macos") {
        &[
            "say",
            "--file-format=WAVE",
            "--data-format=LEI16@16000",
            "-o",
            "{out}",
            "--",
            "{text}",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "powershell",
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
             $s.SetOutputToWaveFile('{out}'); $s.Speak($env:BBPLANE_TTS_TEXT); $s.Dispose()",
        ]
    } else {
        &["espeak-ng", "-w", "{out}", "--", "{text}"]
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// Produce 16 kHz mono PCM for `text`.
pub async fn synthesize(cfg: &TtsConfig, text: &str) -> Result<Vec<i16>> {
    let cmd = if cfg.command.is_empty() {
        preset()
    } else {
        cfg.command.clone()
    };
    let Some((prog, args)) = cmd.split_first() else {
        bail!("empty TTS command")
    };

    let dir = std::env::temp_dir().join("bbplane-tts");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join(format!("{}.wav", uuid::Uuid::new_v4()));
    let out_s = out.to_string_lossy().to_string();

    let args: Vec<String> = args
        .iter()
        .map(|a| {
            let a = a.replace("{out}", &out_s);
            if cfg.text_on_stdin {
                a
            } else {
                a.replace("{text}", text)
            }
        })
        .collect();

    let mut child = Command::new(prog)
        .args(&args)
        // Windows preset reads the text from the environment to avoid quoting issues.
        .env("BBPLANE_TTS_TEXT", text)
        .stdin(if cfg.text_on_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting TTS command '{prog}'"))?;
    if cfg.text_on_stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(text.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        bail!(
            "TTS command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let pcm = crate::audio::read_wav_file_16k(&out);
    let _ = std::fs::remove_file(&out);
    pcm
}

/// The robot's own voice, now made here: flite's `kal16` voice (the stock robot's), through the
/// compressor and light reverb its firmware used to apply. For firmware without a voice of its
/// own (no `tts` capability); 16 kHz mono.
pub async fn robot_voice(text: &str) -> Result<Vec<i16>> {
    let dir = std::env::temp_dir().join("bbplane-tts");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join(format!("{}.wav", uuid::Uuid::new_v4()));
    let output = Command::new(flite())
        .args(["-voice", "kal16", "-t", text, "-o"])
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("starting flite (brew install flite)")?;
    if !output.status.success() {
        bail!(
            "flite failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let pcm = crate::audio::read_wav_file_16k(&out);
    let _ = std::fs::remove_file(&out);
    let mut pcm = pcm?;
    RobotVoiceFx::default().process(&mut pcm);
    Ok(pcm)
}

/// flite from PATH, or Homebrew's (a launchd service does not have it on PATH).
fn flite() -> std::path::PathBuf {
    for p in ["/opt/homebrew/bin/flite", "/usr/local/bin/flite"] {
        if std::path::Path::new(p).exists() {
            return p.into();
        }
    }
    "flite".into()
}

/// The firmware's speech chain: a feed-forward compressor (threshold 0.2, 4:1, makeup 1.5, 5 ms
/// attack, 80 ms release), then a small Schroeder reverb (4 combs, 2 allpasses, 5 % wet).
struct RobotVoiceFx {
    envelope: f32,
    combs: Vec<(Vec<f32>, usize, f32)>,
    allpasses: Vec<(Vec<f32>, usize)>,
}

impl Default for RobotVoiceFx {
    fn default() -> Self {
        Self {
            envelope: 0.0,
            combs: [401, 431, 461, 487]
                .iter()
                .map(|&n| (vec![0.0; n], 0, 0.0))
                .collect(),
            allpasses: [199, 157].iter().map(|&n| (vec![0.0; n], 0)).collect(),
        }
    }
}

impl RobotVoiceFx {
    const RATE: f32 = 16_000.0;
    const THRESHOLD: f32 = 0.2;
    const RATIO: f32 = 4.0;
    const MAKEUP: f32 = 1.5;
    const FEEDBACK: f32 = 0.70 + 0.2 * 0.20;
    const DAMP: f32 = 0.8;
    const WET: f32 = 0.05;

    fn process(&mut self, pcm: &mut [i16]) {
        let attack = (-1.0 / (0.005 * Self::RATE)).exp();
        let release = (-1.0 / (0.080 * Self::RATE)).exp();
        for s in pcm.iter_mut() {
            let x = *s as f32 / 32768.0;
            let mag = x.abs();
            let c = if mag > self.envelope { attack } else { release };
            self.envelope = c * self.envelope + (1.0 - c) * mag;
            let reduction = if self.envelope > Self::THRESHOLD {
                (self.envelope / Self::THRESHOLD).powf(1.0 / Self::RATIO - 1.0)
            } else {
                1.0
            };
            let x = (x * reduction * Self::MAKEUP).clamp(-1.0, 32767.0 / 32768.0);
            let mut sum = 0.0;
            for (buf, pos, store) in self.combs.iter_mut() {
                let out = buf[*pos];
                *store = out * (1.0 - Self::DAMP) + *store * Self::DAMP;
                buf[*pos] = x + *store * Self::FEEDBACK;
                *pos = (*pos + 1) % buf.len();
                sum += out;
            }
            let mut r = sum / self.combs.len() as f32;
            for (buf, pos) in self.allpasses.iter_mut() {
                let b = buf[*pos];
                let out = -r + b;
                buf[*pos] = r + b * 0.5;
                *pos = (*pos + 1) % buf.len();
                r = out;
            }
            let y = x * (1.0 - Self::WET) + r * Self::WET;
            *s = (y * 32768.0).clamp(-32768.0, 32767.0) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn macos_say_preset_produces_audio() {
        let pcm = synthesize(&TtsConfig::default(), "Hello").await.unwrap();
        // "Hello" is well over 200 ms of audio at 16 kHz.
        assert!(pcm.len() > 3200, "only {} samples", pcm.len());
        assert!(pcm.iter().any(|s| s.unsigned_abs() > 1000));
    }

    #[tokio::test]
    async fn robot_voice_speaks_when_flite_is_installed() {
        if !std::path::Path::new(&flite()).exists() && which_flite().is_none() {
            return;
        }
        let pcm = robot_voice("Hello").await.unwrap();
        assert!(pcm.len() > 3200, "only {} samples", pcm.len());
        assert!(pcm.iter().any(|s| s.unsigned_abs() > 1000));
    }

    fn which_flite() -> Option<()> {
        std::process::Command::new("flite")
            .arg("-lv")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ())
    }

    #[tokio::test]
    async fn custom_command_and_failure() {
        let cfg = TtsConfig {
            engine: "command".into(),
            command: vec!["false".into()],
            text_on_stdin: false,
        };
        assert!(synthesize(&cfg, "x").await.is_err());
    }
}
