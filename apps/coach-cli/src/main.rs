//! coach-cli entry point.
//!
//! The host's job is wiring: build peripheral adapters, dispatch on
//! the subcommand. Real product behaviour lives in `adapter-app-coach`
//! and the other adapters.

use clap::{Parser, Subcommand};
use domain_ports::app_coach::{AppCoach, AppCoachDeps};
use domain_ports::audio_devices::{AudioDevices, InputDevice, SampleRateSupport, Transport};
use domain_ports::clock::Clock;
use domain_ports::telemetry::Telemetry;
use std::sync::Arc;

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
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run_coach(),
        Command::ListDevices => list_devices(),
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
    let devices_port = adapter_audio_cpal::new();
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
