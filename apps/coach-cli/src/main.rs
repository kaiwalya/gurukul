//! coach-cli entry point.
//!
//! The host's job is wiring: build peripheral adapters, dispatch on
//! the subcommand. Real product behaviour lives in `adapter-app-coach`
//! and the other adapters.

use clap::{Parser, Subcommand};
use domain_ports::app_coach::{AppCoach, AppCoachDeps};
use domain_ports::audio_capture::{AudioCapture, CaptureConfig, CaptureFrame};
use domain_ports::audio_devices::{AudioDevices, InputDevice, SampleRateSupport, Transport};
use domain_ports::clock::Clock;
use domain_ports::telemetry::{Level, Telemetry};
use domain_ports::{fields, tel_info, tel_warn};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "coach-cli", version, about = "gurukul singing-coach CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the coach (default when no subcommand is given).
    Run,
    /// Enumerate audio input devices and print a summary.
    ListDevices,
    /// Open an input device and log per-window stats.
    Capture {
        /// Duration to capture, in milliseconds.
        #[arg(long, default_value_t = 3000)]
        duration_ms: u64,
        /// Stats window in milliseconds.
        #[arg(long, default_value_t = 100)]
        window_ms: u64,
        /// Persistent id (from `list-devices`) of the device to
        /// open. Default: system multimedia-role default input.
        #[arg(long)]
        persistent_id: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run_coach(),
        Command::ListDevices => list_devices(),
        Command::Capture {
            duration_ms,
            window_ms,
            persistent_id,
        } => capture(duration_ms, window_ms, persistent_id),
    }
}

fn run_coach() {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));
    let coach = adapter_app_coach::new();
    coach.main(AppCoachDeps {
        clock,
        telemetry,
        host_version: env!("CARGO_PKG_VERSION"),
    });
}

fn list_devices() {
    let devices_port = adapter_audio_cpal::new_devices();
    let devices = devices_port.list_devices();
    let default_name = devices_port.default_input().map(|s| s.name);

    if devices.is_empty() {
        println!("No input devices found.");
        return;
    }

    println!("Input devices ({}):", devices.len());
    for d in &devices {
        print_device(d, default_name.as_deref());
    }
}

fn capture(duration_ms: u64, window_ms: u64, persistent_id: Option<String>) {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));

    let devices_port = adapter_audio_cpal::new_devices();
    let chosen = match &persistent_id {
        Some(pid) => devices_port
            .list_devices()
            .into_iter()
            .find(|d| d.persistent_id.as_ref() == Some(pid))
            .and_then(|mut d| d.streams.pop()),
        None => devices_port.default_input(),
    };
    let Some(stream_info) = chosen else {
        tel_warn!(
            &*telemetry,
            "capture: no matching device",
            persistent_id = persistent_id.clone().unwrap_or_default(),
        );
        return;
    };

    let sample_rate = preferred_sample_rate(&stream_info.sample_rates);
    let channels = stream_info.channels;

    tel_info!(
        &*telemetry,
        "capture: opening input",
        device = stream_info.name.clone(),
        sample_rate = sample_rate,
        channels = channels as u32,
        duration_ms = duration_ms,
        window_ms = window_ms,
    );

    let window = WindowAggregator::new(window_ms, sample_rate, channels, Arc::clone(&telemetry));
    let window = Arc::new(Mutex::new(window));
    let window_for_cb = Arc::clone(&window);

    let capture_port = adapter_audio_cpal::new_capture(Arc::clone(&clock));
    let cfg = CaptureConfig {
        sample_rate,
        channels,
    };
    let session = match capture_port.open(
        stream_info.handle.clone(),
        cfg,
        Box::new(move |frame: CaptureFrame<'_>| {
            window_for_cb.lock().unwrap().push(&frame);
        }),
    ) {
        Ok(s) => s,
        Err(e) => {
            tel_warn!(&*telemetry, "capture: open failed", error = e.to_string());
            return;
        }
    };

    thread::sleep(Duration::from_millis(duration_ms));
    drop(session);

    // Flush whatever is left in the current window.
    window.lock().unwrap().flush();
    tel_info!(&*telemetry, "capture: done");
}

