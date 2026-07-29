//! Animation playback: frame timing, branching and exit sequences.

use std::rc::Rc;

use acs::{Character, ImageCache, MouthShape, RgbaImage};

/// Guards against frames with a zero duration chaining forever in one tick.
const MAX_STEPS_PER_TICK: u32 = 64;

/// What happened during one advance of the clock.
#[derive(Debug, Default)]
pub struct Tick {
    /// The displayed frame changed and the stage needs a new texture.
    pub frame_changed: bool,
    /// A sound the new frame asks to play, as an index into the audio table.
    pub sound: Option<usize>,
    /// The animation ran to its end and playback stopped.
    pub finished: bool,
}

pub struct Player {
    character: Rc<Character>,
    cache: ImageCache,
    animation: Option<usize>,
    frame: usize,
    elapsed_us: u64,
    playing: bool,
    looping: bool,
    /// xorshift state, so branch probabilities do not need a crate.
    rng: u64,
}

impl Player {
    pub fn new(character: Rc<Character>) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        Self {
            character,
            cache: ImageCache::new(),
            animation: None,
            frame: 0,
            elapsed_us: 0,
            playing: false,
            looping: true,
            rng: seed | 1,
        }
    }

    pub fn character(&self) -> &Rc<Character> {
        &self.character
    }

    pub fn animation_index(&self) -> Option<usize> {
        self.animation
    }

    pub fn frame_index(&self) -> usize {
        self.frame
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn frame_count(&self) -> usize {
        self.current_animation().map(|a| a.frames.len()).unwrap_or(0)
    }

    pub fn current_animation(&self) -> Option<&acs::Animation> {
        self.animation.and_then(|i| self.character.animations.get(i))
    }

    fn current_frame(&self) -> Option<&acs::Frame> {
        self.current_animation().and_then(|a| a.frames.get(self.frame))
    }

    /// Selects an animation and rewinds to its first frame. Returns the sound
    /// the first frame plays, if any.
    pub fn select(&mut self, index: usize) -> Option<usize> {
        self.animation = Some(index);
        self.frame = 0;
        self.elapsed_us = 0;
        self.playing = self.frame_count() > 0;
        self.current_frame().and_then(|f| f.audio_index).map(|i| i as usize)
    }

    pub fn play(&mut self) {
        if self.frame_count() > 0 {
            self.playing = true;
        }
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Stops and returns to the first frame.
    pub fn stop(&mut self) {
        self.playing = false;
        self.frame = 0;
        self.elapsed_us = 0;
    }

    /// Jumps to the animation's exit frame so it can play out gracefully.
    /// Returns false when the animation has no exit sequence.
    pub fn exit(&mut self) -> bool {
        let Some(frame) = self.current_frame() else { return false };
        let exit = frame.exit_frame;
        if exit < 0 || exit as usize >= self.frame_count() {
            return false;
        }
        self.frame = exit as usize;
        self.elapsed_us = 0;
        self.playing = true;
        true
    }

    pub fn seek(&mut self, frame: usize) {
        if frame < self.frame_count() {
            self.frame = frame;
            self.elapsed_us = 0;
        }
    }

    /// Advances playback by `dt_us` microseconds.
    pub fn advance(&mut self, dt_us: u64) -> Tick {
        let mut tick = Tick::default();
        if !self.playing || self.frame_count() == 0 {
            return tick;
        }

        self.elapsed_us += dt_us;
        let mut steps = 0;

        loop {
            let Some(frame) = self.current_frame() else { break };
            let due_us = frame.duration_ms() * 1000;

            // A zero-duration frame advances immediately.
            if due_us > 0 && self.elapsed_us < due_us {
                break;
            }
            self.elapsed_us = self.elapsed_us.saturating_sub(due_us);

            steps += 1;
            if steps > MAX_STEPS_PER_TICK {
                self.elapsed_us = 0;
                break;
            }

            match self.next_frame() {
                Some(next) => {
                    self.frame = next;
                    tick.frame_changed = true;
                    if let Some(sound) = self.current_frame().and_then(|f| f.audio_index) {
                        tick.sound = Some(sound as usize);
                    }
                }
                None => {
                    self.playing = false;
                    tick.finished = true;
                    self.elapsed_us = 0;
                    break;
                }
            }
        }

        tick
    }

    /// Picks the frame to show next, honouring branch probabilities. Returns
    /// `None` when the animation is over.
    fn next_frame(&mut self) -> Option<usize> {
        let count = self.frame_count();
        let branches = self.current_frame().map(|f| f.branches.clone()).unwrap_or_default();

        if !branches.is_empty() {
            let roll = self.next_random(100) + 1; // 1..=100
            let mut acc = 0u32;
            for b in &branches {
                acc += b.probability as u32;
                if roll <= acc {
                    if (b.frame_index as usize) < count {
                        return Some(b.frame_index as usize);
                    }
                    break;
                }
            }
        }

        let next = self.frame + 1;
        if next < count {
            return Some(next);
        }
        if self.looping {
            return Some(0);
        }
        None
    }

    fn next_random(&mut self, modulo: u32) -> u32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let v = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((v >> 33) % modulo as u64) as u32
    }

    /// Composites the current frame, substituting `mouth` where the frame
    /// provides overlays.
    pub fn render(&mut self, mouth: Option<MouthShape>) -> Option<RgbaImage> {
        let index = self.animation?;
        let frame_index = self.frame;
        // Take a second handle so the frame borrow is independent of `self`,
        // leaving the cache free to be borrowed mutably.
        let character = Rc::clone(&self.character);
        let frame = character.animations.get(index)?.frames.get(frame_index)?;
        character.render_frame(frame, mouth, &mut self.cache).ok()
    }

    /// Renders a still of the first frame of an animation, for previews.
    pub fn render_first_frame(&mut self, index: usize) -> Option<RgbaImage> {
        let character = Rc::clone(&self.character);
        let frame = character.animations.get(index)?.frames.first()?;
        character.render_frame(frame, None, &mut self.cache).ok()
    }
}
