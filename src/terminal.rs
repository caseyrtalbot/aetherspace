//! Terminal boundary guard.

use std::io::{Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::DefaultTerminal;
use ratatui::crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::supports_keyboard_enhancement,
};

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
