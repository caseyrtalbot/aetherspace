//! Input policy: leader handling and shell-safe key encoding.

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

use crate::action::Action;
use crate::layout::SplitDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputConfig {
    pub(crate) leader: Leader,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            leader: Leader::CtrlSpace,
        }
    }
}

impl InputConfig {
    pub(crate) fn from_leader_name(name: &str) -> Self {
        Self {
            leader: Leader::from_name(name).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Leader {
    #[default]
    CtrlSpace,
    Key(KeyBinding),
}

impl Leader {
    fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
        let normalized = normalized.replace('+', "-");
        match normalized.as_str() {
            "" | "ctrl-space" | "control-space" | "c-space" | "ctrl-@" | "control-@" => {
                Some(Self::CtrlSpace)
            }
            "esc" | "escape" => Some(Self::Key(KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE))),
            _ => parse_modified_char(&normalized),
        }
    }

    fn matches(self, key: KeyEvent) -> bool {
        match self {
            Self::CtrlSpace => {
                (key.code == KeyCode::Char(' ') && key.modifiers.contains(KeyModifiers::CONTROL))
                    || key.code == KeyCode::Null
            }
            Self::Key(binding) => binding.matches(key),
        }
    }

    fn bytes(self) -> Vec<u8> {
        match self {
            Self::CtrlSpace => vec![0x00],
            Self::Key(binding) => encode_key(binding.into_key_event()),
        }
    }

    fn label(self) -> String {
        match self {
            Self::CtrlSpace => "^Space".to_string(),
            Self::Key(binding) => binding.label(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub(crate) fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn matches(self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    fn into_key_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    fn label(self) -> String {
        let key = match self.code {
            KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::Backspace => "Backspace".to_string(),
            _ => "?".to_string(),
        };
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            format!("^{key}")
        } else if self.modifiers.contains(KeyModifiers::ALT) {
            format!("Alt-{key}")
        } else {
            key
        }
    }
}

fn parse_modified_char(normalized: &str) -> Option<Leader> {
    let (modifiers, rest) = if let Some(rest) = normalized.strip_prefix("ctrl-") {
        (KeyModifiers::CONTROL, rest)
    } else if let Some(rest) = normalized.strip_prefix("control-") {
        (KeyModifiers::CONTROL, rest)
    } else if let Some(rest) = normalized.strip_prefix("c-") {
        (KeyModifiers::CONTROL, rest)
    } else if let Some(rest) = normalized.strip_prefix("alt-") {
        (KeyModifiers::ALT, rest)
    } else if let Some(rest) = normalized.strip_prefix("option-") {
        (KeyModifiers::ALT, rest)
    } else {
        return None;
    };

    let code = match rest {
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        _ if rest.chars().count() == 1 => KeyCode::Char(rest.chars().next()?),
        _ => return None,
    };
    Some(Leader::Key(KeyBinding::new(code, modifiers)))
}

#[derive(Debug, Clone)]
pub(crate) struct InputRouter {
    config: InputConfig,
    mode: InputMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    ShellCapture,
    Leader,
}

impl InputRouter {
    pub(crate) fn new(config: InputConfig) -> Self {
        Self {
            config,
            mode: InputMode::ShellCapture,
        }
    }

    pub(crate) fn mode_label(&self) -> &'static str {
        match self.mode {
            InputMode::ShellCapture => "shell capture",
            InputMode::Leader => "leader",
        }
    }

    pub(crate) fn leader_label(&self) -> String {
        self.config.leader.label()
    }

    pub(crate) fn is_leader_key(&self, key: KeyEvent) -> bool {
        self.config.leader.matches(key)
    }

    pub(crate) fn route_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::Noop;
        }

        if self.mode == InputMode::Leader {
            self.mode = InputMode::ShellCapture;
            return self.route_after_leader(key);
        }

        if self.config.leader.matches(key) {
            self.mode = InputMode::Leader;
            return Action::Render;
        }

        let bytes = encode_key(key);
        if bytes.is_empty() {
            Action::Noop
        } else {
            Action::SendBytes(bytes)
        }
    }

    pub(crate) fn route_paste(&self, text: String) -> Action {
        if text.is_empty() {
            Action::Noop
        } else {
            Action::SendBytes(encode_bracketed_paste(&text))
        }
    }

    fn route_after_leader(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
            KeyCode::Char('r') | KeyCode::Char('R') => Action::RestartFocusedPane,
            KeyCode::Char('x') | KeyCode::Char('X') => Action::CloseFocusedPane,
            KeyCode::Char('|') => Action::SplitFocusedPane {
                dir: SplitDir::Horizontal,
            },
            KeyCode::Char('-') => Action::SplitFocusedPane {
                dir: SplitDir::Vertical,
            },
            KeyCode::Tab
            | KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('n')
            | KeyCode::Char('N') => Action::FocusNext,
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up => Action::FocusPrev,
            KeyCode::Char('>') | KeyCode::Char('=') | KeyCode::Char('+') => {
                Action::ResizeFocusedPane { delta: 5 }
            }
            KeyCode::Char('<') | KeyCode::Char('_') => Action::ResizeFocusedPane { delta: -5 },
            KeyCode::Char('z') | KeyCode::Char('Z') => Action::ToggleZoomFocusedPane,
            KeyCode::Char('f') | KeyCode::Char('F') => Action::ToggleFloatFocusedPane,
            KeyCode::Char('t') | KeyCode::Char('T') => Action::ToggleStack,
            KeyCode::Char('b') | KeyCode::Char('B') => Action::ToggleCompactChrome,
            KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::Char('H') => Action::OpenHelp,
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char(':') => {
                Action::OpenCommandPalette
            }
            KeyCode::Char('p') | KeyCode::Char('P') => Action::OpenProjectPalette,
            KeyCode::Char('v') | KeyCode::Char('V') => Action::OpenProjectViewer,
            KeyCode::Char('s') | KeyCode::Char('S') => Action::OpenProjectShell,
            KeyCode::Char('e') | KeyCode::Char('E') => Action::EditScrollback,
            KeyCode::Esc => Action::Render,
            _ if self.config.leader.matches(key) => Action::SendBytes(self.config.leader.bytes()),
            _ => Action::Render,
        }
    }
}

