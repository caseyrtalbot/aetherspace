//! Input policy: leader handling and shell-safe key encoding.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::action::Action;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leader {
    CtrlSpace,
    #[allow(dead_code)]
    Key(KeyBinding),
}

impl Leader {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    #[allow(dead_code)]
    pub(crate) fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn matches(self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    fn into_key_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InputRouter {
    config: InputConfig,
    leader_pending: bool,
}

impl InputRouter {
    pub(crate) fn new(config: InputConfig) -> Self {
        Self {
            config,
            leader_pending: false,
        }
    }

    pub(crate) fn route_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::Noop;
        }

        if self.leader_pending {
            self.leader_pending = false;
            return self.route_after_leader(key);
        }

        if self.config.leader.matches(key) {
            self.leader_pending = true;
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
            Action::SendBytes(text.into_bytes())
        }
    }

    fn route_after_leader(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
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
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => Vec::new(),
    };

    if alt && !bytes.is_empty() && key.code != KeyCode::Esc {
        bytes.insert(0, 0x1b);
    }
    bytes
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
    fn unmapped_key_is_empty() {
        assert!(encode_key(key(KeyCode::F(5))).is_empty());
    }

    #[test]
    fn default_leader_opens_command_prefix() {
        let mut router = InputRouter::new(InputConfig::default());
        assert_eq!(router.route_key(ctrl(KeyCode::Char(' '))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn leader_can_be_configured() {
        let mut router = InputRouter::new(InputConfig {
            leader: Leader::Key(KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        });
        assert_eq!(router.route_key(ctrl(KeyCode::Char('g'))), Action::Render);
        assert_eq!(router.route_key(key(KeyCode::Char('q'))), Action::Quit);
    }
}
