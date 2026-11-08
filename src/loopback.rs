//! What the visualisers listen to, beyond Spotifast's own music.
//!
//! - **All sound**: on Windows, cpal opens an input stream on the default
//!   *output* device in WASAPI loopback mode, which hears the mix the
//!   speakers get. While it is on, the player's own pushes are ignored, since
//!   its music is already in that mix.
//! - **Spotifast + an app**: Windows 10 2004 and later can capture one
//!   program's sound and its child processes (process loopback). That sound
//!   is mixed into the player's in `AudioTap::push_app`, so other programs,
//!   notifications and games stay out (see `app_capture`).
//!
//! Captured sound is converted to stereo at the player's sample rate and
//! pushed into the same `AudioTap` the player uses. Other platforms have no
//! capture here, so only Spotifast's music is shown there.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::settings::VisSource;
use crate::vis::AudioTap;

#[cfg(windows)]
mod app_capture;

/// Whether this platform can listen beyond Spotifast's own music.
pub const SUPPORTED: bool = cfg!(windows);

static SYSTEM_STARTED: OnceLock<()> = OnceLock::new();
#[cfg(windows)]
static APP_STARTED: OnceLock<()> = OnceLock::new();
/// The program "Spotifast + an app" listens to, read by its capture thread.
static APP_NAME: Mutex<String> = Mutex::new(String::new());

/// Points `tap` at `source`. Each capture thread starts the first time its
/// source is chosen and then idles while another source is.
pub fn apply(tap: &Arc<AudioTap>, source: VisSource, app: &str) {
    let source = if SUPPORTED { source } else { VisSource::Player };
    *APP_NAME.lock().unwrap_or_else(|p| p.into_inner()) = app.trim().to_owned();
    tap.set_source(source);
    match source {
        VisSource::Player => {}
        VisSource::AllSound => {
            if SYSTEM_STARTED.set(()).is_ok() {
                spawn("system-audio", Arc::clone(tap), run);
            }
        }
        VisSource::PlayerAndApp =>
        {
            #[cfg(windows)]
            if APP_STARTED.set(()).is_ok() {
                spawn("app-audio", Arc::clone(tap), app_capture::run);
            }
        }
    }
}

/// The program name the app capture should follow right now.
#[cfg_attr(not(windows), allow(dead_code))]
fn app_name() -> String {
    APP_NAME.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Picks the program to capture among `processes` (id, parent id, .exe
/// name): one named `name`, ignoring case and a missing ".exe", whose parent
/// is not also that program, so that capturing its tree takes in its
/// helpers (FL Studio's plugin bridges, say) without doubling them.
#[cfg_attr(not(windows), allow(dead_code))]
fn pick_root(processes: &[(u32, u32, String)], name: &str) -> Option<u32> {
    let wanted = normalise(name);
    if wanted.is_empty() {
        return None;
    }
    let named: Vec<&(u32, u32, String)> = processes
        .iter()
        .filter(|(_, _, exe)| normalise(exe) == wanted)
        .collect();
    named
        .iter()
        .find(|(_, parent, _)| !named.iter().any(|(id, _, _)| id == parent))
        .or(named.first())
        .map(|(id, _, _)| *id)
}

fn normalise(name: &str) -> String {
    let name = name.trim().to_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => name,
    }
}

fn spawn(name: &str, tap: Arc<AudioTap>, body: fn(Arc<AudioTap>)) {
    if let Err(error) = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(tap))
    {
        log::warn!("{name}: could not start capture thread: {error}");
    }
}

