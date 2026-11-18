//! Where the listener's audio goes: the default output device on macOS, a
//! `pw-cat`/`aplay` child process elsewhere, or nowhere at all (`--null`).

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::proto::RATE;
use crate::source::Source;

pub type Shared = Arc<Mutex<Source>>;

pub struct Output(Inner);

enum Inner {
    Pump(Pump),
    #[cfg(target_os = "macos")]
    Device(mac::Device),
}

impl Output {
    pub fn open(source: Shared, null: bool) -> Result<Output> {
        if null {
            return Ok(Output(Inner::Pump(Pump::start(source, None))));
        }
        #[cfg(target_os = "macos")]
        return Ok(Output(Inner::Device(mac::Device::open(source)?)));
        #[cfg(not(target_os = "macos"))]
        return Ok(Output(Inner::Pump(Pump::start(source, Some(spawn_player()?)))));
    }

    /// Start or stop pulling audio. A stopped output lets the device (and the
    /// machine) go idle while playback is paused.
    pub fn set_running(&mut self, on: bool) {
        match &mut self.0 {
            Inner::Pump(p) => p.run.store(on, Ordering::Relaxed),
            #[cfg(target_os = "macos")]
            Inner::Device(d) => d.set_running(on),
        }
    }

    /// False once the output has broken and needs to be reopened.
    pub fn healthy(&self) -> bool {
        match &self.0 {
            Inner::Pump(p) => !p.failed.load(Ordering::Relaxed),
            #[cfg(target_os = "macos")]
            Inner::Device(d) => d.healthy(),
        }
    }
}

/// Pulls from the source in real time and writes s16le to a child process.
struct Pump {
    run: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    child: Option<Child>,
}

impl Pump {
    fn start(source: Shared, mut child: Option<Child>) -> Pump {
        let run = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (run2, stop2, failed2) = (run.clone(), stop.clone(), failed.clone());
        let mut pipe = child.as_mut().and_then(|c| c.stdin.take());

        let thread = thread::spawn(move || {
            const TICK: Duration = Duration::from_millis(10);
            let frames = RATE as usize / 100;
            // A rough figure for what sits between our pipe and the speakers.
            let latency = if pipe.is_some() { RATE as u64 * 60 / 1000 } else { 0 };
            let mut samples = vec![0f32; frames * 2];
            let mut bytes = Vec::with_capacity(frames * 4);
            let mut due = Instant::now();

            while !stop2.load(Ordering::Relaxed) {
                if !run2.load(Ordering::Relaxed) {
                    thread::sleep(TICK * 2);
                    due = Instant::now();
                    continue;
                }
                match source.lock() {
                    Ok(mut s) => s.fill(&mut samples, 2, RATE, latency),
                    Err(_) => break,
                }
                if let Some(pipe) = &mut pipe {
                    bytes.clear();
                    for s in &samples {
                        bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                    }
                    if pipe.write_all(&bytes).is_err() {
                        failed2.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                due += TICK;
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                } else if now - due > Duration::from_millis(200) {
                    due = now; // fell far behind (machine was suspended): don't burst
                }
            }
        });

        Pump { run, stop, failed, thread: Some(thread), child }
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Killing the player first unblocks the thread if it is stuck writing to it.
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_player() -> Result<Child> {
    let rate = RATE.to_string();
    let players: [(&str, Vec<&str>); 2] = [
        (
            "pw-cat",
            vec!["--playback", "--raw", "--format", "s16", "--rate", &rate, "--channels", "2", "--latency", "30ms", "-"],
        ),
        ("aplay", vec!["-q", "-t", "raw", "-f", "S16_LE", "-r", &rate, "-c", "2", "-B", "60000", "-"]),
    ];
    for (program, args) in &players {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => return Ok(child),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no audio player found (needs pw-cat or aplay)")
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    pub struct Device {
        stream: cpal::Stream,
        failed: Arc<AtomicBool>,
        running: bool,
    }

    impl Device {
        /// Open the system default output at whatever format it is already
        /// running, so nothing about the hardware is reconfigured.
        pub fn open(source: Shared) -> Result<Device> {
            let device = cpal::default_host()
                .default_output_device()
                .context("no audio output device")?;
            let config = device.default_output_config()?.config();
            let channels = config.channels as usize;
            let rate = config.sample_rate;

            let failed = Arc::new(AtomicBool::new(false));
            let flag = failed.clone();
            let stream = device.build_output_stream(
                config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    let ahead = ts.playback.saturating_duration_since(ts.callback);
                    let latency = (ahead.as_secs_f64() * rate as f64) as u64;
                    match source.lock() {
                        Ok(mut s) => s.fill(data, channels, rate, latency),
                        Err(_) => data.fill(0.0),
                    }
                },
                move |err: cpal::Error| match err.kind() {
                    // The stream carries on through these (it follows the
                    // default device by itself).
                    cpal::ErrorKind::DeviceChanged
                    | cpal::ErrorKind::Xrun
                    | cpal::ErrorKind::RealtimeDenied => {}
                    _ => flag.store(true, Ordering::Relaxed),
                },
                None,
            )?;
            Ok(Device { stream, failed, running: false })
        }

        pub fn set_running(&mut self, on: bool) {
            if on == self.running {
                return;
            }
            let result = if on { self.stream.play() } else { self.stream.pause() };
            match result {
                Ok(()) => self.running = on,
                Err(_) => self.failed.store(true, Ordering::Relaxed),
            }
        }

        pub fn healthy(&self) -> bool {
            !self.failed.load(Ordering::Relaxed)
        }
    }
}
