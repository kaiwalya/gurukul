//! coach-cli entry point.
//!
//! The CLI is a *head*: a thin shell that wires peripheral adapters
//! into an [`AppCoach`], translates subcommands into [`Command`]s,
//! drains [`CoachEvent`]s, and prints. Real product behaviour
//! (state machine, session lifecycle, telemetry) lives in
//! `adapter-app-coach`.
//!
//! See `docs/SPEC-AppCoach.md` for the boundary contract.

use clap::{Parser, Subcommand};
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, CoachEvent, Command, PitchReading, SessionConfig, SessionState,
    ShutdownResult,
};
use domain_ports::audio_devices::{DeviceId, InputDevice, SampleRateSupport};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::io::IsTerminal;
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
    /// Open the mic and print the live pitch — sing, hum, whistle.
    /// One line per fresh f0 estimate (~85 Hz at 48k/hop=512); `--`
    /// shown for unvoiced frames (silence, breath, noise).
    Freestyle {
        /// Duration of the session, in milliseconds.
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
        Subcmd::Freestyle {
            duration_ms,
            persistent_id,
        } => freestyle(duration_ms, persistent_id),
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

fn freestyle(duration_ms: u64, persistent_id: Option<String>) {
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
            eprintln!("freestyle: session error: {kind:?}: {reason}");
            shutdown(&coach);
            return;
        }
        None => {
            eprintln!("freestyle: timed out waiting for Running");
            shutdown(&coach);
            return;
        }
    }

    run_pitch_loop(&coach, Duration::from_millis(duration_ms));

    coach.send_command(Command::StopSession);
    let _ = wait_for(&coach, Duration::from_secs(2), |ev| match ev {
        CoachEvent::SessionStateChanged {
            new_state: SessionState::Idle,
        } => Some(()),
        _ => None,
    });

    shutdown(&coach);
}

/// Poll `latest_pitch()` at the head's frame cadence and print one line
/// per fresh reading. Deduplicates by `t_ms` so the printed rate
/// matches the publisher rate (~85Hz at 48k/hop=512), not the polling
/// rate. Unvoiced frames (f0 == 0.0) render as `--`.
fn run_pitch_loop(coach: &impl AppCoach, duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut last_t: u64 = u64::MAX;
    // TTY check is captured once so colour stays consistent across the
    // whole session — and pipes/redirects stay clean.
    let use_color = std::io::stdout().is_terminal();
    while Instant::now() < deadline {
        if let Some(reading) = coach.latest_pitch() {
            if reading.t_ms != last_t {
                last_t = reading.t_ms;
                print_pitch(&reading, use_color);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn print_pitch(r: &PitchReading, use_color: bool) {
    if r.f0_hz <= 0.0 {
        println!("[{:>10} ms]  --", r.t_ms);
        return;
    }
    let (note, cents) = note_and_cents(r.f0_hz);
    let cents_str = format!("{cents:+5}");
    let painted = if use_color {
        paint_cents(&cents_str, cents)
    } else {
        cents_str
    };
    println!(
        "[{:>10} ms]  {:>10.2} Hz  {:>4}  {} cents",
        r.t_ms, r.f0_hz, note, painted
    );
}

/// ANSI-colour the cents column by tuning band. Matches the mac app's
/// thresholds: ≤5 = green (in tune), ≤20 = default (close enough),
/// >20 = yellow (sharp/flat enough to act on).
fn paint_cents(s: &str, cents: i32) -> String {
    let mag = cents.unsigned_abs();
    if mag <= 5 {
        format!("\x1b[32m{s}\x1b[0m")
    } else if mag <= 20 {
        s.to_string()
    } else {
        format!("\x1b[33m{s}\x1b[0m")
    }
}

/// Convert a frequency in Hz to the nearest equal-temperament note name
/// plus cents offset, using A4 = 440 Hz. Returns ("A4", 0) for an exact
/// 440 Hz input.
fn note_and_cents(f0_hz: f32) -> (String, i32) {
    const A4_HZ: f32 = 440.0;
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    // Semitones from A4. A4 is index 9 within octave 4.
    let semis_from_a4 = 12.0 * (f0_hz / A4_HZ).log2();
    let nearest = semis_from_a4.round() as i32;
    let cents = ((semis_from_a4 - nearest as f32) * 100.0).round() as i32;
    // MIDI note number, with A4 = 69.
    let midi = 69 + nearest;
    let name_idx = midi.rem_euclid(12) as usize;
    let octave = midi.div_euclid(12) - 1;
    (format!("{}{}", NAMES[name_idx], octave), cents)
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
