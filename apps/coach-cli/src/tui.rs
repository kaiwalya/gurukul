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
use tui_big_text::{BigText, PixelSize};

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

/// Upper-bound sample rate for sizing the audio ring. The cpal adapter
/// negotiates 48k by preference and that's the highest we'd plausibly
/// see for voice input; the ring is sized for this so a 44.1k session
/// just runs with a touch of headroom.
const AUDIO_SR_HZ_MAX: usize = 48_000;

/// Fallback sample rate used by the scope/envelope renderers before a
/// session is running (i.e. when [`AppCoach::session_info`] returns
/// `None`). The audio ring is empty then anyway, so this only affects
/// axis labels on the empty placeholder.
const AUDIO_SR_HZ_FALLBACK: u32 = 48_000;

/// Scope window length in milliseconds. Long enough to see a few
/// cycles of a male fundamental (~80Hz → 12.5ms/cycle, so 4 cycles)
/// without the trace becoming a smear.
const SCOPE_WIN_MS: u64 = 50;

/// Rolling RMS window length, in milliseconds. ~10ms → smooth enough
/// to read but still tracks attack/release within a phrase.
const ENVELOPE_WIN_MS: u64 = 10;

/// Envelope step in milliseconds — one RMS frame every 10ms → 100Hz
/// envelope rate.
const ENVELOPE_STEP_MS: u64 = 10;

/// History window for the envelope, in milliseconds — matches the
/// pitch chart so they read together.
const ENVELOPE_WINDOW_MS: u64 = 5_000;

/// How many envelope samples we keep on hand: 5s × 100Hz + a touch of
/// headroom.
const ENVELOPE_RING_CAP: usize = 600;

/// Audio ring capacity, in samples. 5s × max SR + scope window so even
/// at the bottom of the envelope window there's still a scope-worth of
/// fresh samples behind the cursor.
const AUDIO_RING_CAP: usize = AUDIO_SR_HZ_MAX * 5 + AUDIO_SR_HZ_MAX * SCOPE_WIN_MS as usize / 1000;

