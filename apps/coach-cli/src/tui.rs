//! The interactive TUI shell.
//!
//! Owns the alternate screen + raw mode for as long as it runs. The
//! main canvas is intentionally blank — this is the skeleton that
//! future feature panes (waveform, pitch trace, level meter) will
//! land on. Today it offers exactly one affordance:
//!
//! - `~` toggles a console pane at the bottom that shows the live
//!   tail of coach Telemetry. Drained from the [`LogBuffer`] the
//!   tui-backed Telemetry adapter writes into.
//!
//! Exit: `q`, `Esc`, or Ctrl-C.
//!
//! # Why a single owned thread
//!
//! ratatui owns stdout; the AppCoach owns its own threads. The shell
//! just sits between them: render → poll input → drain log buffer →
//! sleep. No async, no extra threads, no contention with the
//! control/data planes.

use adapter_telemetry_tui::LogBuffer;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use domain_ports::app_coach::{AppCoach, FeatureSnapshot};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// Poll cadence for crossterm input. 16ms ≈ 60Hz — a comfortable
/// frame budget; the log pane redraws on every tick, so the lag
/// between a log line being pushed and showing up is at most ~16ms.
const TICK: Duration = Duration::from_millis(16);

/// Capacity of the feature ring. The data plane publishes at ~85Hz,
/// the chart shows the last 5s = ~425 frames; 512 gives headroom for
/// a slightly higher sample rate / shorter hop without resizing.
const FEATURE_RING_CAP: usize = 512;

/// X-axis window length, in milliseconds. The chart shows `[-WIN, 0]`
/// seconds; the right edge is "now" (the freshest snapshot's `t_ms`).
const CHART_WINDOW_MS: u64 = 5_000;

/// Reference frequency for semitone conversion. Standard tuning.
const A4_HZ: f32 = 440.0;

/// Initial Y window when no voiced data has arrived yet, in semitones
/// from A4. Spans roughly A2 (-24) to A5 (+12) — covers most voices
/// with margin. Octave-aligned (multiples of 12) so it doesn't shift
/// when the first re-fit happens.
const Y_INITIAL_LO: f32 = -24.0;
const Y_INITIAL_HI: f32 = 12.0;

/// Re-fit the Y window only when ≥`HYSTERESIS`-fraction of the current
/// window's voiced data has drifted outside it. 0.20 = "20% of the
/// span exits before we resnap." Keeps the chart stable when a singer
/// holds a note near the edge of the window.
const Y_HYSTERESIS: f32 = 0.20;

/// Run the shell until the user quits, or `deadline` elapses.
/// `deadline = None` means run until the user quits.
pub fn run(
    coach: &impl AppCoach,
    logs: LogBuffer,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let mut terminal = setup()?;
    let mut state = State::default();

    let outcome = event_loop(&mut terminal, &mut state, coach, &logs, deadline);

    teardown(&mut terminal)?;
    outcome
}

struct State {
    console_open: bool,
    /// Ring of recent feature snapshots, oldest at the front. Frame
    /// loop pushes one per fresh `t_ms` from `coach.latest_features()`;
    /// the chart, header, and onset-tick widgets all read from here.
    features: VecDeque<FeatureSnapshot>,
    /// Last `t_ms` we pushed, so we don't double-count snapshots when
    /// the data plane hasn't produced a new one between frames.
    last_t_ms: Option<u64>,
    /// Persistent Y window for the pitch chart, in semitones-from-A4.
    /// Stays at `(Y_INITIAL_LO, Y_INITIAL_HI)` until enough voiced
    /// data drifts outside it; then snaps to the nearest octave
    /// bracket. Kept in state across frames so the hysteresis check
    /// can compare against the current window.
    y_lo: f32,
    y_hi: f32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            console_open: false,
            features: VecDeque::new(),
            last_t_ms: None,
            y_lo: Y_INITIAL_LO,
            y_hi: Y_INITIAL_HI,
        }
    }
}

impl State {
    fn ingest(&mut self, snap: FeatureSnapshot) {
        if Some(snap.t_ms) == self.last_t_ms {
            return;
        }
        self.last_t_ms = Some(snap.t_ms);
        if self.features.len() == FEATURE_RING_CAP {
            self.features.pop_front();
        }
        self.features.push_back(snap);
    }

