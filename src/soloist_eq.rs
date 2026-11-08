//! Audible EQ for Soloist on Windows, through a virtual audio cable.
//!
//! Windows gives Spotifast no way to quiet Soloist's own output while still
//! capturing it: the session and stream controls tested affect capture too
//! (see docs/_reference/soloist.md). So the listener points Soloist's output
//! at a virtual cable (VB-Audio's free "VB-Cable") in Windows' per-app
//! sound settings, once. Nobody hears the cable. This route records the
//! cable's other end, runs it through the same equalizer as the mini
//! player, and plays it on the default output. Soloist's own volume still
//! sets the level, since it applies before the cable. Nothing else is
//! routed, so other programs (FL Studio, say) are never equalized.
//!
//! If Windows forgets the per-app choice (a new Soloist build, say),
//! Soloist simply plays straight to the speakers again, without EQ.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::eq::SharedEq;
use crate::loopback::Converter;

/// Whether this platform needs, and has, the cable route.
pub const SUPPORTED: bool = cfg!(windows);

/// Audio kept between the cable and the speakers, in milliseconds: enough to
/// ride over the two devices' scheduling, little enough not to be noticed.
const TARGET_MS: usize = 40;
/// Past this the speakers' clock has fallen behind the cable's; the oldest
/// audio is dropped back to the target.
const MAX_MS: usize = 160;

/// What the route is doing, for Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    /// No virtual cable is installed.
    NoCable,
    /// Windows' default output is the cable itself, so nothing would be
    /// heard.
    CableIsDefault,
    /// Playing from the cable to this output.
    Running {
        output: String,
    },
    Failed(String),
}

/// A running route. Dropping it stops it.
pub struct Route {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("status", &self.status())
            .finish()
    }
}

impl Route {
    pub fn start(eq: SharedEq) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status::Starting));
        let (thread_stop, thread_status) = (Arc::clone(&stop), Arc::clone(&status));
        if let Err(error) = std::thread::Builder::new()
            .name("soloist-eq".into())
            .spawn(move || run(eq, &thread_stop, &thread_status))
        {
            *status.lock().unwrap_or_else(|p| p.into_inner()) = Status::Failed(error.to_string());
        }
        Self { stop, status }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Whether a device's name says it is VB-Cable's recording end. The
/// playback end is "CABLE Input", which is what Soloist is pointed at.
pub fn is_cable_output(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("cable output")
}

/// Whether a playback device is a VB-Cable end, which nobody hears.
pub fn is_cable(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("cable input") || name.contains("vb-audio")
}

fn set(status: &Mutex<Status>, value: Status) {
    let mut current = status.lock().unwrap_or_else(|p| p.into_inner());
    if *current != value {
        log::info!("soloist eq: {value:?}");
        *current = value;
    }
}

fn device_name(device: &cpal::Device) -> String {
    use cpal::traits::DeviceTrait;
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_default()
}