/// Run the shell until the user quits, or `deadline` elapses.
/// `deadline = None` means run until the user quits.
pub fn run(coach: &impl AppCoach, logs: LogBuffer, deadline: Option<Instant>) -> io::Result<()> {
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
    /// Rolling 5s+ ring of raw mic samples. Drained from the coach via
    /// `drain_audio` each tick; the scope reads the freshest 50ms slice,
    /// the envelope re-buckets the whole ring into RMS frames.
    audio: VecDeque<f32>,
    /// Reusable scratch buffer for `drain_audio` so we don't allocate
    /// every tick.
    audio_scratch: Vec<f32>,
    /// Sample index of the *next* envelope frame's window start. Lets
    /// us emit one envelope point per `ENVELOPE_STEP_SAMPLES` of audio
    /// regardless of how lumpy `drain_audio` is between ticks.
    envelope_cursor: u64,
    /// Sample index of the oldest sample still in `audio`. Bumped as
    /// we drop from the front; paired with `envelope_cursor` to know
    /// when a new RMS window has filled.
    audio_origin: u64,
    /// Ring of recent RMS values, oldest at the front. One value per
    /// envelope step of audio. Stored as `(sample_index, rms)` so the
    /// renderer can place each point on the time axis.
    envelope: VecDeque<(u64, f32)>,
    /// Live capture sample rate, refreshed every tick from
    /// `coach.session_info()`. Falls back to [`AUDIO_SR_HZ_FALLBACK`]
    /// when no session is running.
    audio_sr_hz: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            console_open: false,
            features: VecDeque::new(),
            last_t_ms: None,
            y_lo: Y_INITIAL_LO,
            y_hi: Y_INITIAL_HI,
            audio: VecDeque::new(),
            audio_scratch: Vec::new(),
            envelope_cursor: 0,
            audio_origin: 0,
            envelope: VecDeque::new(),
            audio_sr_hz: AUDIO_SR_HZ_FALLBACK,
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

    /// Drain any raw mic samples the coach has accumulated and bake
    /// them into both the audio ring (for the scope) and the rolling
    /// envelope ring (for the slow envelope plot).
    ///
    /// Sample indices (`audio_origin`, `envelope_cursor`) are kept in
    /// terms of "samples ever seen", so dropping from the front of the
    /// ring doesn't lose alignment between audio and envelope time.
    fn ingest_audio(&mut self, coach: &impl AppCoach) {
        self.audio_scratch.clear();
        let n = coach.drain_audio(&mut self.audio_scratch);
        if n == 0 {
            return;
        }
        // Append to the back of the ring, evict from the front if we
        // overshoot the cap.
        self.audio.extend(self.audio_scratch.iter().copied());
        let overflow = self.audio.len().saturating_sub(AUDIO_RING_CAP);
        if overflow > 0 {
            self.audio.drain(..overflow);
            self.audio_origin += overflow as u64;
            // If the envelope cursor was inside the dropped prefix,
            // advance it to the new origin so we don't emit RMS over
            // a window we no longer have samples for.
            if self.envelope_cursor < self.audio_origin {
                self.envelope_cursor = self.audio_origin;
            }
        }
        self.emit_envelope_frames();
    }

    /// Walk the cursor forward, emitting one RMS value per full
    /// envelope step. Tagged with the *sample index* at the end of the
    /// window so the renderer can place each point on a time axis
    /// without depending on wall-clock or `t_ms`.
    fn emit_envelope_frames(&mut self) {
        let step_samples = (self.audio_sr_hz as u64 * ENVELOPE_STEP_MS) / 1000;
        let win_samples = (self.audio_sr_hz as u64 * ENVELOPE_WIN_MS) / 1000;
        let total_seen = self.audio_origin + self.audio.len() as u64;
        while self.envelope_cursor + step_samples <= total_seen {
            let win_end = self.envelope_cursor + step_samples;
            let win_start = win_end.saturating_sub(win_samples);
            // Map sample indices back into ring offsets.
            let ring_lo = win_start.saturating_sub(self.audio_origin) as usize;
            let ring_hi = (win_end - self.audio_origin) as usize;
            let ring_hi = ring_hi.min(self.audio.len());
            let mut sum_sq = 0.0_f32;
            let mut count = 0_usize;
            // VecDeque doesn't expose a slice directly across the wrap;
            // iterate by index — these windows are ≤480 samples so the
            // per-sample cost is negligible.
            for i in ring_lo..ring_hi {
                if let Some(&s) = self.audio.get(i) {
                    sum_sq += s * s;
                    count += 1;
                }
            }
            let rms = if count > 0 {
                (sum_sq / count as f32).sqrt()
            } else {
                0.0
            };
            if self.envelope.len() == ENVELOPE_RING_CAP {
                self.envelope.pop_front();
            }
            self.envelope.push_back((win_end, rms));
            self.envelope_cursor = win_end;
        }
    }

    /// Refresh `audio_sr_hz` from the coach. Called each tick before
    /// `ingest_audio` so envelope/scope math runs against the
    /// negotiated rate. When `session_info()` returns `None` (no
    /// session running) we leave the cached value alone so a
    /// transient drop doesn't flap the renderers.
    fn refresh_sr(&mut self, coach: &impl AppCoach) {
        if let Some(info) = coach.session_info() {
            self.audio_sr_hz = info.sample_rate;
        }
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
        // frame instead of the next one. Refresh SR first so the
        // audio ingest uses the negotiated rate.
        state.refresh_sr(coach);
        if let Some(snap) = coach.latest_features() {
            state.ingest(snap);
        }
        state.ingest_audio(coach);

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

    // Split inner with responsive constraints:
    //  - readout band: 5 rows normally; collapses to 1-row plain text
    //    when the terminal is too short (height < 20) to fit the
    //    BigText glyphs comfortably alongside chart + diag + hint.
    //  - canvas: chart on the right, scope+envelope split vertically
    //    on the left. The whole canvas drops to chart-only on narrow
    //    terminals (width < 100) — scope needs real width to be
    //    legible.
    //  - diag (breath + vibrato): dropped entirely on narrow
    //    terminals (width < 80) where it would overflow or crowd the
    //    chart. Hint stays — it's the only quit affordance.
    let compact_readout = inner.height < 20;
    let show_diag = inner.width >= 80;
    let show_scope = inner.width >= 100;
    let readout_rows = if compact_readout { 1 } else { 5 };

    let mut constraints: Vec<Constraint> =
        vec![Constraint::Length(readout_rows), Constraint::Min(0)];
    if show_diag {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let readout_area = layout[0];
    let canvas_area = layout[1];
    let (diag_area, hint_area) = if show_diag {
        (Some(layout[2]), layout[3])
    } else {
        (None, layout[2])
    };

    if compact_readout {
        draw_readout_compact(f, readout_area, state);
    } else {
        draw_readout(f, readout_area, state);
    }
    if show_scope {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(canvas_area);
        let left = cols[0];
        let chart_area = cols[1];
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(left);
        draw_scope(f, rows[0], state);
        draw_envelope(f, rows[1], state);
        draw_chart(f, chart_area, state);
    } else {
        draw_chart(f, canvas_area, state);
    }
    if let Some(a) = diag_area {
        draw_diag(f, a, state);
    }
    draw_hint(f, hint_area);
}

/// Fast oscilloscope: the trailing 50ms of raw mic, plotted as
/// amplitude vs. time. X axis runs `[-SCOPE_WIN_MS, 0]` in ms, Y axis
/// is amplitude in `[-1, 1]`. Downsampled to roughly one point per
/// inner column so we don't shove 2.4k points through ratatui's
/// braille rasteriser every frame.
fn draw_scope(f: &mut Frame, area: Rect, state: &State) {
    let block = Block::default().borders(Borders::ALL).title(" scope ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 3 {
        return;
    }

    let sr = state.audio_sr_hz as usize;
    let win_samples = sr * SCOPE_WIN_MS as usize / 1000;
    let take = win_samples.min(state.audio.len());
    if take == 0 {
        return;
    }
    let start = state.audio.len() - take;
    // Downsample by stride to ~2 points per terminal column. Braille
    // packs 2 horizontal × 4 vertical sub-cells per character; one
    // sample per sub-column keeps the trace crisp without bursting the
    // dataset.
    let target_points = (inner.width as usize * 2).max(64);
    let stride = (take / target_points).max(1);

    let mut points: Vec<(f64, f64)> = Vec::with_capacity(take / stride + 1);
    for (i, idx) in (start..state.audio.len()).step_by(stride).enumerate() {
        let s = state.audio[idx];
        // x: ms ago, ranging from -SCOPE_WIN_MS at the left edge to 0
        // at the right.
        let ms_ago = -(((take - (i * stride)) as f64) * 1000.0 / sr as f64);
        points.push((ms_ago, s as f64));
    }

    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&points);

    let chart = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-(SCOPE_WIN_MS as f64), 0.0])
                .labels(vec![
                    Span::raw(format!("-{SCOPE_WIN_MS}ms")),
                    Span::raw("0"),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-1.0, 1.0])
                .labels(vec![Span::raw("-1"), Span::raw("0"), Span::raw("1")]),
        );
    f.render_widget(chart, inner);
}

