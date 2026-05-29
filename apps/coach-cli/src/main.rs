//! coach-cli entry point.
//!
//! The host's job is wiring: build peripheral adapters, dispatch on
//! the subcommand. Real product behaviour lives in `adapter-app-coach`
//! and the other adapters.

use clap::{Parser, Subcommand};
use domain_ports::audio_capture::{AudioCapture, CaptureConfig, CaptureFrame};
use domain_ports::audio_devices::{
    AudioDevices, DeviceId, InputDevice, SampleRateSupport, Transport,
};
use domain_ports::clock::Clock;
use domain_ports::telemetry::{Level, Telemetry};
use domain_ports::{fields, tel_info, tel_warn};
use std::sync::Arc;
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
    /// Open an input device and log per-callback stats.
    Capture {
        /// Duration to capture, in milliseconds.
        #[arg(long, default_value_t = 3000)]
        duration_ms: u64,
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
            persistent_id,
        } => capture(duration_ms, persistent_id),
    }
}

fn run_coach() {
    // TODO(PR 19): rewire via AppCoach::send_command + poll_events.
    // PRs 17-18 land the new boundary + implementation; this host body
    // is rewritten in PR 19. Until then, keep the trivial boot/log/
    // shutdown behaviour inline so the workspace stays green.
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));
    use domain_ports::telemetry::Event;
    telemetry.event(&Event::Boot {
        app_version: env!("CARGO_PKG_VERSION"),
    });
    let boot_ms = clock.now_ms();
    tel_info!(&*telemetry, "gurukul: hello", t_ms = clock.now_ms());
    telemetry.event(&Event::Shutdown {
        uptime_ms: clock.now_ms().saturating_sub(boot_ms),
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

fn capture(duration_ms: u64, persistent_id: Option<String>) {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));

    let devices_port = adapter_audio_cpal::new_devices();
    let chosen = match persistent_id.as_ref().map(|s| DeviceId(s.clone())) {
        Some(pid) => devices_port
            .list_devices()
            .into_iter()
            .find(|d| d.persistent_id.as_ref() == Some(&pid))
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
    );

    let telemetry_for_cb = Arc::clone(&telemetry);

    let capture_port = adapter_audio_cpal::new_capture(Arc::clone(&clock));
    let cfg = CaptureConfig {
        sample_rate,
        channels,
        buffer_frames: Some(sample_rate / 100),
    };
    let session = match capture_port.open(
        stream_info.handle.clone(),
        cfg,
        Box::new(move |frame: CaptureFrame<'_>| {
            let (min, max, sum_sq) = frame.samples.iter().fold(
                (f32::INFINITY, f32::NEG_INFINITY, 0.0_f64),
                |(mn, mx, ss), &s| (mn.min(s), mx.max(s), ss + (s as f64) * (s as f64)),
            );
            let vpp = max - min;
            let mid = (max + min) * 0.5;
            let rms = if frame.samples.is_empty() {
                0.0
            } else {
                (sum_sq / frame.samples.len() as f64).sqrt() as f32
            };
            telemetry_for_cb.log(
                Level::Debug,
                "capture frame",
                &fields! {
                    t_ms = frame.t_ms,
                    frames = frame.frames as u64,
                    vpp = format!("{vpp:.4}"),
                    mid = format!("{mid:+.4}"),
                    rms = format!("{rms:.4}"),
                },
            );
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

fn print_device(d: &InputDevice, default_name: Option<&str>) {
    println!();
    println!("  {}", d.name);
    println!("    transport:     {}", transport_str(d.transport));
    match &d.persistent_id {
        Some(id) => println!("    persistent_id: {id}"),
        None => println!("    persistent_id: <none>"),
    }
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