/// Pick a sample rate to request from the stream. For ranges we
/// prefer 48000 if it falls in any range, else the lowest range
/// minimum we see, else 48000 as a guess for `ProbeOnly`.
fn preferred_sample_rate(s: &SampleRateSupport) -> u32 {
    const PREFERRED: u32 = 48_000;
    match s {
        SampleRateSupport::List(rates) => {
            if rates.contains(&PREFERRED) {
                PREFERRED
            } else {
                rates.first().copied().unwrap_or(PREFERRED)
            }
        }
        SampleRateSupport::Ranges(ranges) => {
            for (lo, hi) in ranges {
                if (*lo..=*hi).contains(&PREFERRED) {
                    return PREFERRED;
                }
            }
            ranges.iter().map(|(lo, _)| *lo).min().unwrap_or(PREFERRED)
        }
        SampleRateSupport::ProbeOnly => PREFERRED,
    }
}

/// Accumulate stats across `window_ms` of audio, emit one log line
/// per closed window.
struct WindowAggregator {
    samples_per_window: usize,
    channels: u16,
    telemetry: Arc<dyn Telemetry>,
    accum: usize,
    min: f32,
    max: f32,
    sum_sq: f64,
    count: usize,
    window_index: u64,
}

impl WindowAggregator {
    fn new(window_ms: u64, sample_rate: u32, channels: u16, telemetry: Arc<dyn Telemetry>) -> Self {
        let samples_per_window =
            ((window_ms * sample_rate as u64) / 1000) as usize * channels.max(1) as usize;
        Self {
            samples_per_window,
            channels,
            telemetry,
            accum: 0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sum_sq: 0.0,
            count: 0,
            window_index: 0,
        }
    }

    fn push(&mut self, frame: &CaptureFrame<'_>) {
        for &s in frame.samples {
            if s < self.min {
                self.min = s;
            }
            if s > self.max {
                self.max = s;
            }
            self.sum_sq += (s as f64) * (s as f64);
            self.count += 1;
            self.accum += 1;
            if self.accum >= self.samples_per_window {
                self.emit(frame.t_ms);
                self.reset();
            }
        }
    }

    fn flush(&mut self) {
        if self.count > 0 {
            // Use 0 t_ms — we don't have a clock here; the caller
            // already logged "capture: done".
            self.emit(0);
            self.reset();
        }
    }

    fn emit(&mut self, t_ms: u64) {
        let rms = if self.count > 0 {
            (self.sum_sq / self.count as f64).sqrt() as f32
        } else {
            0.0
        };
        self.telemetry.log(
            Level::Info,
            "capture window",
            &fields! {
                window = self.window_index,
                t_ms = t_ms,
                min = self.min as f64,
                max = self.max as f64,
                rms = rms as f64,
                samples = self.count as u64,
                channels = self.channels as u32,
            },
        );
        self.window_index += 1;
    }

    fn reset(&mut self) {
        self.accum = 0;
        self.min = f32::INFINITY;
        self.max = f32::NEG_INFINITY;
        self.sum_sq = 0.0;
        self.count = 0;
    }
}

fn print_device(d: &InputDevice, default_name: Option<&str>) {
    println!();
    println!("  {}", d.name);
    println!("    transport:     {}", transport_str(d.transport));
    println!(
        "    persistent_id: {}",
        d.persistent_id.as_deref().unwrap_or("<none>")
    );
    for s in &d.streams {
        let is_default = default_name == Some(s.name.as_str());
        let marker = if is_default { " [default]" } else { "" };
        println!("    stream: {}{}", s.name, marker);
        println!("      channels:     {}", s.channels);
        println!("      sample_rates: {}", sample_rates_str(&s.sample_rates));
    }
}

fn transport_str(t: Transport) -> &'static str {
    match t {
        Transport::BuiltIn => "built-in",
        Transport::Usb => "usb",
        Transport::Bluetooth => "bluetooth",
        Transport::Virtual => "virtual",
        Transport::Unknown => "unknown",
    }
}

fn sample_rates_str(s: &SampleRateSupport) -> String {
    match s {
        SampleRateSupport::List(rates) => rates
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        SampleRateSupport::Ranges(ranges) => ranges
            .iter()
            .map(|(lo, hi)| {
                if lo == hi {
                    lo.to_string()
                } else {
                    format!("{lo}-{hi}")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        SampleRateSupport::ProbeOnly => "probe-only".to_string(),
    }
}