/// Opens the default output device in loopback and keeps it open, reopening
/// it after an error or when Windows switches the default output.
fn run(tap: Arc<AudioTap>) {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    loop {
        if tap.source() != VisSource::AllSound {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let Some(device) = host.default_output_device() else {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        let id = device.id().ok();
        let (failed, failure) = std::sync::mpsc::channel::<()>();
        match open(&device, Arc::clone(&tap), failed) {
            Ok(stream) => {
                log::info!("system audio: capturing the default output");
                loop {
                    match failure.recv_timeout(Duration::from_secs(3)) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if tap.source() != VisSource::AllSound {
                                break;
                            }
                            let now = host.default_output_device().and_then(|d| d.id().ok());
                            if now != id {
                                break;
                            }
                        }
                    }
                }
                drop(stream);
            }
            Err(error) => {
                log::warn!("system audio: could not open loopback: {error}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn open(
    device: &cpal::Device,
    tap: Arc<AudioTap>,
    failed: std::sync::mpsc::Sender<()>,
) -> Result<cpal::Stream, String> {
    use cpal::SampleFormat;
    use cpal::traits::{DeviceTrait, StreamTrait};

    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels.max(1));
    let mut convert = Converter::new(channels, config.sample_rate);

    let stream = device
        .build_input_stream_raw(
            config,
            format,
            move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                let out = match format {
                    SampleFormat::F32 => data
                        .as_slice::<f32>()
                        .map(|s| convert.run(s.iter().map(|&x| f64::from(x)))),
                    SampleFormat::F64 => data
                        .as_slice::<f64>()
                        .map(|s| convert.run(s.iter().copied())),
                    SampleFormat::I16 => data.as_slice::<i16>().map(|s| {
                        convert.run(s.iter().map(|&x| f64::from(x) / f64::from(i16::MAX)))
                    }),
                    SampleFormat::I32 => data.as_slice::<i32>().map(|s| {
                        convert.run(s.iter().map(|&x| f64::from(x) / f64::from(i32::MAX)))
                    }),
                    _ => None,
                };
                if let Some(stereo) = out {
                    tap.push_system(stereo);
                }
            },
            move |error: cpal::Error| {
                if crate::soloist_eq::passing(&error) {
                    return;
                }
                log::warn!("system audio: stream error: {error}");
                let _ = failed.send(());
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Mixes any channel count down to stereo and resamples to the player's
/// rate by linear interpolation, which is plenty for a visualiser.
pub(crate) struct Converter {
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame, in input frames, relative to
    /// `last` (which sits at -1).
    pos: f64,
    last: [f64; 2],
    out: Vec<f64>,
}

impl Converter {
    pub(crate) fn new(channels: usize, rate: u32) -> Self {
        Self::between(channels, rate, librespot_playback::SAMPLE_RATE)
    }

    /// Stereo at `to` hertz from `channels` at `from` hertz.
    pub(crate) fn between(channels: usize, from: u32, to: u32) -> Self {
        Self {
            channels,
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 0.0,
            last: [0.0; 2],
            out: Vec::with_capacity(8192),
        }
    }

    pub(crate) fn run(&mut self, samples: impl Iterator<Item = f64>) -> &[f64] {
        self.out.clear();
        let mut frame = [0.0f64; 2];
        let mut index = 0usize;
        let mut channel = 0usize;
        for sample in samples {
            match self.channels {
                1 => {
                    frame = [sample, sample];
                }
                _ if channel < 2 => frame[channel] = sample,
                _ => {}
            }
            channel += 1;
            if channel < self.channels {
                continue;
            }
            channel = 0;
            // `frame` is input frame `index`; `last` is frame `index - 1`.
            while self.pos <= index as f64 {
                let t = self.pos - (index as f64 - 1.0);
                for c in 0..2 {
                    self.out.push(self.last[c] + (frame[c] - self.last[c]) * t);
                }
                self.pos += self.step;
            }
            self.last = frame;
            index += 1;
        }
        self.pos -= index as f64;
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::{Converter, pick_root};

    #[test]
    fn the_program_is_found_by_name_without_case_or_exe() {
        let running = vec![
            (4, 0, "System".to_owned()),
            (100, 50, "FL64.exe".to_owned()),
            (101, 100, "ilbridge.exe".to_owned()),
            (102, 100, "FL64.exe".to_owned()),
        ];
        assert_eq!(pick_root(&running, "FL64.exe"), Some(100));
        assert_eq!(pick_root(&running, "fl64"), Some(100));
        assert_eq!(pick_root(&running, " FL64.EXE "), Some(100));
        assert_eq!(pick_root(&running, "Ableton Live 12 Suite.exe"), None);
        assert_eq!(pick_root(&running, ""), None);
    }

    #[test]
    fn same_rate_stereo_passes_through() {
        let mut c = Converter::new(2, librespot_playback::SAMPLE_RATE);
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let first = c.run(input.iter().copied()).to_vec();
        // One frame of delay from interpolating against the previous frame.
        assert_eq!(first.len(), 6);
        let second = c.run(input.iter().copied()).to_vec();
        assert_eq!(second.len(), 6);
    }

    #[test]
    fn forty_eight_khz_comes_out_at_the_player_rate() {
        let mut c = Converter::new(2, 48_000);
        let mut total = 0;
        for _ in 0..100 {
            total += c.run(std::iter::repeat_n(0.5, 960 * 2)).len() / 2;
        }
        let expected = 96_000 * librespot_playback::SAMPLE_RATE as usize / 48_000;
        assert!(total.abs_diff(expected) <= 2, "{total} vs {expected}");
    }

    #[test]
    fn surround_and_mono_become_stereo() {
        let mut c = Converter::new(6, librespot_playback::SAMPLE_RATE);
        let out = c.run([1.0, -1.0, 9.0, 9.0, 9.0, 9.0].into_iter()).to_vec();
        assert_eq!(out.len(), 2);
        let mut m = Converter::new(1, librespot_playback::SAMPLE_RATE);
        let out = m.run([0.5, 0.5].into_iter()).to_vec();
        assert_eq!(out.len(), 4);
        assert_eq!(out[2], out[3]);
    }
}
