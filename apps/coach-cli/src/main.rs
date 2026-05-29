//! coach-cli entry point.
//!
//! The CLI is a *head*: a thin shell that wires peripheral adapters
//! into an [`AppCoach`], translates subcommands into [`Command`]s,
//! drains [`CoachEvent`]s, and prints. Real product behaviour
//! (state machine, capture lifecycle, telemetry) lives in
//! `adapter-app-coach`.
//!
//! See `docs/SPEC-AppCoach.md` for the boundary contract.

use clap::{Parser, Subcommand};
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, CoachEvent, Command, SessionConfig, SessionState, ShutdownResult,
};
use domain_ports::audio_devices::{DeviceId, InputDevice, SampleRateSupport};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "coach-cli", version, about = "gurukul singing-coach CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcmd>,
}

#[derive(Subcommand)]
enum Subcmd {
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
    match cli.command.unwrap_or(Subcmd::Run) {
        Subcmd::Run => run_coach(),
        Subcmd::ListDevices => list_devices(),
        Subcmd::Capture {
            duration_ms,
            persistent_id,
        } => capture(duration_ms, persistent_id),
    }
}

/// Default head loop sleep between event drains. 50ms = 20Hz,
/// comfortably above the spec's 10Hz floor.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn build_coach() -> impl AppCoach {
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let telemetry: Arc<dyn Telemetry> = Arc::new(adapter_telemetry_std::new(Arc::clone(&clock)));
    let audio_devices = Arc::new(adapter_audio_cpal::new_devices());
    let audio_capture = Arc::new(adapter_audio_cpal::new_capture(Arc::clone(&clock)));

    adapter_app_coach::new(AppCoachDeps {
        clock,
        telemetry,
        audio_devices,
        audio_capture,
        host_version: env!("CARGO_PKG_VERSION"),
    })
}

fn run_coach() {
    // No subcommand: just boot the coach and immediately shut it down,
    // so telemetry emits the lifecycle events. The interactive REPL is
    // a future PR.
    let coach = build_coach();
    let _ = coach.shutdown(SHUTDOWN_TIMEOUT);
}

fn list_devices() {
    let coach = build_coach();
    coach.send_command(Command::ListDevices);

    let devices = match wait_for(
        &coach,
        Duration::from_secs(2),
        |ev| -> Option<Vec<InputDevice>> {
            match ev {
                CoachEvent::DevicesListed { devices } => Some(devices.clone()),
                _ => None,
            }
        },
    ) {
        Some(d) => d,
        None => {
            eprintln!("list-devices: timed out waiting for DevicesListed");
            shutdown(&coach);
            return;
        }
    };

    print_device_list(&devices);
    shutdown(&coach);
}

fn capture(duration_ms: u64, persistent_id: Option<String>) {
    let coach = build_coach();

    let cfg = SessionConfig {
        device_id: persistent_id.map(DeviceId),
        sample_rate: None,
        buffer_frames: None,
    };
    coach.send_command(Command::StartSession(cfg));

    // Wait for Running, or an error.
    let started = wait_for(&coach, Duration::from_secs(2), |ev| match ev {
        CoachEvent::SessionStateChanged {
            new_state: SessionState::Running,
        } => Some(Ok(())),
        CoachEvent::SessionError { kind, reason } => Some(Err((*kind, reason.clone()))),
        _ => None,
    });

    match started {
        Some(Ok(())) => {}
        Some(Err((kind, reason))) => {
            eprintln!("capture: session error: {kind:?}: {reason}");
            shutdown(&coach);
            return;
        }
        None => {
            eprintln!("capture: timed out waiting for Running");
            shutdown(&coach);
            return;
        }
    }

    // Capture runs in the coach; the head just waits.
    thread::sleep(Duration::from_millis(duration_ms));

    coach.send_command(Command::StopSession);
    let _ = wait_for(&coach, Duration::from_secs(2), |ev| match ev {
        CoachEvent::SessionStateChanged {
            new_state: SessionState::Idle,
        } => Some(()),
        _ => None,
    });

    shutdown(&coach);
}

/// Drain events until `pred` returns `Some(T)` or `timeout` elapses.
///
/// Events that don't match the predicate are dropped (the CLI head
/// doesn't need them — it's a one-shot script, not a long-running
/// state renderer). A future head with a UI would route everything
/// through a state model instead.
fn wait_for<T>(
    coach: &impl AppCoach,
    timeout: Duration,
    mut pred: impl FnMut(&CoachEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        coach.poll_events(&mut buf);
        for ev in &buf {
            if let Some(t) = pred(ev) {
                return Some(t);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn shutdown(coach: &impl AppCoach) {
    match coach.shutdown(SHUTDOWN_TIMEOUT) {
        ShutdownResult::Clean | ShutdownResult::AlreadyShutDown => {}
        ShutdownResult::TimedOut => {
            eprintln!("coach-cli: shutdown timed out after {SHUTDOWN_TIMEOUT:?}");
        }
    }
}

// ---------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------

fn print_device_list(devices: &[InputDevice]) {
    if devices.is_empty() {
        println!("No input devices found.");
        return;
    }
    println!("Input devices ({}):", devices.len());
    for d in devices {
        print_device(d);
    }
}

fn print_device(d: &InputDevice) {
    println!();
    println!("  {}", d.name);
    println!("    transport:     {}", transport_str(d.transport));
    match &d.persistent_id {
        Some(id) => println!("    persistent_id: {id}"),
        None => println!("    persistent_id: <none>"),
    }
    for s in &d.streams {
        println!("    stream: {}", s.name);
        println!("      channels:     {}", s.channels);
        println!("      sample_rates: {}", sample_rates_str(&s.sample_rates));
    }
}

fn transport_str(t: domain_ports::audio_devices::Transport) -> &'static str {
    use domain_ports::audio_devices::Transport;
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
