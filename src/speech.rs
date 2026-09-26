//! Text-to-speech through external engines, plus the amplitude envelope that
//! drives mouth overlays.
//!
//! Microsoft Agent got viseme timings from the SAPI 4 engine. The supported
//! engines do not expose those, so the mouth is driven from the loudness of the
//! rendered audio instead: loud passages open the mouth wide, silence closes
//! it. That tracks speech closely enough to read as lip sync.

use acs::{MouthShape, VoiceInfo};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::audio::parse_wav;

/// Envelope resolution. Roughly matches how often Agent changed mouth images.
const WINDOW_MS: u64 = 45;
static KOKORO_OUTPUT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Default)]
pub enum Engine {
    #[default]
    EspeakNg,
    Kokoro,
}

impl Engine {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "espeak-ng" => Some(Self::EspeakNg),
            "kokoro" => Some(Self::Kokoro),
            _ => None,
        }
    }
}

pub struct Speech {
    pub text: String,
    pub wav: Vec<u8>,
    pub duration_ms: u64,
    envelope: Vec<f32>,
}

impl Speech {
    /// Mouth shape at a point in the utterance.
    pub fn mouth_at(&self, t_ms: u64) -> MouthShape {
        let i = (t_ms / WINDOW_MS) as usize;
        let level = self.envelope.get(i).copied().unwrap_or(0.0);
        match level {
            l if l < 0.08 => MouthShape::Closed,
            l if l < 0.20 => MouthShape::Narrow,
            l if l < 0.38 => MouthShape::Medium,
            l if l < 0.55 => MouthShape::WideOpen1,
            l if l < 0.70 => MouthShape::WideOpen2,
            l if l < 0.85 => MouthShape::WideOpen3,
            _ => MouthShape::WideOpen4,
        }
    }

    /// Fraction of the utterance elapsed, clamped to 0..=1.
    pub fn progress(&self, t_ms: u64) -> f64 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        (t_ms as f64 / self.duration_ms as f64).clamp(0.0, 1.0)
    }
}

/// Renders `text` to audio using the selected engine.
pub fn synthesize(
    text: &str,
    voice: Option<&VoiceInfo>,
    engine: Engine,
    kokoro_voice: Option<&str>,
) -> Result<Speech, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to say".into());
    }

    let wav = match engine {
        Engine::EspeakNg => synthesize_espeak(text, voice)?,
        Engine::Kokoro => synthesize_kokoro(text, voice, kokoro_voice)?,
    };

    let (envelope, duration_ms) = envelope_of(&wav);
    Ok(Speech {
        text: text.to_string(),
        wav,
        duration_ms,
        envelope,
    })
}

