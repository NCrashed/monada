//! Render-side audio: mix many one-shot SFX in parallel plus one looping
//! music track, with mass-repeat protection. rodio owns the cpal output stream
//! and is `!Send`, so [`Audio`] lives in the host `App` (never in the `Send`
//! bridge); it is fed each frame by [`MapRender::drain_audio`].
//!
//! Behind the `audio` feature: with it off, every method is a no-op that pulls
//! in no cpal/ALSA build dependency (headless / CI / no-audio boxes).
//!
//! **Determinism:** audio is triggered from the map's `tick`, but — like
//! `status` / `entity_set_anim` — it never touches the world hash. A headless
//! peer (or the oracle) no-ops it, so a match can't desync on sound.

#[cfg(feature = "audio")]
pub use real::Audio;
#[cfg(not(feature = "audio"))]
pub use stub::Audio;

#[cfg(feature = "audio")]
mod real {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::time::{Duration, Instant};

    use rodio::buffer::SamplesBuffer;
    use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};

    /// A re-triggered *identical* one-shot within this window of the last is
    /// dropped — the cross-frame half of the mass-repeat guard (the bridge
    /// already de-dups identical sounds fired within one frame).
    const DEBOUNCE: Duration = Duration::from_millis(70);

    /// A loop keeps playing this long after its last `play_loop` request, then
    /// stops. Comfortably longer than a sim tick (33 ms) + a couple of render
    /// frames, so 0-tick frames don't stutter it, but short enough that the
    /// footsteps stop promptly when the hero halts.
    const LOOP_TIMEOUT: Duration = Duration::from_millis(150);

    /// Decoded PCM for one sound, cached so replays don't re-decode the MP3.
    struct Pcm {
        channels: u16,
        rate: u32,
        samples: Vec<f32>,
    }

    pub struct Audio {
        // The stream must outlive the handle it produced; keep it alive even
        // though it's otherwise unused.
        _stream: Option<OutputStream>,
        handle: Option<OutputStreamHandle>,
        /// Raw MP3 bytes by asset path (the map's `assets/sounds/*`).
        assets: BTreeMap<String, Vec<u8>>,
        /// Decoded-PCM cache (`None` = decode failed / missing, don't retry).
        decoded: BTreeMap<String, Option<Pcm>>,
        /// Wall-clock a given sound last started (for the debounce).
        last: BTreeMap<String, Instant>,
        /// The looping-music sink + its path, so replaying the current track is
        /// a seamless no-op instead of a restart.
        music: Option<Sink>,
        music_path: Option<String>,
        /// Active looping SFX (footsteps …) by path: the sink + when it was
        /// last requested. Refreshed by `sync_loops`; stopped after
        /// `LOOP_TIMEOUT` without a request.
        loops: BTreeMap<String, (Sink, Instant)>,
    }

    impl Audio {
        pub fn new(assets: Vec<(String, Vec<u8>)>) -> Self {
            let (stream, handle) = match OutputStream::try_default() {
                Ok((s, h)) => (Some(s), Some(h)),
                Err(e) => {
                    eprintln!("monada-host: audio output unavailable ({e}) — muted");
                    (None, None)
                }
            };
            Audio {
                _stream: stream,
                handle,
                assets: assets.into_iter().collect(),
                decoded: BTreeMap::new(),
                last: BTreeMap::new(),
                music: None,
                music_path: None,
                loops: BTreeMap::new(),
            }
        }

        /// Decode + cache a sound's PCM; `None` on a missing asset / decode
        /// error (the failure is cached so it isn't retried every frame).
        fn pcm<'a>(
            decoded: &'a mut BTreeMap<String, Option<Pcm>>,
            assets: &BTreeMap<String, Vec<u8>>,
            path: &str,
        ) -> Option<&'a Pcm> {
            if !decoded.contains_key(path) {
                let pcm = match assets.get(path) {
                    None => {
                        eprintln!("monada-host: audio: missing asset {path:?}");
                        None
                    }
                    Some(bytes) => match Decoder::new(Cursor::new(bytes.clone())) {
                        Ok(dec) => {
                            let channels = dec.channels();
                            let rate = dec.sample_rate();
                            let samples: Vec<f32> = dec.convert_samples().collect();
                            Some(Pcm {
                                channels,
                                rate,
                                samples,
                            })
                        }
                        Err(e) => {
                            eprintln!("monada-host: audio decode {path:?}: {e}");
                            None
                        }
                    },
                };
                decoded.insert(path.to_string(), pcm);
            }
            decoded.get(path).and_then(Option::as_ref)
        }

        /// Synthesise a short tone on the fly (the Undertale-style typing
        /// "voice") and mix it. `wave`: 0 square / 1 saw / 2 triangle / 3 sine /
        /// 4 noise; `freq` Hz; `dur_ms` length; `gain` 0..1. A tiny attack/
        /// release envelope de-clicks it. No cache/debounce — blips are meant to
        /// fire rapidly, one per typed glyph.
        pub fn blip(&self, wave: i64, freq: i64, dur_ms: i64, gain: f32) {
            if let Some(handle) = &self.handle {
                let _ = handle.play_raw(Self::synth(wave, freq, dur_ms, gain));
            }
        }

        fn synth(wave: i64, freq: i64, dur_ms: i64, gain: f32) -> SamplesBuffer<f32> {
            const RATE: u32 = 44_100;
            let dur_ms = dur_ms.clamp(1, 500) as u32;
            let freq = (freq.clamp(20, 20_000)) as f32;
            let n = (RATE * dur_ms / 1000).max(1) as usize;
            let period = RATE as f32 / freq; // samples per cycle
            let atk = ((RATE as usize * 2 / 1000).min(n / 4)).max(1); // ~2 ms
            let rel = ((RATE as usize * 15 / 1000).min(n / 2)).max(1); // ~15 ms
            let mut seed: u32 = 0x9E37_79B9 ^ (freq as u32); // noise LCG
            let mut data = Vec::with_capacity(n);
            for i in 0..n {
                let phase = (i as f32 / period).fract();
                let s = match wave {
                    1 => 2.0 * phase - 1.0,                     // saw
                    2 => 1.0 - 4.0 * (phase - 0.5).abs(),       // triangle
                    3 => (phase * std::f32::consts::TAU).sin(), // sine
                    4 => {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0 // 24-bit → -1..1
                    }
                    _ => {
                        if phase < 0.5 {
                            1.0
                        } else {
                            -1.0
                        }
                    } // 0 = square (default)
                };
                let env = if i < atk {
                    i as f32 / atk as f32
                } else if i >= n - rel {
                    (n - i) as f32 / rel as f32
                } else {
                    1.0
                };
                data.push(s * gain * env);
            }
            SamplesBuffer::new(1, RATE, data)
        }

        pub fn play(&mut self, path: &str, gain: f32, now: Instant) {
            let Some(handle) = &self.handle else { return };
            if let Some(&t) = self.last.get(path) {
                if now.saturating_duration_since(t) < DEBOUNCE {
                    return;
                }
            }
            let Some(pcm) = Self::pcm(&mut self.decoded, &self.assets, path) else {
                return;
            };
            let src = SamplesBuffer::new(pcm.channels, pcm.rate, pcm.samples.clone());
            if handle.play_raw(src.amplify(gain)).is_ok() {
                self.last.insert(path.to_string(), now);
            }
        }

        pub fn play_music(&mut self, path: &str) {
            let Some(handle) = &self.handle else { return };
            if self.music.is_some() && self.music_path.as_deref() == Some(path) {
                return; // already looping this track
            }
            let Some(pcm) = Self::pcm(&mut self.decoded, &self.assets, path) else {
                return;
            };
            let Ok(sink) = Sink::try_new(handle) else {
                return;
            };
            let src = SamplesBuffer::new(pcm.channels, pcm.rate, pcm.samples.clone());
            sink.append(src.repeat_infinite());
            self.music = Some(sink);
            self.music_path = Some(path.to_string());
        }

        pub fn stop_music(&mut self) {
            if let Some(sink) = self.music.take() {
                sink.stop();
            }
            self.music_path = None;
        }

        /// Reconcile the active loops with the set requested this frame: refresh
        /// or start each requested loop, then stop any that has gone unrequested
        /// for `LOOP_TIMEOUT`.
        pub fn sync_loops(&mut self, requested: &[String], now: Instant) {
            let Some(handle) = &self.handle else { return };
            for path in requested {
                if let Some((_, ts)) = self.loops.get_mut(path) {
                    *ts = now; // still wanted — keep it alive
                } else if let Some(pcm) = Self::pcm(&mut self.decoded, &self.assets, path) {
                    if let Ok(sink) = Sink::try_new(handle) {
                        let src = SamplesBuffer::new(pcm.channels, pcm.rate, pcm.samples.clone());
                        sink.append(src.repeat_infinite());
                        self.loops.insert(path.clone(), (sink, now));
                    }
                }
            }
            self.loops.retain(|_, (sink, ts)| {
                let keep = now.saturating_duration_since(*ts) < LOOP_TIMEOUT;
                if !keep {
                    sink.stop();
                }
                keep
            });
        }
    }
}

#[cfg(not(feature = "audio"))]
mod stub {
    use std::time::Instant;

    /// No-op audio (feature `audio` off): compiles with no cpal / ALSA.
    pub struct Audio;

    impl Audio {
        #[must_use]
        pub fn new(_assets: Vec<(String, Vec<u8>)>) -> Self {
            Audio
        }
        pub fn play(&mut self, _path: &str, _gain: f32, _now: Instant) {}
        pub fn blip(&self, _wave: i64, _freq: i64, _dur_ms: i64, _gain: f32) {}
        pub fn play_music(&mut self, _path: &str) {}
        pub fn stop_music(&mut self) {}
        pub fn sync_loops(&mut self, _requested: &[String], _now: Instant) {}
    }
}