    /// Recompute the Y window if recent voiced data has drifted out
    /// of it by more than the hysteresis threshold. The chart calls
    /// this once per frame, before plotting.
    fn refit_y(&mut self) {
        let (Some(lo), Some(hi)) = voiced_semitone_range(&self.features) else {
            return; // No voiced data yet — keep the initial window.
        };
        let span = self.y_hi - self.y_lo;
        let slack = span * Y_HYSTERESIS;
        let inside = lo >= self.y_lo - slack && hi <= self.y_hi + slack;
        if inside {
            return;
        }
        // Snap the new window to the nearest octave bracket around
        // (lo, hi), with one octave of padding on each side. Octave
        // snapping = floor/ceil to multiples of 12 semitones.
        let new_lo = ((lo / 12.0).floor() * 12.0) - 12.0;
        let new_hi = ((hi / 12.0).ceil() * 12.0) + 12.0;
        self.y_lo = new_lo;
        self.y_hi = new_hi;
    }
}

/// Convert f0 in Hz to semitones above/below A4. Used as the chart's
/// Y coordinate — equal vertical spacing per semitone matches how the
/// ear perceives pitch.
fn hz_to_semitones(hz: f32) -> f32 {
    12.0 * (hz / A4_HZ).log2()
}

/// Find the (min, max) semitone range across voiced frames currently
/// in the ring. Returns `None` if no voiced frame is present.
fn voiced_semitone_range(features: &VecDeque<FeatureSnapshot>) -> (Option<f32>, Option<f32>) {
    let mut lo: Option<f32> = None;
    let mut hi: Option<f32> = None;
    for s in features {
        if s.f0_hz <= 0.0 {
            continue;
        }
        let st = hz_to_semitones(s.f0_hz);
        lo = Some(lo.map_or(st, |v| v.min(st)));
        hi = Some(hi.map_or(st, |v| v.max(st)));
    }
    (lo, hi)
}

fn setup() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn teardown(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut State,
    coach: &impl AppCoach,
    logs: &LogBuffer,
    deadline: Option<Instant>,
) -> io::Result<()> {
    loop {
        // Ingest before drawing so the freshest snapshot lands in this
        // frame instead of the next one.
        if let Some(snap) = coach.latest_features() {
            state.ingest(snap);
        }

        terminal.draw(|f| draw(f, state, logs))?;
        // `draw` borrows state mutably, the rest of this iteration
        // doesn't — borrow ends at the closure return.

        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(());
            }
        }

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if should_quit(key.code, key.modifiers) {
                        return Ok(());
                    }
                    if key.code == KeyCode::Char('~') {
                        state.console_open = !state.console_open;
                    }
                }
                _ => {}
            }
        }
    }
}

fn should_quit(code: KeyCode, mods: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
}

fn draw(f: &mut Frame, state: &mut State, logs: &LogBuffer) {
    state.refit_y();
    let area = f.area();
    if state.console_open {
        let [main, console] = split_with_console(area);
        draw_main(f, main, state);
        draw_console(f, console, logs);
    } else {
        draw_main(f, area, state);
    }
}

fn split_with_console(area: Rect) -> [Rect; 2] {
    // Console takes the bottom 40% — enough rows to actually read,
    // not so many it eats the canvas. Capped at 20 rows so it stays
    // sane on tall terminals.
    let console_rows = (area.height as u32 * 40 / 100).clamp(6, 20) as u16;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(console_rows)])
        .split(area);
    [layout[0], layout[1]]
}

fn draw_main(f: &mut Frame, area: Rect, state: &State) {
    // Outer frame; chart and hint sit inside.
    let outer = Block::default().borders(Borders::ALL).title(" gurukul ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    // Split inner: chart fills the rest, footer hint takes the bottom
    // row. One-row hint is the seated-posture compromise — the chart
    // gets the real estate, the hint stays visible.
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let chart_area = layout[0];
    let hint_area = layout[1];

    draw_chart(f, chart_area, state);
    draw_hint(f, hint_area);
}

fn draw_hint(f: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::styled("~", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" console   "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(hint), area);
}

fn draw_chart(f: &mut Frame, area: Rect, state: &State) {
    // X axis: seconds-ago from "now" (the freshest snapshot's t_ms).
    // Using the data plane's clock instead of wall time keeps the
    // right edge of the chart pinned to the most recent datapoint —
    // no drift if the data plane stalls.
    let now_ms = match state.features.back().map(|s| s.t_ms) {
        Some(t) => t,
        None => {
            // No data yet — render an empty chart with the axis
            // skeleton so the singer sees something to sing into.
            render_empty_chart(f, area, state);
            return;
        }
    };

    // Build voiced segments (each a contiguous run of voiced frames).
    // ratatui's Dataset::data takes a borrowed slice, so the Vec<Vec>
    // must live until f.render_widget returns — both Vecs live in
    // this stack frame.
    let segments = build_voiced_segments(&state.features, now_ms);

    let datasets: Vec<Dataset> = segments
        .iter()
        .map(|seg| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(seg)
        })
        .collect();

    let y_labels = octave_labels(state.y_lo, state.y_hi);
    let x_labels = vec![
        Span::raw("-5s"),
        Span::raw("-4s"),
        Span::raw("-3s"),
        Span::raw("-2s"),
        Span::raw("-1s"),
        Span::raw("0"),
    ];

    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(" pitch "))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-(CHART_WINDOW_MS as f64) / 1000.0, 0.0])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([state.y_lo as f64, state.y_hi as f64])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