fn run(eq: SharedEq, stop: &AtomicBool, status: &Mutex<Status>) {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    while !stop.load(Ordering::Relaxed) {
        let cable = host
            .input_devices()
            .ok()
            .and_then(|mut devices| devices.find(|device| is_cable_output(&device_name(device))));
        let Some(cable) = cable else {
            set(status, Status::NoCable);
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let Some(speakers) = host.default_output_device() else {
            set(status, Status::Failed("no output device".into()));
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let output = device_name(&speakers);
        if is_cable(&output) {
            set(status, Status::CableIsDefault);
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        let speakers_id = speakers.id().ok();
        let (failed, failure) = std::sync::mpsc::channel::<String>();
        match open(&cable, &speakers, Arc::clone(&eq), failed) {
            Ok(streams) => {
                set(status, Status::Running { output });
                // Hold the streams until one fails, the default output
                // changes, or the route is turned off.
                loop {
                    match failure.recv_timeout(Duration::from_secs(2)) {
                        Ok(error) => {
                            set(status, Status::Failed(error));
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            let now = host.default_output_device().and_then(|d| d.id().ok());
                            if now != speakers_id {
                                break;
                            }
                        }
                    }
                }
                drop(streams);
            }
            Err(error) => {
                set(status, Status::Failed(error));
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

/// Audio on its way from the cable to the speakers: stereo, interleaved,
/// at the speakers' rate.
#[derive(Debug, Default)]
pub struct Buffer {
    samples: VecDeque<f32>,
    /// Refilling to the target after running dry, rather than playing each
    /// scrap as it comes.
    priming: bool,
}

impl Buffer {
    /// Adds audio, dropping the oldest when more than `max` frames wait.
    pub fn push(&mut self, stereo: &[f64], target: usize, max: usize) {
        self.samples.extend(stereo.iter().map(|&s| s as f32));
        if self.samples.len() > max * 2 {
            let excess = self.samples.len() - target * 2;
            self.samples.drain(..excess);
        }
    }

    /// Fills `out` (`channels` interleaved) from the buffer: the left and
    /// right into the first two channels, silence elsewhere, and silence
    /// while refilling to `target` frames.
    pub fn pull(&mut self, out: &mut [f32], channels: usize, target: usize) {
        let channels = channels.max(1);
        if self.samples.is_empty() {
            self.priming = true;
        }
        if self.priming && self.samples.len() >= target * 2 {
            self.priming = false;
        }
        for frame in out.chunks_mut(channels) {
            let (left, right) = if self.priming || self.samples.len() < 2 {
                (0.0, 0.0)
            } else {
                (
                    self.samples.pop_front().unwrap_or(0.0),
                    self.samples.pop_front().unwrap_or(0.0),
                )
            };
            match frame {
                [mono] => *mono = (left + right) / 2.0,
                [l, r, rest @ ..] => {
                    *l = left;
                    *r = right;
                    rest.fill(0.0);
                }
                [] => {}
            }
        }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }
}

/// Whether a stream error is one the stream carries on through: a moment's
/// gap in the audio (the cable marks one whenever Soloist starts, stops or
/// stutters), or a reroute it already followed. Rebuilding the route for
/// these would cut the sound far worse than the gap did.
pub fn passing(error: &cpal::Error) -> bool {
    matches!(
        error.kind(),
        cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged
    )
}

fn open(
    cable: &cpal::Device,
    speakers: &cpal::Device,
    eq: SharedEq,
    failed: std::sync::mpsc::Sender<String>,
) -> Result<(cpal::Stream, cpal::Stream), String> {
    use cpal::SampleFormat;
    use cpal::traits::{DeviceTrait, StreamTrait};

    let input = cable.default_input_config().map_err(|e| e.to_string())?;
    let output = speakers
        .default_output_config()
        .map_err(|e| e.to_string())?;
    if output.sample_format() != SampleFormat::F32 {
        return Err(format!(
            "the output plays {:?} samples; only 32-bit float is supported",
            output.sample_format()
        ));
    }
    let in_format = input.sample_format();
    let in_config = input.config();
    let out_config = output.config();
    let out_rate = out_config.sample_rate;
    let out_channels = usize::from(out_config.channels.max(1));
    let target = out_rate as usize * TARGET_MS / 1000;
    let max = out_rate as usize * MAX_MS / 1000;

    let buffer = Arc::new(Mutex::new(Buffer::default()));
    let mut convert = Converter::between(
        usize::from(in_config.channels.max(1)),
        in_config.sample_rate,
        out_rate,
    );
    let mut equalizer = crate::eq::Processor::at_rate(eq, out_rate);
    let mut work: Vec<f64> = Vec::new();
    let into = Arc::clone(&buffer);
    let in_failed = failed.clone();
    let capture = cable
        .build_input_stream_raw(
            in_config,
            in_format,
            move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                let stereo = match in_format {
                    SampleFormat::F32 => data
                        .as_slice::<f32>()
                        .map(|s| convert.run(s.iter().map(|&x| f64::from(x)))),
                    SampleFormat::I16 => data.as_slice::<i16>().map(|s| {
                        convert.run(s.iter().map(|&x| f64::from(x) / f64::from(i16::MAX)))
                    }),
                    SampleFormat::I32 => data.as_slice::<i32>().map(|s| {
                        convert.run(s.iter().map(|&x| f64::from(x) / f64::from(i32::MAX)))
                    }),
                    _ => None,
                };
                let Some(stereo) = stereo else {
                    return;
                };
                work.clear();
                work.extend_from_slice(stereo);
                equalizer.process(&mut work);
                // The equalizer leaves boosts unclipped; keep them in range.
                for sample in &mut work {
                    *sample = sample.clamp(-1.0, 1.0);
                }
                into.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(&work, target, max);
            },
            move |error: cpal::Error| {
                if passing(&error) {
                    log::debug!("soloist eq: cable: {error}");
                } else {
                    let _ = in_failed.send(format!("cable: {error}"));
                }
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    let from = Arc::clone(&buffer);
    let playback = speakers
        .build_output_stream(
            out_config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                from.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pull(out, out_channels, target);
            },
            move |error: cpal::Error| {
                if passing(&error) {
                    log::debug!("soloist eq: output: {error}");
                } else {
                    let _ = failed.send(format!("output: {error}"));
                }
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    capture.play().map_err(|e| e.to_string())?;
    playback.play().map_err(|e| e.to_string())?;
    Ok((capture, playback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glitch_does_not_rebuild_the_route_but_a_lost_device_does() {
        assert!(passing(&cpal::ErrorKind::Xrun.into()));
        assert!(passing(&cpal::ErrorKind::DeviceChanged.into()));
        assert!(!passing(&cpal::ErrorKind::DeviceNotAvailable.into()));
        assert!(!passing(&cpal::ErrorKind::BackendError.into()));
    }

    #[test]
    fn vb_cable_is_recognised_by_its_names() {
        assert!(is_cable_output("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!is_cable_output("Microphone (Realtek Audio)"));
        assert!(is_cable("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(!is_cable("Speakers (Realtek(R) Audio)"));
        assert!(!is_cable("Focusrite USB (Focusrite USB Audio)"));
    }

    #[test]
    fn the_buffer_primes_then_plays_in_order() {
        let mut buffer = Buffer::default();
        let mut out = [9.0f32; 4];
        // Empty: silence while it fills.
        buffer.pull(&mut out, 2, 2);
        assert_eq!(out, [0.0; 4]);
        buffer.push(&[0.1, 0.2], 2, 10);
        buffer.pull(&mut out, 2, 2);
        assert_eq!(out, [0.0; 4], "still under the target");
        buffer.push(&[0.3, 0.4, 0.5, 0.6], 2, 10);
        buffer.pull(&mut out, 2, 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn the_buffer_drops_old_audio_when_it_backs_up() {
        let mut buffer = Buffer::default();
        buffer.push(&vec![0.5; 2 * 20], 4, 10);
        assert_eq!(buffer.frames(), 4);
    }

    #[test]
    fn surround_outputs_get_left_and_right_only() {
        let mut buffer = Buffer::default();
        buffer.push(&[0.25, -0.25], 1, 10);
        let mut out = [9.0f32; 6];
        buffer.pull(&mut out, 6, 1);
        assert_eq!(out, [0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }
}
