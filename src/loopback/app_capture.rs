//! "Spotifast + an app" on Windows: captures one program's sound, and its
//! child processes', with WASAPI process loopback (Windows 10 2004 and
//! later), through the `wasapi` crate.
//!
//! The thread finds the program by its .exe name, captures it until it
//! closes or the setting changes, then looks for it again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::settings::VisSource;
use crate::vis::AudioTap;

/// How often to look for the program while it is not running.
const LOOK_AGAIN: Duration = Duration::from_secs(2);
/// How often to check, while capturing, that the program is still running
/// and still the one chosen.
const CHECK_EVERY: Duration = Duration::from_secs(2);

pub fn run(tap: Arc<AudioTap>) {
    // Process loopback is activated through COM, which wants this thread
    // in the multithreaded apartment.
    let _ = wasapi::initialize_mta();
    let mut warned_for = String::new();
    loop {
        if tap.source() != VisSource::PlayerAndApp {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let name = super::app_name();
        let Some(pid) = find(&name) else {
            std::thread::sleep(LOOK_AGAIN);
            continue;
        };
        log::info!("app audio: capturing {name} (process {pid})");
        if let Err(error) = capture(&tap, pid, &name) {
            if warned_for != name {
                log::warn!("app audio: could not capture {name}: {error}");
                warned_for = name;
            }
            std::thread::sleep(LOOK_AGAIN);
        }
    }
}

/// Captures `pid` and its children until the program closes, the program's
/// name or the source changes, or capture fails.
fn capture(tap: &AudioTap, pid: u32, name: &str) -> Result<(), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

    let rate = librespot_playback::SAMPLE_RATE as usize;
    // Windows converts to this format for us, so no resampling is needed.
    let format = WaveFormat::new(32, 32, &SampleType::Float, rate, 2, None);
    let frame_bytes = format.get_blockalign() as usize;

    let mut client =
        AudioClient::new_application_loopback_client(pid, true).map_err(|e| e.to_string())?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 0,
    };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| e.to_string())?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    client.start_stream().map_err(|e| e.to_string())?;

    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut stereo: Vec<f64> = Vec::new();
    let mut checked = Instant::now();
    let result = loop {
        if checked.elapsed() >= CHECK_EVERY {
            checked = Instant::now();
            if tap.source() != VisSource::PlayerAndApp
                || super::app_name() != name
                || find(name) != Some(pid)
            {
                break Ok(());
            }
        }
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_)) => {
                    if let Err(e) = capture.read_from_device_to_deque(&mut bytes) {
                        return Err(e.to_string());
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let whole = bytes.len() / frame_bytes * frame_bytes;
        if whole > 0 {
            stereo.clear();
            let mut sample = [0u8; 4];
            let mut filled = 0;
            for byte in bytes.drain(..whole) {
                sample[filled] = byte;
                filled += 1;
                if filled == 4 {
                    stereo.push(f64::from(f32::from_le_bytes(sample)));
                    filled = 0;
                }
            }
            tap.push_app(&stereo);
        }
        // Process loopback only signals while the program plays, so a
        // timeout here just means silence.
        let _ = event.wait_for_event(250);
    };
    let _ = client.stop_stream();
    result
}

/// The running program called `name`, or `None`.
fn find(name: &str) -> Option<u32> {
    super::pick_root(&processes(), name)
}

/// Every running process as (id, parent id, .exe name).
fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    // SAFETY: a snapshot handle is checked before use and closed after;
    // the entry is sized as the API asks.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return out;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    out
}
