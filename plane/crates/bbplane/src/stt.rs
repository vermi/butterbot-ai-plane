//! Speech-to-text with whisper.cpp, on this machine. The English base model is downloaded on
//! first use and checked against a pinned SHA-256.

#[cfg(feature = "stt")]
use std::path::Path;

#[cfg(feature = "stt")]
use anyhow::{Context, Result, anyhow};
#[cfg(feature = "stt")]
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[cfg(feature = "stt")]
use crate::models::{ModelFile, fetch};

/// Whisper base.en (MIT license, OpenAI weights converted by the whisper.cpp project).
#[cfg(feature = "stt")]
const MODEL: ModelFile = ModelFile {
    name: "ggml-base.en.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    max_bytes: 200 << 20,
};

#[cfg(feature = "stt")]
pub struct Stt {
    ctx: WhisperContext,
}

#[cfg(feature = "stt")]
impl Stt {
    /// Load the model from `dir`, downloading it if needed.
    pub fn load(dir: &Path) -> Result<Stt> {
        // whisper.cpp logs through `tracing` (target whisper_rs) instead of straight to stderr.
        static HOOKS: std::sync::Once = std::sync::Once::new();
        HOOKS.call_once(whisper_rs::install_logging_hooks);
        let path = fetch(dir, &MODEL)?;
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("model path is not UTF-8"))?;
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| anyhow!("loading {path}: {e}"))?;
        Ok(Stt { ctx })
    }

    /// Text spoken in `pcm` (16 kHz mono). Empty when there is no speech.
    pub fn transcribe(&self, pcm: &[i16]) -> Result<String> {
        // The robot's microphone is quiet: bring the loudest peak up to about -1 dBFS.
        let peak = pcm
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap_or(0)
            .max(1) as f32;
        let gain = (29_000.0 / peak).clamp(1.0, 30.0) / 32_768.0;
        let audio: Vec<f32> = pcm.iter().map(|&s| s as f32 * gain).collect();

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        // No initial prompt: primed with the robot's name, Whisper treats it as already said
        // and leaves it out of the transcript ("Butterbot, make me..." -> "Make me...").
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_n_threads(4);

        let mut state = self.ctx.create_state().context("whisper state")?;
        state.full(params, &audio).context("transcribing")?;
        let text: String = state
            .as_iter()
            .filter_map(|seg| seg.to_str_lossy().ok().map(|s| s.into_owned()))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(clean(&text))
    }
}

/// Drop Whisper's non-speech markers ("[ Pause ]", "(music)", "[BLANK_AUDIO]") and spacing.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' | '*' if depth == 0 => depth = 1,
            ']' | ')' | '*' if depth == 1 => depth = 0,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // Music notes and punctuation alone ("♪♪", "...") are not speech.
    if !out.chars().any(char::is_alphanumeric) {
        return String::new();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// If `text` addresses the robot as `name` (allowing "hey"/"ok" before it and Whisper's
/// usual spellings such as "Butter bot"), the rest of what was said; else `None`.
pub fn addressed<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let want: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if want.is_empty() {
        return Some(text.trim());
    }
    // Words with their byte ranges.
    let mut words = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_alphanumeric() || c == '\'', start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                words.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        words.push((s, text.len()));
    }
    let lower = |(s, e): (usize, usize)| text[s..e].to_lowercase();
    let mut first = 0;
    while first < words.len()
        && first < 2
        && ["hey", "ok", "okay", "hi", "yo", "oh"].contains(&lower(words[first]).as_str())
    {
        first += 1;
    }
    // The name may come out as one to three words ("Butterbot", "Butter bot", "But her bot").
    for n in 1..=3 {
        if first + n > words.len() {
            break;
        }
        let joined: String = (first..first + n)
            .map(|i| lower(words[i]))
            .collect::<String>()
            .replace('\'', "");
        let joined = joined
            .strip_suffix('s')
            .filter(|_| !want.ends_with('s'))
            .unwrap_or(&joined);
        if edit_distance(joined, &want) <= want.len() / 4 {
            let rest = &text[words[first + n - 1].1..];
            return Some(
                rest.trim_start_matches(|c: char| !c.is_alphanumeric())
                    .trim_end(),
            );
        }
    }
    None
}