/// Slow envelope: rolling RMS over the last 5s, plotted in dB. Reads
/// from the pre-computed `state.envelope` ring (one value per
/// `ENVELOPE_STEP_SAMPLES` of audio); the renderer just maps sample
/// indices to "seconds ago" using the most-recent envelope point as
/// the zero reference.
fn draw_envelope(f: &mut Frame, area: Rect, state: &State) {
    let block = Block::default().borders(Borders::ALL).title(" envelope ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 3 {
        return;
    }

    let Some(&(now_sample, _)) = state.envelope.back() else {
        return;
    };
    let sr = state.audio_sr_hz as f64;
    let win_secs = ENVELOPE_WINDOW_MS as f64 / 1000.0;
    let min_x = -win_secs;

    // Linear amplitude on [0, 0.5] reads more naturally than dB at this
    // size — a quiet hum is near the floor, a loud "ah" is most of the
    // way up. Clamp at 0.5; sustained louder than that is already
    // clipping territory and the singer wants to know.
    let points: Vec<(f64, f64)> = state
        .envelope
        .iter()
        .filter_map(|(idx, rms)| {
            let dx = now_sample as f64 - *idx as f64;
            let secs_ago = -dx / sr;
            if secs_ago < min_x {
                None
            } else {
                Some((secs_ago, (*rms as f64).min(0.5)))
            }
        })
        .collect();

    if points.is_empty() {
        return;
    }

    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Magenta))
        .data(&points);

    let chart = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([min_x, 0.0])
                .labels(vec![
                    Span::raw(format!("-{}s", win_secs as u64)),
                    Span::raw("0"),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, 0.5])
                .labels(vec![Span::raw("0"), Span::raw(".5")]),
        );
    f.render_widget(chart, inner);
}

