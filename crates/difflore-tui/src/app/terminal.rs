//! Terminal lifecycle for the dashboard: raw-mode session, input-reader
//! thread, Ctrl-C signal task, and the panic hook that restores the screen
//! before the default handler prints.

use std::io;
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub(super) type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) struct InputReader {
    receiver: mpsc::Receiver<io::Result<KeyEvent>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl InputReader {
    pub(super) fn spawn() -> Self {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || read_input_events(&thread_stop, &sender));
        Self {
            receiver,
            stop,
            handle: Some(handle),
        }
    }

    pub(super) fn next_key(&self) -> Option<io::Result<KeyEvent>> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_input_events(stop: &Arc<AtomicBool>, sender: &mpsc::Sender<io::Result<KeyEvent>>) {
    while !stop.load(Ordering::Relaxed) {
        match event::poll(INPUT_POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if sender.send(Ok(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = sender.send(Err(err));
                    break;
                }
            },
            Ok(false) => {}
            Err(err) => {
                let _ = sender.send(Err(err));
                break;
            }
        }
    }
}

pub(super) struct SignalTask {
    handle: tokio::task::JoinHandle<()>,
}

impl SignalTask {
    pub(super) fn spawn(shutdown_requested: Arc<AtomicBool>) -> Self {
        let handle = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown_requested.store(true, Ordering::Relaxed);
            }
        });
        Self { handle }
    }
}

impl Drop for SignalTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(super) struct TerminalSession {
    terminal: TuiTerminal,
    cleaned: bool,
}

impl TerminalSession {
    pub(super) fn start() -> crate::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                restore_terminal_best_effort();
                return Err(err.into());
            }
        };

        Ok(Self {
            terminal,
            cleaned: false,
        })
    }

    pub(super) const fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }

    pub(super) fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        let mut errors = CleanupErrors::default();
        errors.record(disable_raw_mode());
        errors.record(execute!(self.terminal.backend_mut(), LeaveAlternateScreen));
        errors.record(self.terminal.show_cursor());
        errors.finish()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(super) struct TerminalPanicHook {
    original_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalPanicHook {
    pub(super) fn install() -> Self {
        let original_hook = Arc::new(Mutex::new(Some(panic::take_hook())));
        let hook_for_panic = Arc::clone(&original_hook);
        panic::set_hook(Box::new(move |info| {
            restore_terminal_best_effort();
            if let Ok(original_hook) = hook_for_panic.lock()
                && let Some(original_hook) = original_hook.as_ref()
            {
                original_hook(info);
            }
        }));
        Self { original_hook }
    }
}

impl Drop for TerminalPanicHook {
    fn drop(&mut self) {
        if let Ok(mut original_hook) = self.original_hook.lock()
            && let Some(original_hook) = original_hook.take()
        {
            panic::set_hook(original_hook);
        }
    }
}

#[derive(Default)]
struct CleanupErrors {
    first: Option<io::Error>,
}

impl CleanupErrors {
    fn record(&mut self, result: io::Result<()>) {
        if let Err(err) = result
            && self.first.is_none()
        {
            self.first = Some(err);
        }
    }

    fn finish(self) -> io::Result<()> {
        self.first.map_or_else(|| Ok(()), Err)
    }
}

fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

pub(super) fn finish_run(
    result: crate::Result<()>,
    cleanup_result: io::Result<()>,
) -> crate::Result<()> {
    match result {
        Ok(()) => cleanup_result.map_err(crate::TuiError::from),
        Err(err) => Err(err),
    }
}

pub(super) fn is_ctrl_c_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_key_is_detected_only_for_control_c_presses() {
        assert!(is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
        assert!(!is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn cleanup_errors_keep_first_failure() {
        let mut errors = CleanupErrors::default();
        errors.record(Err(io::Error::other("first")));
        errors.record(Ok(()));
        errors.record(Err(io::Error::other("second")));

        let err = errors.finish().unwrap_err();
        assert_eq!(err.to_string(), "first");
    }

    #[test]
    fn event_loop_error_wins_over_cleanup_error() {
        let result = finish_run(
            Err(crate::TuiError::Io(io::Error::other("event loop"))),
            Err(io::Error::other("cleanup")),
        );

        let err = result.unwrap_err();
        assert!(err.to_string().contains("event loop"));
    }

    #[test]
    fn cleanup_error_is_returned_when_event_loop_succeeds() {
        let result = finish_run(Ok(()), Err(io::Error::other("cleanup")));

        let err = result.unwrap_err();
        assert!(err.to_string().contains("cleanup"));
    }
}
