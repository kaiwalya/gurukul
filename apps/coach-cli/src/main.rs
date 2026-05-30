//! coach-cli entry point.
//!
//! The CLI is a *head*: a thin shell that wires peripheral adapters
//! into an [`AppCoach`], translates subcommands into [`Command`]s,
//! drains [`CoachEvent`]s, and prints. Real product behaviour
//! (state machine, session lifecycle, telemetry) lives in
//! `adapter-app-coach`.
//!
//! See `docs/SPEC-AppCoach.md` for the boundary contract.

mod tui;

use clap::{Parser, Subcommand};
use domain_ports::app_coach::{
    AppCoach, AppCoachDeps, CoachEvent, Command, FeatureSnapshot, SessionConfig, SessionState,
    ShutdownResult,
};
use domain_ports::audio_devices::{DeviceId, InputDevice, SampleRateSupport};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Open the mic and run the coach pipeline. Default when no
    /// subcommand is given. In an interactive terminal this brings up
    /// the TUI shell (`~` toggles a console pane showing live coach
    /// logs; `q` / `Esc` / Ctrl-C exits). When stdout is piped or
    /// redirected — or `--no-tui` is passed — it streams one line per
    /// fresh f0 estimate (~85 Hz at 48k/hop=512); `--` shown for
    /// unvoiced frames (silence, breath, noise).
    Run {
        /// Force the line-streaming path even in an interactive
        /// terminal. Useful when piping into `less -R` or grepping
        /// live output in a real TTY.
        #[arg(long)]
        no_tui: bool,
        /// Duration of the session, in milliseconds. Omit to run until
        /// Ctrl-C. Honoured in both TUI and line-streaming modes.
        #[arg(long)]
        duration_ms: Option<u64>,
        /// Persistent id (from `list-devices`) of the device to
        /// open. Default: system multimedia-role default input.
        #[arg(long)]
        persistent_id: Option<String>,
    },
    /// Enumerate audio input devices and print a summary.
    ListDevices,
}

fn main() {
    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or(Subcmd::Run {
        no_tui: false,
        duration_ms: None,
        persistent_id: None,
    });
    match cmd {
        Subcmd::Run {
            no_tui,
            duration_ms,
            persistent_id,
        } => run(no_tui, duration_ms, persistent_id),
        Subcmd::ListDevices => list_devices(),
    }
}

/// Default head loop sleep between event drains. 50ms = 20Hz,
/// comfortably above the spec's 10Hz floor.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the coach with the stderr telemetry adapter — used by
/// short-lived non-TUI commands (`list-devices`, plain mode) where
/// stderr is fair game. The TUI path builds the coach inline so it
/// can also hold the [`LogBuffer`] handle.
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

/// Pick the head based on whether stdout is a real terminal. Piped or
/// redirected output gets the line-streaming path; an interactive TTY
/// gets the TUI shell. `--no-tui` forces the plain path even in a TTY
/// — useful for `coach-cli | less -R` and the like.
fn run(no_tui: bool, duration_ms: Option<u64>, persistent_id: Option<String>) {
    let use_tui = !no_tui && std::io::stdout().is_terminal();
    if use_tui {
        run_tui(duration_ms, persistent_id);
    } else {
        run_plain(duration_ms, persistent_id);
    }
}

fn run_tui(duration_ms: Option<u64>, persistent_id: Option<String>) {
    // The TUI owns stdout via the alternate screen — anything that
    // bypasses Telemetry (cpal warnings, panics) will be invisible
    // until we leave. Acceptable v1 tradeoff. duration_ms is wired
    // through so a `--duration-ms N` invocation auto-exits even in
    // the shell; without it the user quits via q/Esc/Ctrl-C.
    let clock: Arc<dyn Clock> = Arc::new(adapter_clock_std::new());
    let (tui_telemetry, log_buffer) = adapter_telemetry_tui::new(Arc::clone(&clock), 2048);
    let telemetry: Arc<dyn Telemetry> = Arc::new(tui_telemetry);
    let audio_devices = Arc::new(adapter_audio_cpal::new_devices());
    let audio_capture = Arc::new(adapter_audio_cpal::new_capture(Arc::clone(&clock)));

    let coach = adapter_app_coach::new(AppCoachDeps {
        clock,
        telemetry,
        audio_devices,
        audio_capture,
        host_version: env!("CARGO_PKG_VERSION"),
    });

    coach.send_command(Command::StartSession(SessionConfig {
        device_id: persistent_id.map(DeviceId),
        sample_rate: None,
        buffer_frames: None,
    }));

    let deadline = duration_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    if let Err(e) = tui::run(log_buffer, deadline) {
        eprintln!("coach-cli: tui error: {e}");
    }

    coach.send_command(Command::StopSession);
    let _ = coach.shutdown(SHUTDOWN_TIMEOUT);
}

