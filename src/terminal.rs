//! Terminal boundary guard.

use std::io::{Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::{DefaultTerminal, Terminal, backend::CrosstermBackend};

static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);

pub(crate) struct TerminalGuard {
    terminal: DefaultTerminal,
    restored: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> Self {
        clear_main_screen();

        let terminal = ratatui::init();
        install_panic_hook();
        enable_terminal_extras();

        Self {
            terminal,
            restored: false,
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }

    /// Re-acquire the alternate screen after a foreign full-screen program (an
    /// `$EDITOR`) ran on the main screen. Pairs with a prior [`restore`].
    ///
    /// This re-runs only what `ratatui::init` does MINUS its panic hook: raw mode,
    /// the alternate screen, and a fresh terminal backend. `ratatui::init` installs
    /// a panic hook on every call, so calling it here would stack a new hook (and
    /// leak the previous one) on every EditScrollback. The hook from `enter()` is
    /// still installed and survives `restore`, so re-entry must avoid `init`. It
    /// DOES re-enable mouse capture + bracketed paste (which the editor's restore
    /// disabled, or which a SIGKILLed editor left in an unknown state) so the TUI
    /// is usable on return.
    pub(crate) fn reenter(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(stdout(), EnterAlternateScreen);
        if let Ok(terminal) = Terminal::new(CrosstermBackend::new(stdout())) {
            self.terminal = terminal;
        }
        enable_terminal_extras();
        self.restored = false;
    }

    pub(crate) fn restore(&mut self) {
        if self.restored {
            return;
        }
        restore_terminal_extras();
        ratatui::restore();
        self.restored = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn clear_main_screen() {
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[2J\x1b[3J\x1b[H");
    let _ = out.flush();
}

fn enable_terminal_extras() {
    let mut out = stdout();
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        if execute!(out, PushKeyboardEnhancementFlags(flags)).is_ok() {
            KEYBOARD_FLAGS_PUSHED.store(true, Ordering::Release);
        }
    }
    let _ = execute!(out, EnableBracketedPaste, EnableMouseCapture);
}

fn restore_terminal_extras() {
    let mut out = stdout();
    if KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::AcqRel) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, DisableMouseCapture);
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::log::error(&format!("panic: {info}"));
        restore_terminal_extras();
        original(info);
    }));
}
