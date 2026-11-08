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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::eq::SharedEq;
use crate::loopback::Converter;

/// Whether this platform needs, and has, the cable route.
pub const SUPPORTED: bool = cfg!(windows);

/// Audio kept between the cable and the speakers, in milliseconds. VB-Cable
/// hands its audio over in bursts rather than evenly, so the reserve has to
/// outlast the gaps between them; for music a little delay goes unnoticed.
const TARGET_MS: usize = 150;
/// Past this the speakers' clock has fallen behind the cable's, or a burst
/// came in late; the oldest audio is dropped back to the target.
const MAX_MS: usize = 700;

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
    /// How many times the reserve ran dry and the sound stopped to refill.
    dropouts: Arc<AtomicU64>,
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
        let dropouts = Arc::new(AtomicU64::new(0));
        let (thread_stop, thread_status, thread_dropouts) = (
            Arc::clone(&stop),
            Arc::clone(&status),
            Arc::clone(&dropouts),
        );
        if let Err(error) = std::thread::Builder::new()
            .name("soloist-eq".into())
            .spawn(move || run(eq, &thread_stop, &thread_status, &thread_dropouts))
        {
            *status.lock().unwrap_or_else(|p| p.into_inner()) = Status::Failed(error.to_string());
        }
        Self {
            stop,
            status,
            dropouts,
        }
    }

    /// How many times the sound has stopped to refill since the route
    /// started.
    pub fn dropouts(&self) -> u64 {
        self.dropouts.load(Ordering::Relaxed)
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

fn run(eq: SharedEq, stop: &AtomicBool, status: &Mutex<Status>, dropouts: &Arc<AtomicU64>) {
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
        match open(
            &cable,
            &speakers,
            Arc::clone(&eq),
            failed,
            Arc::clone(dropouts),
        ) {
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
    /// Whether anything has played yet, so the first fill is not counted as
    /// a dropout.
    played: bool,
    /// Times it ran dry after playing.
    pub dropouts: u64,
    /// Frames of reserve added to the target since, one step per dropout:
    /// the reserve grows to whatever this cable and computer need.
    grown: usize,
}

impl Buffer {
    /// The reserve to keep: `target`, grown after dropouts, at most half
    /// of `max` so that trimming always has room.
    fn reserve(&self, target: usize, max: usize) -> usize {
        (target + self.grown).min(max / 2).max(target.min(max))
    }

    /// Adds audio, dropping the oldest when more than `max` frames wait.
    pub fn push(&mut self, stereo: &[f64], target: usize, max: usize) {
        self.samples.extend(stereo.iter().map(|&s| s as f32));
        if self.samples.len() > max * 2 {
            let keep = self.reserve(target, max) * 2;
            let excess = self.samples.len() - keep;
            self.samples.drain(..excess);
        }
    }

    /// Fills `out` (`channels` interleaved) from the buffer: the left and
    /// right into the first two channels, silence elsewhere, and silence
    /// while refilling to `target` frames.
    pub fn pull(&mut self, out: &mut [f32], channels: usize, target: usize, max: usize) {
        let channels = channels.max(1);
        let target = self.reserve(target, max);
        if self.priming && self.samples.len() >= target * 2 {
            self.priming = false;
        }
        for frame in out.chunks_mut(channels) {
            if !self.priming && self.samples.len() < 2 {
                // Ran dry: wait for the whole reserve again rather than
                // stuttering on each scrap that comes in.
                self.priming = true;
                if self.played {
                    self.dropouts += 1;
                    self.grown += target / 2;
                }
            }
            let (left, right) = if self.priming {
                (0.0, 0.0)
            } else {
                self.played = true;
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
    dropouts: Arc<AtomicU64>,
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
                let mut buffer = from.lock().unwrap_or_else(|p| p.into_inner());
                let before = buffer.dropouts;
                buffer.pull(out, out_channels, target, max);
                if buffer.dropouts != before {
                    dropouts.fetch_add(buffer.dropouts - before, Ordering::Relaxed);
                }
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
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.0; 4]);
        buffer.push(&[0.1, 0.2], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.0; 4], "still under the target");
        buffer.push(&[0.3, 0.4, 0.5, 0.6], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn running_dry_counts_a_dropout_and_refills_before_playing_on() {
        let mut buffer = Buffer::default();
        let mut out = [9.0f32; 4];
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(buffer.dropouts, 0, "the first fill is not a dropout");
        buffer.push(&[0.1, 0.2, 0.3, 0.4], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(buffer.dropouts, 1);
        // A scrap is not enough to play on.
        buffer.push(&[0.5, 0.6], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.0; 4]);
        // The reserve grew by half after the dropout: two frames are no
        // longer enough, three are.
        buffer.push(&[0.7, 0.8], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.0; 4], "the reserve grew");
        buffer.push(&[0.9, 1.0], 2, 10);
        buffer.pull(&mut out, 2, 2, 10);
        assert_eq!(out, [0.5, 0.6, 0.7, 0.8]);
        assert_eq!(buffer.dropouts, 1);
    }

    #[test]
    fn bursty_cable_audio_plays_without_gaps() {
        // 48 kHz; the speakers take 10 ms at a time, the cable hands over
        // up to 120 ms at once, unevenly, at the same average rate.
        let rate = 48_000;
        let (target, max) = (rate * super::TARGET_MS / 1000, rate * super::MAX_MS / 1000);
        let mut buffer = Buffer::default();
        let mut out = vec![0.0f32; 480 * 2];
        let bursts = [1200usize, 5760, 960, 4800, 2880, 480, 5760, 2400];
        let mut owed = 0usize;
        let mut burst = 0;
        for tick in 0..6000 {
            owed += 480;
            // Every few ticks the cable delivers whatever it held back.
            if tick % 7 == 0 || owed >= 5760 {
                let size = bursts[burst % bursts.len()].min(owed);
                burst += 1;
                owed -= size;
                buffer.push(&vec![0.25; size * 2], target, max);
            }
            buffer.pull(&mut out, 2, target, max);
        }
        // At most a couple while the reserve finds its size, then none.
        let settled = buffer.dropouts;
        assert!(settled <= 2, "{settled} dropouts settling");
        for tick in 0..6000 {
            owed += 480;
            if tick % 7 == 0 || owed >= 5760 {
                let size = bursts[burst % bursts.len()].min(owed);
                burst += 1;
                owed -= size;
                buffer.push(&vec![0.25; size * 2], target, max);
            }
            buffer.pull(&mut out, 2, target, max);
        }
        assert_eq!(buffer.dropouts, settled, "none once it has grown");
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
        buffer.pull(&mut out, 6, 1, 10);
        assert_eq!(out, [0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }
}
