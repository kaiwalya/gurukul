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
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// Poll cadence for crossterm input. 16ms ≈ 60Hz — a comfortable
/// frame budget; the log pane redraws on every tick, so the lag
/// between a log line being pushed and showing up is at most ~16ms.
const TICK: Duration = Duration::from_millis(16);

/// Run the shell until the user quits, or `deadline` elapses.
/// `deadline = None` means run until the user quits.
pub fn run(logs: LogBuffer, deadline: Option<Instant>) -> io::Result<()> {
    let mut terminal = setup()?;
    let mut state = State::default();

    let outcome = event_loop(&mut terminal, &mut state, &logs, deadline);

    teardown(&mut terminal)?;
    outcome
}

#[derive(Default)]
struct State {
    console_open: bool,
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
    logs: &LogBuffer,
    deadline: Option<Instant>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, state, logs))?;

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

fn draw(f: &mut Frame, state: &State, logs: &LogBuffer) {
    let area = f.area();
    if state.console_open {
        let [main, console] = split_with_console(area);
        draw_main(f, main);
        draw_console(f, console, logs);
    } else {
        draw_main(f, area);
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

fn draw_main(f: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::styled("~", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" console   "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]);
    let body =
        Paragraph::new(hint).block(Block::default().borders(Borders::ALL).title(" gurukul "));
    f.render_widget(body, area);
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