fn render_empty_chart(f: &mut Frame, area: Rect, state: &State) {
    // Same chart shell as the populated path, just no datasets. Gives
    // the singer an empty staff to sing into; matches the populated
    // layout so the appearance doesn't jump when data starts arriving.
    let y_labels = octave_labels(state.y_lo, state.y_hi);
    let x_labels = vec![
        Span::raw("-5s"),
        Span::raw("-4s"),
        Span::raw("-3s"),
        Span::raw("-2s"),
        Span::raw("-1s"),
        Span::raw("0"),
    ];
    let empty: Vec<Dataset> = Vec::new();
    let chart = Chart::new(empty)
        .block(Block::default().borders(Borders::ALL).title(" pitch "))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-(CHART_WINDOW_MS as f64) / 1000.0, 0.0])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([state.y_lo as f64, state.y_hi as f64])
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

/// Split the ring into contiguous voiced segments, each translated
/// into chart coordinates `(seconds_ago, semitones)`. Unvoiced frames
/// terminate the current segment; the next voiced frame starts a new
/// one — this is what produces a visual "gap" in the trace.
fn build_voiced_segments(
    features: &VecDeque<FeatureSnapshot>,
    now_ms: u64,
) -> Vec<Vec<(f64, f64)>> {
    let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for s in features {
        if s.f0_hz <= 0.0 {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        let x = -((now_ms.saturating_sub(s.t_ms)) as f64) / 1000.0;
        // Drop points older than the visible window — keeps the chart
        // bounds honest even though ratatui would clip anyway.
        if x < -(CHART_WINDOW_MS as f64) / 1000.0 {
            continue;
        }
        let y = hz_to_semitones(s.f0_hz) as f64;
        current.push((x, y));
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Generate Y-axis labels at each octave boundary inside the current
/// window. Labels are note names with octave (e.g. `A3`, `A4`, `A5`)
/// — they sit on the A's because the semitone axis is anchored at A4.
fn octave_labels(y_lo: f32, y_hi: f32) -> Vec<Span<'static>> {
    let lo_oct = (y_lo / 12.0).ceil() as i32;
    let hi_oct = (y_hi / 12.0).floor() as i32;
    (lo_oct..=hi_oct)
        .map(|o| {
            // A4 = octave 0 in our semitone-from-A4 system.
            let octave_number = 4 + o;
            Span::raw(format!("A{octave_number}"))
        })
        .collect()
}

fn draw_console(f: &mut Frame, area: Rect, logs: &LogBuffer) {
    let lines = logs.snapshot();
    // Show only the tail that fits the inner height. Cheap arithmetic
    // beats a scroll model for v1 — the head can't scroll back yet,
    // they see the live tail.
    let inner_h = area.height.saturating_sub(2) as usize;
    let start = lines.len().saturating_sub(inner_h);
    let visible: Vec<Line> = lines[start..]
        .iter()
        .map(|s| Line::from(colourise(s)))
        .collect();

    let para = Paragraph::new(visible)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" console (~ to close) "),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

/// Tint the `[LEVEL]` prefix so warns and errors pop. Everything else
/// renders in the terminal's default colour.
fn colourise(line: &str) -> Vec<Span<'_>> {
    let (prefix, rest) = match line.find(']') {
        Some(idx) => line.split_at(idx + 1),
        None => return vec![Span::raw(line)],
    };
    let style = match prefix {
        "[ERROR]" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "[WARN]" => Style::default().fg(Color::Yellow),
        "[INFO]" => Style::default().fg(Color::Cyan),
        "[DEBUG]" => Style::default().fg(Color::DarkGray),
        "[TRACE]" => Style::default().fg(Color::DarkGray),
        "[EVENT]" => Style::default().fg(Color::Magenta),
        _ => Style::default(),
    };
    vec![
        Span::styled(prefix.to_string(), style),
        Span::raw(rest.to_string()),
    ]
}