/// Compact 1-row readout for short terminals. Same data and tinting
/// as the BigText variant but in plain centred text — `A4 +003  440.27 Hz`.
fn draw_readout_compact(f: &mut Frame, area: Rect, state: &State) {
    let latest_voiced = state.features.iter().rev().find(|s| s.f0_hz > 0.0).copied();
    let line = match latest_voiced {
        Some(s) => {
            let (note, cents) = note_and_cents(s.f0_hz);
            let sign = if cents >= 0 { '+' } else { '-' };
            let style = cents_style(cents);
            Line::from(vec![
                Span::styled(
                    format!("{note:<3} {sign}{:03}", cents.unsigned_abs()),
                    style,
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:.2} Hz", s.f0_hz),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
        None => Line::from(Span::styled(
            "-- sing to begin",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let para = Paragraph::new(line).alignment(ratatui::layout::Alignment::Center);
    f.render_widget(para, area);
}

/// Secondary diagnostic strip: breath and vibrato, right-aligned in a
/// muted colour. Sits between the chart and the hint. Drawn from the
/// freshest snapshot (not just the latest voiced one — breath is a
/// noise/voicing indicator that's interesting even when f0 is 0).
/// Empty until the data plane produces anything.
fn draw_diag(f: &mut Frame, area: Rect, state: &State) {
    let Some(s) = state.features.back() else {
        return;
    };
    let breath = format!("br {:.2}", s.breath);
    let vib = if s.vibrato_rate > 0.0 {
        format!("vib {:.1}Hz/{:.2}st", s.vibrato_rate, s.vibrato_depth)
    } else {
        "vib --".to_string()
    };
    let line = Line::from(vec![
        Span::styled(breath, Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(vib, Style::default().fg(Color::DarkGray)),
    ]);
    let para = Paragraph::new(line).alignment(ratatui::layout::Alignment::Right);
    f.render_widget(para, area);
}

/// Big block-character readout: `<NOTE> <±cents>` rendered with
/// `tui-big-text` (font8x8 glyphs at quadrant resolution — 4 cells per
/// glyph) on rows 0-3, with the f0 in Hz on row 4. Tinted by cents
/// band — green ≤5, default ≤20, yellow otherwise. Falls back to a
/// `--` placeholder and a "sing to begin" hint when no voiced frame
/// is available yet.
fn draw_readout(f: &mut Frame, area: Rect, state: &State) {
    let latest_voiced = state.features.iter().rev().find(|s| s.f0_hz > 0.0).copied();

    let (text, hz_text, style) = match latest_voiced {
        Some(s) => {
            let (note, cents) = note_and_cents(s.f0_hz);
            // Pad both halves so the block has identical character
            // width every frame: 3-char note slot (covers `C#4` and
            // pads `A4 `), space, sign + 3-digit zero-padded cents.
            // Without this the centred BigText jitters left/right as
            // `+3` → `+12` → `-100` change width.
            let sign = if cents >= 0 { '+' } else { '-' };
            let text = format!("{note:<3} {sign}{:03}", cents.unsigned_abs());
            let hz_text = format!("{:.2} Hz", s.f0_hz);
            let style = cents_style(cents);
            (text, hz_text, style)
        }
        None => (
            "--".to_string(),
            "sing to begin".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    };

    // Split the readout band: 4 rows of big glyphs (centred via the
    // BigText builder) + 1 row of Hz (centred Paragraph).
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(1)])
        .split(area);

    let big = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .style(style)
        .centered()
        .lines(vec![Line::from(text)])
        .build();
    f.render_widget(big, layout[0]);

    let hz_para = Paragraph::new(Line::from(Span::styled(
        hz_text,
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(hz_para, layout[1]);
}

/// Tint for the cents band, mirroring the line-mode `paint_cents`:
/// ≤5 green, ≤20 default, >20 yellow.
fn cents_style(cents: i32) -> Style {
    let mag = cents.unsigned_abs();
    if mag <= 5 {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if mag <= 20 {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
}

/// Equal-temperament note + cents from f0, A4=440. Mirrors the helper
/// in `main.rs` — kept duplicated here to avoid leaking it into a
/// shared module just for two callers.
fn note_and_cents(f0_hz: f32) -> (String, i32) {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let semis_from_a4 = 12.0 * (f0_hz / A4_HZ).log2();
    let nearest = semis_from_a4.round() as i32;
    let cents = ((semis_from_a4 - nearest as f32) * 100.0).round() as i32;
    let midi = 69 + nearest;
    let name_idx = midi.rem_euclid(12) as usize;
    let octave = midi.div_euclid(12) - 1;
    (format!("{}{}", NAMES[name_idx], octave), cents)
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

    // Build voiced segments split by tuning band — each sub-segment is
    // a contiguous run of voiced frames that share the same band
    // (green/default/yellow). Adjacent sub-segments share their
    // boundary point so the line stays visually continuous across
    // band transitions. ratatui's Dataset::data takes a borrowed
    // slice, so the Vec<(Band, Vec<…>)> must outlive the render call.
    let segments = build_voiced_segments_by_band(&state.features, now_ms);

    // Onset ticks: tiny dots pinned to the chart's lower edge, one per
    // onset-flagged frame in the window. Same x-coordinate system as
    // the trace so they slide left with time naturally.
    let onset_points = build_onset_ticks(&state.features, now_ms, state.y_lo);

    let mut datasets: Vec<Dataset> = segments
        .iter()
        .map(|(band, seg)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(band.color()))
                .data(seg)
        })
        .collect();
    if !onset_points.is_empty() {
        datasets.push(
            Dataset::default()
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(Color::DarkGray))
                .data(&onset_points),
        );
    }

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

/// Tuning band for a single voiced frame, derived from its distance to
/// the nearest semitone in cents. Mirrors the readout's tint logic so
/// the chart agrees with the big-text verdict at a glance.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Band {
    InTune,   // |cents| ≤ 5
    Close,    // 5 < |cents| ≤ 20
    OffPitch, // |cents| > 20
}

impl Band {
    fn color(self) -> Color {
        match self {
            Band::InTune => Color::Green,
            Band::Close => Color::Cyan,
            Band::OffPitch => Color::Yellow,
        }
    }
}

/// Compute the band for a voiced f0. Caller must guard against
/// `f0_hz <= 0.0` (unvoiced) — this function assumes voiced input.
fn band_for(f0_hz: f32) -> Band {
    let semis = 12.0 * (f0_hz / A4_HZ).log2();
    let cents = ((semis - semis.round()) * 100.0).abs();
    if cents <= 5.0 {
        Band::InTune
    } else if cents <= 20.0 {
        Band::Close
    } else {
        Band::OffPitch
    }
}

/// Split the ring into contiguous voiced sub-segments, each tagged
/// with its tuning band and translated into chart coordinates
/// `(seconds_ago, semitones)`. Unvoiced frames terminate the current
/// sub-segment (producing the visual gap). A band change *within* a
/// voiced run also starts a new sub-segment, but the boundary point
/// is shared with the previous sub-segment so the line stays visually
/// continuous across the colour change.
fn build_voiced_segments_by_band(
    features: &VecDeque<FeatureSnapshot>,
    now_ms: u64,
) -> Vec<(Band, Vec<(f64, f64)>)> {
    let mut segments: Vec<(Band, Vec<(f64, f64)>)> = Vec::new();
    let min_x = -(CHART_WINDOW_MS as f64) / 1000.0;
    let mut current: Option<(Band, Vec<(f64, f64)>)> = None;

    for s in features {
        if s.f0_hz <= 0.0 {
            if let Some(seg) = current.take() {
                segments.push(seg);
            }
            continue;
        }
        let x = -((now_ms.saturating_sub(s.t_ms)) as f64) / 1000.0;
        if x < min_x {
            continue;
        }
        let y = hz_to_semitones(s.f0_hz) as f64;
        let b = band_for(s.f0_hz);

        match current.as_mut() {
            None => {
                current = Some((b, vec![(x, y)]));
            }
            Some((cur_band, pts)) if *cur_band == b => {
                pts.push((x, y));
            }
            Some(_) => {
                // Band changed mid-run. Push the previous sub-segment
                // and start a new one — but include this boundary
                // point in both so the visible line is continuous
                // across the colour change.
                if let Some((prev_band, mut prev_pts)) = current.take() {
                    prev_pts.push((x, y));
                    segments.push((prev_band, prev_pts));
                }
                current = Some((b, vec![(x, y)]));
            }
        }
    }
    if let Some(seg) = current {
        segments.push(seg);
    }
    segments
}

/// Collect chart-coordinate points for every onset-flagged frame in
/// the visible window, pinned to the bottom of the chart so they read
/// as a row of tick marks under the trace. `y_lo` is the current
/// chart bottom (semitones from A4).
fn build_onset_ticks(
    features: &VecDeque<FeatureSnapshot>,
    now_ms: u64,
    y_lo: f32,
) -> Vec<(f64, f64)> {
    let min_x = -(CHART_WINDOW_MS as f64) / 1000.0;
    let y = y_lo as f64;
    features
        .iter()
        .filter(|s| s.onset > 0.0)
        .filter_map(|s| {
            let x = -((now_ms.saturating_sub(s.t_ms)) as f64) / 1000.0;
            if x < min_x {
                None
            } else {
                Some((x, y))
            }
        })
        .collect()
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
