//! Waveform playback.
//!
//! Character sounds are RIFF/WAVE blobs lifted straight out of the .acs file
//! and may use any codec the author had installed, so playback is handed to a
//! system player backed by libsndfile rather than decoded in-process.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Players tried in order; each one reads a WAVE stream on stdin.
const BACKENDS: &[(&str, &[&str])] = &[
    ("paplay", &["--client-name=Agent Viewer", "--stream-name=Agent Viewer"]),
    ("pw-play", &["-"]),
    ("aplay", &["-q", "-"]),
];

#[derive(Clone, Default)]
pub struct AudioPlayer {
    running: Arc<Mutex<Vec<Child>>>,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts playing a WAVE buffer, returning immediately.
    pub fn play(&self, data: Vec<u8>) {
        self.reap();

        let mut spawned = None;
        for (program, args) in BACKENDS {
            let child = Command::new(program)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(c) = child {
                spawned = Some(c);
                break;
            }
        }
        let Some(mut child) = spawned else { return };

        // Hand the pipe to a writer thread: the buffer can exceed the pipe
        // capacity, and blocking here would stall the frame clock.
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
                let _ = stdin.flush();
            });
        }

        if let Ok(mut running) = self.running.lock() {
            running.push(child);
        }
    }

    /// Stops everything currently playing.
    pub fn stop_all(&self) {
        if let Ok(mut running) = self.running.lock() {
            for mut child in running.drain(..) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Clears finished players so the list does not grow without bound.
    fn reap(&self) {
        if let Ok(mut running) = self.running.lock() {
            running.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Locates the PCM payload of a WAVE buffer and returns it with its format.
///
/// The declared `data` chunk size is ignored: streamed WAVE output (espeak-ng's
/// among others) writes a placeholder length, so the payload is taken as the
/// rest of the buffer when the declared size does not fit.
pub fn parse_wav(data: &[u8]) -> Option<(WavFormat, &[u8])> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }

    let mut pos = 12;
    let mut format: Option<WavFormat> = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        let body = pos + 8;

        if id == b"fmt " && body + 16 <= data.len() {
            format = Some(WavFormat {
                tag: u16::from_le_bytes([data[body], data[body + 1]]),
                channels: u16::from_le_bytes([data[body + 2], data[body + 3]]).max(1),
                sample_rate: u32::from_le_bytes([
                    data[body + 4],
                    data[body + 5],
                    data[body + 6],
                    data[body + 7],
                ])
                .max(1),
                bits: u16::from_le_bytes([data[body + 14], data[body + 15]]),
            });
        } else if id == b"data" {
            let end = body.saturating_add(size).min(data.len());
            let pcm = if size == 0 || body + size > data.len() { &data[body..] } else { &data[body..end] };
            return format.map(|f| (f, pcm));
        }

        // Chunks are padded to even lengths.
        pos = body + size + (size & 1);
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub struct WavFormat {
    pub tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
}