/// Translate a crossterm key event into bytes for a PTY.
pub(crate) fn encode_key(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                encode_control_char(c)
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Null if ctrl || key.modifiers.is_empty() => vec![0x00],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => vec![0x17],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => encode_csi_final('D', key.modifiers),
        KeyCode::Right => encode_csi_final('C', key.modifiers),
        KeyCode::Up => encode_csi_final('A', key.modifiers),
        KeyCode::Down => encode_csi_final('B', key.modifiers),
        KeyCode::Home => encode_csi_final('H', key.modifiers),
        KeyCode::End => encode_csi_final('F', key.modifiers),
        KeyCode::Insert => encode_csi_tilde(2, key.modifiers),
        KeyCode::Delete => encode_csi_tilde(3, key.modifiers),
        KeyCode::PageUp => encode_csi_tilde(5, key.modifiers),
        KeyCode::PageDown => encode_csi_tilde(6, key.modifiers),
        KeyCode::F(n) => encode_function_key(n, key.modifiers),
        _ => Vec::new(),
    };

    if alt && alt_uses_escape_prefix(key.code) && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn alt_uses_escape_prefix(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace
    )
}

fn encode_bracketed_paste(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Translate a crossterm mouse event into bytes for a child PTY.
///
/// `column` and `row` are zero-based coordinates in the child pane's terminal
/// content area. The emitted xterm coordinates are one-based.
pub(crate) fn encode_mouse(
    mouse: MouseEvent,
    column: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if !mouse_event_enabled(mouse.kind, mode) {
        return None;
    }

    let cb = mouse_cb(mouse, encoding);
    let x = u32::from(column) + 1;
    let y = u32::from(row) + 1;
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{cb};{x};{y}{suffix}").into_bytes())
        }
        MouseProtocolEncoding::Default => encode_default_mouse(cb, x, y),
        MouseProtocolEncoding::Utf8 => encode_utf8_mouse(cb, x, y),
    }
}

fn mouse_event_enabled(kind: MouseEventKind, mode: MouseProtocolMode) -> bool {
    match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::PressRelease => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::ButtonMotion => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        MouseProtocolMode::AnyMotion => true,
    }
}

fn mouse_cb(mouse: MouseEvent, encoding: MouseProtocolEncoding) -> u16 {
    let mut cb = match mouse.kind {
        MouseEventKind::Down(button) => mouse_button_code(button),
        MouseEventKind::Up(button) if encoding == MouseProtocolEncoding::Sgr => {
            mouse_button_code(button)
        }
        MouseEventKind::Up(_) => 3,
        MouseEventKind::Drag(button) => mouse_button_code(button) + 32,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };

    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    cb
}

fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn encode_default_mouse(cb: u16, x: u32, y: u32) -> Option<Vec<u8>> {
    Some(vec![
        0x1b,
        b'[',
        b'M',
        encode_default_mouse_value(u32::from(cb))?,
        encode_default_mouse_value(x)?,
        encode_default_mouse_value(y)?,
    ])
}

fn encode_default_mouse_value(value: u32) -> Option<u8> {
    (value <= 223).then_some((value + 32) as u8)
}

fn encode_utf8_mouse(cb: u16, x: u32, y: u32) -> Option<Vec<u8>> {
    let mut out = b"\x1b[M".to_vec();
    push_utf8_mouse_value(&mut out, u32::from(cb))?;
    push_utf8_mouse_value(&mut out, x)?;
    push_utf8_mouse_value(&mut out, y)?;
    Some(out)
}

fn push_utf8_mouse_value(out: &mut Vec<u8>, value: u32) -> Option<()> {
    let ch = char::from_u32(value + 32)?;
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
}

fn encode_csi_final(final_byte: char, modifiers: KeyModifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[1;{param}{final_byte}").into_bytes(),
        None => format!("\x1b[{final_byte}").into_bytes(),
    }
}

fn encode_csi_tilde(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[{number};{param}~").into_bytes(),
        None => format!("\x1b[{number}~").into_bytes(),
    }
}

fn encode_function_key(n: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let Some(param) = modifier_param(modifiers) else {
        return match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        };
    };

    match n {
        1 => format!("\x1b[1;{param}P").into_bytes(),
        2 => format!("\x1b[1;{param}Q").into_bytes(),
        3 => format!("\x1b[1;{param}R").into_bytes(),
        4 => format!("\x1b[1;{param}S").into_bytes(),
        5 => format!("\x1b[15;{param}~").into_bytes(),
        6 => format!("\x1b[17;{param}~").into_bytes(),
        7 => format!("\x1b[18;{param}~").into_bytes(),
        8 => format!("\x1b[19;{param}~").into_bytes(),
        9 => format!("\x1b[20;{param}~").into_bytes(),
        10 => format!("\x1b[21;{param}~").into_bytes(),
        11 => format!("\x1b[23;{param}~").into_bytes(),
        12 => format!("\x1b[24;{param}~").into_bytes(),
        _ => Vec::new(),
    }
}

fn modifier_param(modifiers: KeyModifiers) -> Option<u8> {
    let mut param = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        param += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        param += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        param += 4;
    }
    (param > 1).then_some(param)
}