fn run_plain(duration_ms: Option<u64>, persistent_id: Option<String>) {
    // Install a Ctrl-C handler before we open the mic. The handler
    // flips an atomic that the pitch loop polls — this keeps shutdown
    // graceful (the loop falls out, we StopSession, then shutdown the
    // coach) instead of leaving an open cpal stream when the process
    // exits.
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_for_handler = Arc::clone(&interrupted);
    if let Err(e) = ctrlc::set_handler(move || {
        interrupted_for_handler.store(true, Ordering::Release);
    }) {
        eprintln!("coach-cli: failed to install Ctrl-C handler: {e}");
    }

    let coach = build_coach();

    let cfg = SessionConfig {
        device_id: persistent_id.map(DeviceId),
        sample_rate: None,
        buffer_frames: None,
    };
    coach.send_command(Command::StartSession(cfg));

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
            eprintln!("coach-cli: session error: {kind:?}: {reason}");
            shutdown(&coach);
            return;
        }
        None => {
            eprintln!("coach-cli: timed out waiting for Running");
            shutdown(&coach);
            return;
        }
    }

    run_pitch_loop(&coach, duration_ms.map(Duration::from_millis), &interrupted);

    coach.send_command(Command::StopSession);
    let _ = wait_for(&coach, Duration::from_secs(2), |ev| match ev {
        CoachEvent::SessionStateChanged {
            new_state: SessionState::Idle,
        } => Some(()),
        _ => None,
    });

    shutdown(&coach);
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

/// Poll `latest_features()` at the head's frame cadence and print one
/// line per fresh snapshot. Deduplicates by `t_ms` so the printed rate
/// matches the publisher rate (~85Hz at 48k/hop=512), not the polling
/// rate. Unvoiced frames (f0 == 0.0) render as `--`.
///
/// `duration = None` runs until `interrupted` flips (Ctrl-C).
fn run_pitch_loop(coach: &impl AppCoach, duration: Option<Duration>, interrupted: &AtomicBool) {
    let deadline = duration.map(|d| Instant::now() + d);
    let mut last_t: u64 = u64::MAX;
    // TTY check is captured once so colour stays consistent across the
    // whole session — and pipes/redirects stay clean.
    let use_color = std::io::stdout().is_terminal();
    loop {
        if interrupted.load(Ordering::Acquire) {
            break;
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        if let Some(snap) = coach.latest_features() {
            if snap.t_ms != last_t {
                last_t = snap.t_ms;
                print_features(&snap, use_color);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn print_features(s: &FeatureSnapshot, use_color: bool) {
    // Onset/breath/vibrato render unconditionally — they have their own
    // 0.0-when-inactive convention and aren't gated on voicedness.
    let onset_marker = if s.onset > 0.0 { "•" } else { " " };
    let breath_str = format!("br {:.2}", s.breath);
    let vib_str = if s.vibrato_rate > 0.0 {
        format!("vib {:.1}Hz/{:.2}st", s.vibrato_rate, s.vibrato_depth)
    } else {
        "vib --".to_string()
    };

    if s.f0_hz <= 0.0 {
        println!(
            "[{:>10} ms]  {}  --                       {}  {}",
            s.t_ms, onset_marker, breath_str, vib_str
        );
        return;
    }
    let (note, cents) = note_and_cents(s.f0_hz);
    let cents_str = format!("{cents:+5}");
    let painted = if use_color {
        paint_cents(&cents_str, cents)
    } else {
        cents_str
    };
    println!(
        "[{:>10} ms]  {}  {:>10.2} Hz  {:>4}  {} cents  {}  {}",
        s.t_ms, onset_marker, s.f0_hz, note, painted, breath_str, vib_str
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