/// Whether someone is ending the conversation ("goodbye", "that's all", ...). Only short
/// utterances that are nothing but a farewell count.
pub fn is_goodbye(text: &str) -> bool {
    let t: String = text
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = ["ok ", "okay ", "alright ", "well ", "thanks ", "thank you "]
        .iter()
        .fold(t.as_str(), |t, p| t.strip_prefix(p).unwrap_or(t))
        .to_string();
    [
        "goodbye",
        "good bye",
        "bye",
        "bye bye",
        "bye now",
        "see you",
        "see ya",
        "see you later",
        "later",
        "that's all",
        "that is all",
        "that's it",
        "i'm done",
        "we're done",
        "stop listening",
        "goodnight",
        "good night",
        "talk later",
        "talk to you later",
    ]
    .iter()
    .any(|g| t == *g || t == format!("{g} butterbot") || t == format!("{g} buddy"))
}

/// Whether a request is (probably) about what the robot can see.
pub fn about_seeing(text: &str) -> bool {
    let t = format!(
        " {} ",
        text.to_lowercase()
            .replace(|c: char| !c.is_alphanumeric() && c != '\'', " ")
    );
    [
        " see ",
        " seeing ",
        " look ",
        " looking ",
        " looks ",
        " watch ",
        " show ",
        " camera ",
        " picture ",
        " photo ",
        " describe ",
        " color ",
        " colour ",
        " wearing ",
        " holding ",
        " in front ",
        " around you ",
        " who is ",
        " who's ",
        " whos ",
        " what's this ",
        " what is this ",
        " what's that ",
        " what is that ",
        " how many ",
        " read ",
        " recognize ",
        " recognise ",
    ]
    .iter()
    .any(|w| t.contains(w))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_markers() {
        assert_eq!(clean(" [ Pause ]"), "");
        assert_eq!(clean("(music) Hello  there [BLANK_AUDIO]"), "Hello there");
        assert_eq!(clean(" One, two, three."), "One, two, three.");
        assert_eq!(clean(" ♪♪ "), "");
        assert_eq!(clean("..."), "");
    }

    #[test]
    fn goodbyes() {
        assert!(is_goodbye("Goodbye."));
        assert!(is_goodbye("Okay, bye!"));
        assert!(is_goodbye("Thanks, that's all."));
        assert!(is_goodbye("See you later, Butterbot"));
        assert!(!is_goodbye("Say goodbye to Tommy for me"));
        assert!(!is_goodbye("What's the weather later?"));
    }

    #[test]
    fn seeing() {
        assert!(about_seeing("What do you see?"));
        assert!(about_seeing("Who's that next to me"));
        assert!(about_seeing("What colour is my shirt?"));
        assert!(!about_seeing("Tell me a joke."));
        assert!(!about_seeing("How are you feeling today?"));
        assert!(!about_seeing("Seems fine"));
    }

    #[test]
    fn wake_word() {
        let a = |t| addressed(t, "Butterbot");
        assert_eq!(
            a("Butterbot, make me a sandwich."),
            Some("make me a sandwich.")
        );
        assert_eq!(
            a("Hey Butter bot, what time is it?"),
            Some("what time is it?")
        );
        assert_eq!(a("OK, butterbots. Turn left"), Some("Turn left"));
        assert_eq!(a("Butter-bot!"), Some(""));
        assert_eq!(a("Butterbock come here"), Some("come here"));
        assert_eq!(a("I put the butter on the bread"), None);
        assert_eq!(a("Make me a sandwich, Butterbot"), None);
        assert_eq!(addressed("anything goes", ""), Some("anything goes"));
    }
}