fn synthesize_espeak(text: &str, voice: Option<&VoiceInfo>) -> Result<Vec<u8>, String> {
    let mut cmd = std::process::Command::new("espeak-ng");
    cmd.arg("--stdout");
    cmd.args(["-v", &espeak_voice(voice)]);
    cmd.args(["-s", &speed_wpm(voice).to_string()]);
    cmd.args(["-p", &pitch(voice).to_string()]);
    // Everything after this is data, never options.
    cmd.arg("--");
    cmd.arg(text);

    let out = cmd
        .output()
        .map_err(|e| format!("could not run espeak-ng: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("espeak-ng failed: {}", err.trim()));
    }
    if out.stdout.is_empty() {
        return Err("espeak-ng produced no audio".into());
    }
    Ok(out.stdout)
}

fn synthesize_kokoro(
    text: &str,
    voice: Option<&VoiceInfo>,
    configured_voice: Option<&str>,
) -> Result<Vec<u8>, String> {
    let output = kokoro_output_path();
    fs::File::create_new(&output)
        .map_err(|e| format!("could not create Kokoro output file: {}", e))?;

    let voice = configured_voice.unwrap_or_else(|| match voice.and_then(|v| v.gender) {
        Some(2) => "am_adam",
        _ => "af_heart",
    });
    let result = std::process::Command::new("kokoro-tts")
        .args(["--text", text, "--voice", voice])
        .arg(&output)
        .output()
        .map_err(|e| format!("could not run kokoro-tts: {}", e))
        .and_then(|out| {
            if out.status.success() {
                fs::read(&output).map_err(|e| format!("could not read Kokoro audio: {}", e))
            } else {
                Err(format!(
                    "kokoro-tts failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        });
    let _ = fs::remove_file(&output);

    let wav = result?;
    if wav.is_empty() {
        return Err("kokoro-tts produced no audio".into());
    }
    Ok(wav)
}

fn kokoro_output_path() -> std::path::PathBuf {
    let id = KOKORO_OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "agentview-kokoro-{}-{}.wav",
        std::process::id(),
        id
    ))
}

#[cfg(test)]
mod tests {
    use super::Engine;

    #[test]
    fn parses_supported_engines() {
        assert!(matches!(Engine::parse("espeak-ng"), Some(Engine::EspeakNg)));
        assert!(matches!(Engine::parse("kokoro"), Some(Engine::Kokoro)));
        assert!(Engine::parse("other").is_none());
    }
}

/// espeak-ng voice name, e.g. `en+m3`, derived from the character's VOICEINFO.
fn espeak_voice(voice: Option<&VoiceInfo>) -> String {
    let lang = voice
        .and_then(|v| v.language_id)
        .and_then(|id| espeak_language(id & 0x3FF))
        .unwrap_or("en");

    // SAPI 4 encoded gender as 1 = female, 2 = male.
    let variant = match voice.and_then(|v| v.gender) {
        Some(1) => "+f3",
        Some(2) => "+m3",
        _ => "+m3",
    };
    format!("{}{}", lang, variant)
}

fn espeak_language(primary: u16) -> Option<&'static str> {
    Some(match primary {
        0x01 => "ar",
        0x04 => "cmn",
        0x05 => "cs",
        0x06 => "da",
        0x07 => "de",
        0x08 => "el",
        0x09 => "en",
        0x0A => "es",
        0x0B => "fi",
        0x0C => "fr",
        0x0D => "he",
        0x0E => "hu",
        0x10 => "it",
        0x11 => "ja",
        0x12 => "ko",
        0x13 => "nl",
        0x14 => "nb",
        0x15 => "pl",
        0x16 => "pt",
        0x19 => "ru",
        0x1A => "hr",
        0x1B => "sk",
        0x1D => "sv",
        0x1E => "th",
        0x1F => "tr",
        0x24 => "sl",
        _ => return None,
    })
}

/// espeak-ng accepts 80-450 words per minute.
fn speed_wpm(voice: Option<&VoiceInfo>) -> u32 {
    match voice.map(|v| v.speed) {
        Some(s) if (80..=450).contains(&s) => s,
        _ => 165,
    }
}

/// espeak-ng pitch is 0-99. Characters store either that range directly or a
/// SAPI 4 baseline frequency in hertz.
fn pitch(voice: Option<&VoiceInfo>) -> u32 {
    match voice.map(|v| v.pitch as u32) {
        Some(p) if p <= 99 => p,
        Some(hz) => ((hz.saturating_sub(50)) / 3).min(99),
        None => 50,
    }
}

/// Root-mean-square loudness per window, normalised against the loudest window.
fn envelope_of(wav: &[u8]) -> (Vec<f32>, u64) {
    let Some((fmt, pcm)) = parse_wav(wav) else {
        return (Vec::new(), 0);
    };
    if fmt.bits != 16 || pcm.is_empty() {
        return (Vec::new(), 0);
    }

    let channels = fmt.channels as usize;
    let frames = pcm.len() / 2 / channels;
    let duration_ms = (frames as u64 * 1000) / fmt.sample_rate as u64;

    let per_window = (fmt.sample_rate as u64 * WINDOW_MS / 1000).max(1) as usize;
    let mut envelope = Vec::with_capacity(frames / per_window + 1);

    let mut i = 0;
    while i < frames {
        let end = (i + per_window).min(frames);
        let mut sum = 0f64;
        for f in i..end {
            // Mono-mix by taking the first channel; speech is near-identical
            // across channels and this keeps the scan cheap.
            let o = f * channels * 2;
            let s = i16::from_le_bytes([pcm[o], pcm[o + 1]]) as f64 / 32768.0;
            sum += s * s;
        }
        let n = (end - i).max(1) as f64;
        envelope.push((sum / n).sqrt() as f32);
        i = end;
    }

    let peak = envelope.iter().copied().fold(0f32, f32::max);
    if peak > 0.0 {
        for v in &mut envelope {
            *v /= peak;
        }
    }
    (envelope, duration_ms)
}