fn encode_control_char(c: char) -> Vec<u8> {
    match c {
        ' ' => vec![0x00],
        '?' => vec![0x7f],
        c if c.is_ascii() => vec![(c.to_ascii_uppercase() as u8).wrapping_sub(0x40) & 0x1f],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn ctrl_letters_map_to_control_bytes() {
        assert_eq!(encode_key(ctrl(KeyCode::Char('a'))), vec![0x01]);
        assert_eq!(encode_key(ctrl(KeyCode::Char('z'))), vec![0x1a]);
        assert_eq!(encode_key(ctrl(KeyCode::Char('C'))), vec![0x03]);
    }

    #[test]
    fn ctrl_space_maps_to_nul() {
        assert_eq!(encode_key(ctrl(KeyCode::Char(' '))), vec![0x00]);
        assert_eq!(encode_key(key(KeyCode::Null)), vec![0x00]);
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(encode_key(key(KeyCode::Enter)), vec![b'\r']);
        assert_eq!(encode_key(key(KeyCode::Backspace)), vec![0x7f]);
    }

    #[test]
    fn arrows_emit_csi() {
        assert_eq!(encode_key(key(KeyCode::Up)), b"\x1b[A".to_vec());
        assert_eq!(encode_key(key(KeyCode::Down)), b"\x1b[B".to_vec());
        assert_eq!(encode_key(key(KeyCode::Right)), b"\x1b[C".to_vec());
        assert_eq!(encode_key(key(KeyCode::Left)), b"\x1b[D".to_vec());
    }

    #[test]
    fn plain_char_is_its_byte() {
        assert_eq!(encode_key(key(KeyCode::Char('a'))), vec![b'a']);
    }

    #[test]
    fn multibyte_char_is_utf8() {
        assert_eq!(encode_key(key(KeyCode::Char('é'))), "é".as_bytes().to_vec());
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(encode_key(alt(KeyCode::Char('x'))), b"\x1bx".to_vec());
    }

    #[test]
    fn function_keys_are_encoded() {
        assert_eq!(encode_key(key(KeyCode::F(1))), b"\x1bOP".to_vec());
        assert_eq!(encode_key(key(KeyCode::F(5))), b"\x1b[15~".to_vec());
        assert_eq!(encode_key(shift(KeyCode::F(5))), b"\x1b[15;2~".to_vec());
    }

    #[test]
    fn modified_special_keys_use_xterm_modifier_params() {
        assert_eq!(encode_key(ctrl(KeyCode::Left)), b"\x1b[1;5D".to_vec());
        assert_eq!(encode_key(alt(KeyCode::Delete)), b"\x1b[3;3~".to_vec());
        assert_eq!(encode_key(shift(KeyCode::Home)), b"\x1b[1;2H".to_vec());
    }

    #[test]
    fn unmapped_function_key_is_empty() {
        assert!(encode_key(key(KeyCode::F(13))).is_empty());
    }

    #[test]
    fn paste_is_bracketed_for_the_child() {
        let router = InputRouter::new(InputConfig::default());
        assert_eq!(
            router.route_paste("hello\n".to_string()),
            Action::SendBytes(b"\x1b[200~hello\n\x1b[201~".to_vec())
        );
    }

    #[test]
    fn sgr_mouse_encodes_press_release_and_coordinates() {
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                0,
                0,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Up(MouseButton::Left)),
                4,
                7,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<0;5;8m".to_vec())
        );
    }

    #[test]
    fn mouse_modes_filter_motion_events() {
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Moved),
                0,
                0,
                MouseProtocolMode::ButtonMotion,
                MouseProtocolEncoding::Sgr,
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Moved),
                0,
                0,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
            ),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
    }

    #[test]
    fn default_mouse_uses_legacy_release_code() {
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Up(MouseButton::Right)),
                0,
                0,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
            ),
            Some(b"\x1b[M#!!".to_vec())
        );
    }

    #[test]
    fn default_mouse_drops_coordinates_outside_legacy_range() {
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                223,
                0,
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Default,
            ),
            None
        );
    }

    #[test]
    fn default_leader_opens_command_prefix() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn leader_can_be_configured() {
        let mut router = InputRouter::new(InputConfig::from_leader_name("ctrl-g"));
        assert_eq!(router.route_key(ctrl(KeyCode::Char('g'))), Action::Render);
        assert_eq!(router.mode_label(), "leader");
        assert_eq!(router.route_key(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(router.mode_label(), "shell capture");
        assert_eq!(router.leader_label(), "^G");
    }

    #[test]
    fn invalid_configured_leader_falls_back_to_ctrl_space() {
        let mut router = InputRouter::new(InputConfig::from_leader_name("not-a-key"));
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn leader_routes_restart_and_close() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('r'))),
            Action::RestartFocusedPane
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('x'))),
            Action::CloseFocusedPane
        );
    }

    #[test]
    fn leader_routes_phase3_layout_actions() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('|'))),
            Action::SplitFocusedPane {
                dir: SplitDir::Horizontal
            }
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Tab)), Action::FocusNext);

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('>'))),
            Action::ResizeFocusedPane { delta: 5 }
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('z'))),
            Action::ToggleZoomFocusedPane
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('f'))),
            Action::ToggleFloatFocusedPane
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('t'))),
            Action::ToggleStack
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('b'))),
            Action::ToggleCompactChrome
        );
    }

    #[test]
    fn leader_routes_phase5_workflow_actions() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('c'))),
            Action::OpenCommandPalette
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('p'))),
            Action::OpenProjectPalette
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('v'))),
            Action::OpenProjectViewer
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('s'))),
            Action::OpenProjectShell
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(
            router.route_key(key(KeyCode::Char('e'))),
            Action::EditScrollback
        );

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('h'))), Action::OpenHelp);

        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('?'))), Action::OpenHelp);
    }

    #[test]
    fn shell_capture_passes_common_readline_keys_through() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(
            router.route_key(ctrl(KeyCode::Char('a'))),
            Action::SendBytes(vec![0x01])
        );
        assert_eq!(
            router.route_key(ctrl(KeyCode::Char('e'))),
            Action::SendBytes(vec![0x05])
        );
        assert_eq!(
            router.route_key(key(KeyCode::Tab)),
            Action::SendBytes(vec![b'\t'])
        );
    }
}
