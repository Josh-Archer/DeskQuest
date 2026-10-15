use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, is_raw_mode_enabled, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::data_source::TelemetrySource;
use super::model::DashboardModel;
use super::render::render_dashboard;
use crate::HostError;

static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs a panic hook that restores the terminal to a clean state before printing the panic message.
pub fn install_panic_hook() {
    if !PANIC_HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = restore_terminal();
            original_hook(panic_info);
        }));
    }
}

/// Restores the terminal to its normal cooked mode and shows the cursor.
pub fn restore_terminal() -> io::Result<()> {
    if is_raw_mode_enabled().unwrap_or(false) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
    }
    Ok(())
}

/// Initializes the terminal in raw mode with an alternate screen buffer.
pub fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

/// Decides whether a key event should cause the dashboard to quit, reset, or pause.
/// Returns `true` if the event loop should exit.
pub fn handle_key_action(key: KeyEvent, model: &mut DashboardModel, paused: &mut bool) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('r') => {
            model.reset_stats();
            false
        }
        KeyCode::Char(' ') => {
            *paused = !*paused;
            false
        }
        _ => false,
    }
}

/// Runs the interactive TUI dashboard loop with the specified telemetry data source.
pub async fn run_dashboard_with_source<S: TelemetrySource>(
    mut source: S,
    tick_rate: Duration,
    shutdown_flag: Option<Arc<AtomicBool>>,
) -> Result<(), HostError> {
    let mut terminal = init_terminal().map_err(HostError::Io)?;
    let mut model = DashboardModel::new(60);
    let mut paused = false;

    let res = run_loop(
        &mut terminal,
        &mut source,
        &mut model,
        &mut paused,
        tick_rate,
        shutdown_flag,
    )
    .await;

    // Guarantee terminal restoration even if run_loop returns an error
    let _ = restore_terminal();
    res
}

async fn run_loop<S: TelemetrySource>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    source: &mut S,
    model: &mut DashboardModel,
    paused: &mut bool,
    tick_rate: Duration,
    shutdown_flag: Option<Arc<AtomicBool>>,
) -> Result<(), HostError> {
    loop {
        if let Some(ref flag) = shutdown_flag {
            if flag.load(Ordering::Relaxed) {
                break;
            }
        }

        // 1. Fetch telemetry unless paused
        if !*paused {
            match source.fetch_snapshot().await {
                Ok(snapshot) => {
                    model.apply_snapshot(snapshot);
                }
                Err(e) => {
                    tracing::debug!("Telemetry fetch error: {e}");
                }
            }
        }

        // 2. Render frame
        terminal
            .draw(|f| {
                render_dashboard(f, model);
            })
            .map_err(|e| HostError::Dashboard(format!("Terminal draw error: {e}")))?;

        // 3. Poll keyboard events
        if event::poll(tick_rate).map_err(HostError::Io)? {
            if let Event::Key(key) = event::read().map_err(HostError::Io)? {
                if handle_key_action(key, model, paused) {
                    break;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_key_action_quit_keys() {
        let mut model = DashboardModel::new(10);
        let mut paused = false;

        // 'q' should quit
        let quit_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(handle_key_action(quit_q, &mut model, &mut paused));

        // 'Q' should quit
        let quit_upper_q = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE);
        assert!(handle_key_action(quit_upper_q, &mut model, &mut paused));

        // 'Esc' should quit
        let quit_esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_key_action(quit_esc, &mut model, &mut paused));

        // Ctrl+C should quit
        let quit_ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(handle_key_action(quit_ctrl_c, &mut model, &mut paused));
    }

    #[test]
    fn test_handle_key_action_pause_and_reset() {
        let mut model = DashboardModel::new(10);
        let mut paused = false;

        // Space toggles pause
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!handle_key_action(space, &mut model, &mut paused));
        assert!(paused);

        assert!(!handle_key_action(space, &mut model, &mut paused));
        assert!(!paused);

        // 'r' resets stats
        let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(!handle_key_action(r, &mut model, &mut paused));

        // Non-action keys return false
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!handle_key_action(other, &mut model, &mut paused));
    }

    #[test]
    fn test_panic_hook_registration() {
        install_panic_hook();
        assert!(PANIC_HOOK_INSTALLED.load(Ordering::SeqCst));
    }
}
