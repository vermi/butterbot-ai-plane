//! Laptop-side text-to-speech: runs an external command that writes a WAV file,
//! then the audio is streamed to the robot speaker. The default `robot` engine
//! (on-robot flite) does not go through here.

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
    async fn custom_command_and_failure() {
        let cfg = TtsConfig {
            engine: "command".into(),
            command: vec!["false".into()],
            text_on_stdin: false,
        };
        assert!(synthesize(&cfg, "x").await.is_err());
    }
}
