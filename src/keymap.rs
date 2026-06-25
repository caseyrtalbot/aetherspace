//! Data-driven keymap: one code-built table per physical dispatch surface.
//!
//! Two tables mirror the two dispatch sites exactly:
//! - the LEADER table (post-leader keys, matched on [`KeyCode`] ONLY — modifiers
//!   ignored, so leader-then-Ctrl+q still Quits), and
//! - the GLOBAL table (always-on shortcuts, matched by EXACT `(KeyCode,
//!   KeyModifiers)` equality across the NONE / CONTROL / ALT / CONTROL|SHIFT arms).
//!
//! Storage is `Vec<(Chord, Binding)>` per table with linear lookup. crossterm
//! 0.29 derives `PartialOrd` but NOT `Ord` on `KeyCode`/`KeyModifiers`, so a
//! `BTreeMap` keyed on those is a hard compile error; a Vec of ~25 entries is
//! trivially fast on a keypress and preserves insertion order for deterministic
//! first-offender reporting. Raw config maps keep `BTreeMap<String,String>`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Action;
use crate::layout::SplitDir;

/// A bindable chord. Leader chords match on key code only (the input.rs dispatch
/// asymmetry); global chords match on exact code + modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chord {
    Leader(KeyCode),
    Global(KeyCode, KeyModifiers),
}

/// Whether a binding applies always or only when a viewer pane is focused. Carries
/// the `focused_is_viewer` gate that the old global table applied to Tab/BackTab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    Always,
    ViewerOnly,
}

/// One bound entry: which action fires and under what focus context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) action: BindAction,
    pub(crate) context: Context,
}

impl Binding {
    fn always(action: BindAction) -> Self {
        Self {
            action,
            context: Context::Always,
        }
    }

    fn viewer_only(action: BindAction) -> Self {
        Self {
            action,
            context: Context::ViewerOnly,
        }
    }
}

/// The user-bindable subset of [`Action`], with data-carrying variants FLATTENED
/// to distinct names so each maps to a bare config string. Excludes the
/// non-bindable runtime actions (Render / Resize / SendBytes / Noop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindAction {
    Quit,
    SplitHorizontal,
    SplitVertical,
    Restart,
    Close,
    FocusNext,
    FocusPrev,
    ResizeGrow,
    ResizeShrink,
    Zoom,
    Float,
    ToggleStack,
    ToggleCompact,
    Help,
    CommandPalette,
    ProjectPalette,
    Viewer,
    Shell,
    EditScrollback,
    ReloadConfig,
}

impl BindAction {
    /// Expand a bindable action into the runtime [`Action`], re-inflating the
    /// flattened payloads (split direction, resize delta).
    pub(crate) fn to_action(self) -> Action {
        match self {
            Self::Quit => Action::Quit,
            Self::SplitHorizontal => Action::SplitFocusedPane {
                dir: SplitDir::Horizontal,
            },
            Self::SplitVertical => Action::SplitFocusedPane {
                dir: SplitDir::Vertical,
            },
            Self::Restart => Action::RestartFocusedPane,
            Self::Close => Action::CloseFocusedPane,
            Self::FocusNext => Action::FocusNext,
            Self::FocusPrev => Action::FocusPrev,
            Self::ResizeGrow => Action::ResizeFocusedPane { delta: 5 },
            Self::ResizeShrink => Action::ResizeFocusedPane { delta: -5 },
            Self::Zoom => Action::ToggleZoomFocusedPane,
            Self::Float => Action::ToggleFloatFocusedPane,
            Self::ToggleStack => Action::ToggleStack,
            Self::ToggleCompact => Action::ToggleCompactChrome,
            Self::Help => Action::OpenHelp,
            Self::CommandPalette => Action::OpenCommandPalette,
            Self::ProjectPalette => Action::OpenProjectPalette,
            Self::Viewer => Action::OpenProjectViewer,
            Self::Shell => Action::OpenProjectShell,
            Self::EditScrollback => Action::EditScrollback,
            Self::ReloadConfig => Action::ReloadConfig,
        }
    }

    /// Parse a config action name (snake or kebab case). Unknown names return
    /// `None` so the caller can drop-and-warn.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
        let action = match normalized.as_str() {
            "quit" => Self::Quit,
            "split_horizontal" => Self::SplitHorizontal,
            "split_vertical" => Self::SplitVertical,
            "restart" => Self::Restart,
            "close" => Self::Close,
            "focus_next" => Self::FocusNext,
            "focus_prev" => Self::FocusPrev,
            "resize_grow" => Self::ResizeGrow,
            "resize_shrink" => Self::ResizeShrink,
            "zoom" => Self::Zoom,
            "float" => Self::Float,
            "toggle_stack" => Self::ToggleStack,
            "toggle_compact" => Self::ToggleCompact,
            "help" => Self::Help,
            "command_palette" => Self::CommandPalette,
            "project_palette" => Self::ProjectPalette,
            "viewer" => Self::Viewer,
            "shell" => Self::Shell,
            "edit_scrollback" => Self::EditScrollback,
            "reload" => Self::ReloadConfig,
            _ => return None,
        };
        Some(action)
    }
}

/// The two dispatch tables, code-built from defaults and (optionally) overlaid
/// with a sparse user `[keymap]` config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keymap {
    global: Vec<(Chord, Binding)>,
    leader: Vec<(Chord, Binding)>,
}

impl Default for Keymap {
    fn default() -> Self {
        default_keymap()
    }
}

impl Keymap {
    /// Resolve a global shortcut by EXACT `(code, modifiers)` equality, honoring
    /// the ViewerOnly gate. Linear scan over insertion order; first match wins.
    pub(crate) fn lookup_global(
        &self,
        focused_is_viewer: bool,
        key: KeyEvent,
    ) -> Option<BindAction> {
        for (chord, binding) in &self.global {
            let Chord::Global(code, modifiers) = chord else {
                continue;
            };
            if *code == key.code && *modifiers == key.modifiers {
                if binding.context == Context::ViewerOnly && !focused_is_viewer {
                    continue;
                }
                return Some(binding.action);
            }
        }
        None
    }

    /// Resolve a post-leader key by code ONLY (modifiers ignored), matching the
    /// historical leader dispatch. Linear scan; first match wins.
    pub(crate) fn lookup_leader(&self, key: KeyEvent) -> Option<BindAction> {
        for (chord, binding) in &self.leader {
            if let Chord::Leader(code) = chord
                && *code == key.code
            {
                return Some(binding.action);
            }
        }
        None
    }

    /// Iterate the global table in insertion order (chord + binding). Used by the
    /// help overlay and statusline regeneration.
    pub(crate) fn global_entries(&self) -> impl Iterator<Item = (Chord, Binding)> + '_ {
        self.global.iter().copied()
    }

    /// Look up the first global chord bound to a given action (insertion order).
    /// Drives statusline hints that name a key by its action.
    pub(crate) fn global_chord_for(&self, action: BindAction) -> Option<Chord> {
        self.global
            .iter()
            .find(|(_, binding)| binding.action == action)
            .map(|(chord, _)| *chord)
    }

    /// Look up the first leader chord bound to a given action (insertion order).
    pub(crate) fn leader_chord_for(&self, action: BindAction) -> Option<Chord> {
        self.leader
            .iter()
            .find(|(_, binding)| binding.action == action)
            .map(|(chord, _)| *chord)
    }

    /// Apply a sparse user overlay to both tables. For each `(spelling, name)` in
    /// a user map: parse the chord and action; a PRIOR user entry in THIS pass for
    /// the same chord is a conflict (first-offender warning, later one dropped); an
    /// unknown key spelling or action name drops that entry with a warning;
    /// otherwise upsert, replacing any default binding for that chord (intended
    /// override, no warning). Returns the first one-line warning, if any.
    ///
    /// `leader_chord` is the configured `input.leader`'s representative [`Chord`]
    /// (e.g. the default CtrlSpace → `Char(' ')`). A `[keymap].leader` entry whose
    /// chord equals it is a LEADER COLLISION (decision 4b): that chord is consumed
    /// by leader-mode entry and can never reach the keymap, so the entry is dropped
    /// with a first-offender warning. Pass `None` to skip the collision check.
    pub(crate) fn merge_overlay(
        &mut self,
        global: &std::collections::BTreeMap<String, String>,
        leader: &std::collections::BTreeMap<String, String>,
        leader_chord: Option<Chord>,
    ) -> Option<String> {
        let mut first_warning = None;
        merge_table(
            &mut self.global,
            global,
            ChordMode::Global,
            None,
            &mut first_warning,
        );
        merge_table(
            &mut self.leader,
            leader,
            ChordMode::Leader,
            leader_chord,
            &mut first_warning,
        );
        first_warning
    }
}

/// Derive the configured leader's representative [`Chord`] for collision detection.
/// Mirrors `input::Leader::from_name`: the CtrlSpace family normalizes to a literal
/// `Char(' ')` (decision 13), `esc` to `Esc`, and any modified-char leader to its
/// bare key code. The result is a `Chord::Leader` because the keymap leader table
/// dispatches on code only. Returns `None` when the name does not parse (the leader
/// itself falls back to CtrlSpace, but with no usable representative we skip the
/// collision check rather than guess).
pub(crate) fn leader_representative_chord(leader_name: &str) -> Option<Chord> {
    let normalized = leader_name
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '+'], "-");
    match normalized.as_str() {
        "" | "ctrl-space" | "control-space" | "c-space" | "ctrl-@" | "control-@" => {
            Some(Chord::Leader(KeyCode::Char(' ')))
        }
        _ => {
            let (_, code) = parse_key_spelling(&normalized)?;
            Some(Chord::Leader(code))
        }
    }
}

/// Which table a parse targets: leader chords are code-only, global chords carry
/// modifiers parsed from the spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChordMode {
    Leader,
    Global,
}

fn merge_table(
    table: &mut Vec<(Chord, Binding)>,
    overlay: &std::collections::BTreeMap<String, String>,
    mode: ChordMode,
    leader_chord: Option<Chord>,
    first_warning: &mut Option<String>,
) {
    // Chords set by THIS overlay pass, to detect distinct spellings that normalize
    // to one chord (TOML already rejects identical-string duplicate keys).
    let mut seen: Vec<Chord> = Vec::new();
    for (spelling, name) in overlay {
        let Some(chord) = parse_chord(spelling, mode) else {
            warn(first_warning, format!("keymap: unknown key '{spelling}'"));
            continue;
        };
        // Leader collision (decision 4b): a [keymap].leader entry whose chord equals
        // the configured input.leader can never reach the keymap (leader-mode entry
        // intercepts it). Drop it and warn, before the action even matters.
        if leader_chord == Some(chord) {
            warn(
                first_warning,
                format!("keymap: leader binding '{spelling}' collides with the configured leader"),
            );
            continue;
        }
        let Some(action) = BindAction::from_name(name) else {
            warn(
                first_warning,
                format!("keymap: unknown action '{name}' for '{spelling}'"),
            );
            continue;
        };
        if seen.contains(&chord) {
            warn(
                first_warning,
                format!("keymap: duplicate binding for '{spelling}'"),
            );
            continue;
        }
        seen.push(chord);
        let binding = Binding::always(action);
        // Upsert: replace any existing binding for this chord (default override),
        // else append. ViewerOnly defaults are replaced by an explicit user bind.
        if let Some(slot) = table.iter_mut().find(|(c, _)| *c == chord) {
            slot.1 = binding;
        } else {
            table.push((chord, binding));
        }
    }
}

fn warn(first_warning: &mut Option<String>, message: String) {
    crate::log::warn(&message);
    if first_warning.is_none() {
        *first_warning = Some(message);
    }
}

/// Build the default keymap: every binding the two hardcoded dispatch tables
/// carried, in code. Insertion order is the deterministic ordering for
/// first-offender reporting and discoverability surfaces.
pub(crate) fn default_keymap() -> Keymap {
    Keymap {
        global: default_global(),
        leader: default_leader(),
    }
}

fn default_global() -> Vec<(Chord, Binding)> {
    use BindAction::*;
    use KeyCode::*;
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    let alt = KeyModifiers::ALT;
    let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;

    vec![
        // NONE: function keys + viewer-gated Tab/BackTab.
        (Chord::Global(F(1), none), Binding::always(Help)),
        (Chord::Global(F(2), none), Binding::always(CommandPalette)),
        (Chord::Global(F(3), none), Binding::always(ProjectPalette)),
        (Chord::Global(F(4), none), Binding::always(Viewer)),
        (Chord::Global(F(5), none), Binding::always(Shell)),
        (Chord::Global(F(6), none), Binding::always(FocusNext)),
        (Chord::Global(F(7), none), Binding::always(FocusPrev)),
        (Chord::Global(F(8), none), Binding::always(Zoom)),
        (Chord::Global(F(9), none), Binding::always(Close)),
        (Chord::Global(F(10), none), Binding::always(Quit)),
        // F11 reloads config (F1-F10 taken above; r/R are taken in the leader table
        // by Restart, so reload uses leader g/G + global F11).
        (Chord::Global(F(11), none), Binding::always(ReloadConfig)),
        (Chord::Global(Tab, none), Binding::viewer_only(FocusNext)),
        (
            Chord::Global(BackTab, none),
            Binding::viewer_only(FocusPrev),
        ),
        // CONTROL.
        (Chord::Global(Enter, ctrl), Binding::always(CommandPalette)),
        (Chord::Global(Char('/'), ctrl), Binding::always(Help)),
        (Chord::Global(Tab, ctrl), Binding::always(FocusNext)),
        (Chord::Global(BackTab, ctrl), Binding::always(FocusPrev)),
        // ALT (the can't-send-Ctrl fallback mirror).
        (Chord::Global(Enter, alt), Binding::always(CommandPalette)),
        (Chord::Global(Char('/'), alt), Binding::always(Help)),
        (Chord::Global(Tab, alt), Binding::always(FocusNext)),
        (Chord::Global(BackTab, alt), Binding::always(FocusPrev)),
        // CONTROL|SHIFT.
        (Chord::Global(Tab, ctrl_shift), Binding::always(FocusPrev)),
        (
            Chord::Global(BackTab, ctrl_shift),
            Binding::always(FocusPrev),
        ),
    ]
}

fn default_leader() -> Vec<(Chord, Binding)> {
    use BindAction::*;
    use KeyCode::*;

    // Each KeyCode the old `route_after_leader` matched, in source order, mapped to
    // its action. Modifiers are ignored at lookup, so only the code matters.
    let entries: &[(KeyCode, BindAction)] = &[
        (Char('q'), Quit),
        (Char('Q'), Quit),
        (Char('r'), Restart),
        (Char('R'), Restart),
        (Char('x'), Close),
        (Char('X'), Close),
        (Char('|'), SplitHorizontal),
        (Char('-'), SplitVertical),
        (Tab, FocusNext),
        (Right, FocusNext),
        (Down, FocusNext),
        (Char('n'), FocusNext),
        (Char('N'), FocusNext),
        (BackTab, FocusPrev),
        (Left, FocusPrev),
        (Up, FocusPrev),
        (Char('>'), ResizeGrow),
        (Char('='), ResizeGrow),
        (Char('+'), ResizeGrow),
        (Char('<'), ResizeShrink),
        (Char('_'), ResizeShrink),
        (Char('z'), Zoom),
        (Char('Z'), Zoom),
        (Char('f'), Float),
        (Char('F'), Float),
        (Char('t'), ToggleStack),
        (Char('T'), ToggleStack),
        (Char('b'), ToggleCompact),
        (Char('B'), ToggleCompact),
        // h first so the leader-hint surface advertises 'h' (more discoverable than
        // '?'); dispatch is identical across all three codes.
        (Char('h'), Help),
        (Char('H'), Help),
        (Char('?'), Help),
        (Char('c'), CommandPalette),
        (Char('C'), CommandPalette),
        (Char(':'), CommandPalette),
        (Char('p'), ProjectPalette),
        (Char('P'), ProjectPalette),
        (Char('v'), Viewer),
        (Char('V'), Viewer),
        (Char('s'), Shell),
        (Char('S'), Shell),
        (Char('e'), EditScrollback),
        (Char('E'), EditScrollback),
        // g/G reload config (r/R are taken by Restart above).
        (Char('g'), ReloadConfig),
        (Char('G'), ReloadConfig),
    ];
    entries
        .iter()
        .map(|(code, action)| (Chord::Leader(*code), Binding::always(*action)))
        .collect()
}

/// Parse a config key spelling into a [`Chord`] for the given table. Extends the
/// leader vocab (`input.rs`) with f1..f12, enter, tab, backtab, arrows, esc.
/// Leader chords drop any modifiers (code-only dispatch); global chords keep them.
pub(crate) fn parse_chord(spelling: &str, mode: ChordMode) -> Option<Chord> {
    let (modifiers, code) = parse_key_spelling(spelling)?;
    match mode {
        ChordMode::Leader => Some(Chord::Leader(code)),
        ChordMode::Global => Some(Chord::Global(code, modifiers)),
    }
}

/// Parse a key spelling into `(modifiers, code)`. Shared vocabulary for the leader
/// and keymap parsers: `ctrl-`/`control-`/`c-`/`alt-`/`option-` prefixes plus a
/// named-or-single-char remainder. Returns `None` on an unknown spelling.
pub(crate) fn parse_key_spelling(spelling: &str) -> Option<(KeyModifiers, KeyCode)> {
    let normalized = spelling
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '+'], "-");

    let mut modifiers = KeyModifiers::NONE;
    let mut rest = normalized.as_str();
    loop {
        if let Some(tail) = rest
            .strip_prefix("ctrl-")
            .or_else(|| rest.strip_prefix("control-"))
            .or_else(|| rest.strip_prefix("c-"))
        {
            modifiers |= KeyModifiers::CONTROL;
            rest = tail;
        } else if let Some(tail) = rest
            .strip_prefix("alt-")
            .or_else(|| rest.strip_prefix("option-"))
        {
            modifiers |= KeyModifiers::ALT;
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("shift-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = tail;
        } else {
            break;
        }
    }

    let code = parse_key_name(rest)?;
    Some((modifiers, code))
}

/// Parse the modifier-free remainder of a key spelling into a [`KeyCode`]. Shared
/// with `input.rs` so the leader and keymap parsers agree on key names.
pub(crate) fn parse_key_name(name: &str) -> Option<KeyCode> {
    let code = match name {
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "backtab" | "shift-tab" => KeyCode::BackTab,
        "enter" | "return" => KeyCode::Enter,
        "backspace" => KeyCode::Backspace,
        "esc" | "escape" => KeyCode::Esc,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        _ if name.starts_with('f') && name.len() >= 2 => {
            let n: u8 = name[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
        _ if name.chars().count() == 1 => KeyCode::Char(name.chars().next()?),
        _ => return None,
    };
    Some(code)
}

/// Render a chord as a display label. Generalizes the old `KeyBinding::label`
/// (which returned `?` for F-keys / BackTab / arrows) to cover the global table.
pub(crate) fn chord_label(chord: Chord) -> String {
    match chord {
        Chord::Leader(code) => key_code_label(code),
        Chord::Global(code, modifiers) => {
            let key = key_code_label(code);
            let mut prefix = String::new();
            if modifiers.contains(KeyModifiers::CONTROL) {
                prefix.push_str("Ctrl+");
            }
            if modifiers.contains(KeyModifiers::ALT) {
                prefix.push_str("Alt+");
            }
            if modifiers.contains(KeyModifiers::SHIFT) {
                prefix.push_str("Shift+");
            }
            format!("{prefix}{key}")
        }
    }
}

fn key_code_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ev(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_keymap_routes_all_builtin_leader_chords() {
        let map = default_keymap();
        // A representative chord from every leader action, code-only.
        let cases = [
            (KeyCode::Char('q'), BindAction::Quit),
            (KeyCode::Char('Q'), BindAction::Quit),
            (KeyCode::Char('r'), BindAction::Restart),
            (KeyCode::Char('x'), BindAction::Close),
            (KeyCode::Char('|'), BindAction::SplitHorizontal),
            (KeyCode::Char('-'), BindAction::SplitVertical),
            (KeyCode::Tab, BindAction::FocusNext),
            (KeyCode::Right, BindAction::FocusNext),
            (KeyCode::Char('n'), BindAction::FocusNext),
            (KeyCode::BackTab, BindAction::FocusPrev),
            (KeyCode::Left, BindAction::FocusPrev),
            (KeyCode::Char('>'), BindAction::ResizeGrow),
            (KeyCode::Char('<'), BindAction::ResizeShrink),
            (KeyCode::Char('z'), BindAction::Zoom),
            (KeyCode::Char('f'), BindAction::Float),
            (KeyCode::Char('t'), BindAction::ToggleStack),
            (KeyCode::Char('b'), BindAction::ToggleCompact),
            (KeyCode::Char('?'), BindAction::Help),
            (KeyCode::Char('h'), BindAction::Help),
            (KeyCode::Char('c'), BindAction::CommandPalette),
            (KeyCode::Char(':'), BindAction::CommandPalette),
            (KeyCode::Char('p'), BindAction::ProjectPalette),
            (KeyCode::Char('v'), BindAction::Viewer),
            (KeyCode::Char('s'), BindAction::Shell),
            (KeyCode::Char('e'), BindAction::EditScrollback),
        ];
        for (code, want) in cases {
            assert_eq!(
                map.lookup_leader(ev(code, KeyModifiers::NONE)),
                Some(want),
                "leader code {code:?}"
            );
        }
    }

    #[test]
    fn default_keymap_global_lookup_matches_old_table() {
        let map = default_keymap();
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;

        assert_eq!(
            map.lookup_global(false, ev(KeyCode::F(1), none)),
            Some(BindAction::Help)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::F(2), none)),
            Some(BindAction::CommandPalette)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::F(6), none)),
            Some(BindAction::FocusNext)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::F(10), none)),
            Some(BindAction::Quit)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::Enter, ctrl)),
            Some(BindAction::CommandPalette)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::Enter, alt)),
            Some(BindAction::CommandPalette)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::Tab, ctrl)),
            Some(BindAction::FocusNext)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::BackTab, ctrl)),
            Some(BindAction::FocusPrev)
        );
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::Tab, ctrl_shift)),
            Some(BindAction::FocusPrev)
        );
    }

    #[test]
    fn f_key_actions_have_non_fkey_leader_routes() {
        let map = default_keymap();
        let cases = [
            (KeyCode::Char('h'), BindAction::Help),
            (KeyCode::Char('c'), BindAction::CommandPalette),
            (KeyCode::Char('p'), BindAction::ProjectPalette),
            (KeyCode::Char('v'), BindAction::Viewer),
            (KeyCode::Char('s'), BindAction::Shell),
            (KeyCode::Tab, BindAction::FocusNext),
            (KeyCode::BackTab, BindAction::FocusPrev),
            (KeyCode::Char('z'), BindAction::Zoom),
            (KeyCode::Char('x'), BindAction::Close),
            (KeyCode::Char('q'), BindAction::Quit),
            (KeyCode::Char('g'), BindAction::ReloadConfig),
        ];
        for (code, action) in cases {
            assert_eq!(
                map.lookup_leader(ev(code, KeyModifiers::NONE)),
                Some(action),
                "{action:?} should not require an F-key"
            );
        }
    }

    #[test]
    fn global_lookup_is_shell_safe() {
        let map = default_keymap();
        let none = KeyModifiers::NONE;
        // Plain '?', plain Tab, Shift+F2 fall through to shell typing.
        assert_eq!(map.lookup_global(false, ev(KeyCode::Char('?'), none)), None);
        assert_eq!(map.lookup_global(false, ev(KeyCode::Tab, none)), None);
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::F(2), KeyModifiers::SHIFT)),
            None
        );
        // Viewer-gated Tab only fires when a viewer is focused.
        assert_eq!(
            map.lookup_global(true, ev(KeyCode::Tab, none)),
            Some(BindAction::FocusNext)
        );
        assert_eq!(
            map.lookup_global(true, ev(KeyCode::BackTab, none)),
            Some(BindAction::FocusPrev)
        );
    }

    #[test]
    fn leader_lookup_ignores_modifiers() {
        let map = default_keymap();
        // leader-then-Ctrl+q still Quits (code-only match).
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(BindAction::Quit)
        );
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('z'), KeyModifiers::ALT)),
            Some(BindAction::Zoom)
        );
    }

    #[test]
    fn chord_label_renders_fkeys_and_backtab() {
        assert_eq!(
            chord_label(Chord::Global(KeyCode::F(1), KeyModifiers::NONE)),
            "F1"
        );
        assert_eq!(
            chord_label(Chord::Global(KeyCode::BackTab, KeyModifiers::NONE)),
            "Shift+Tab"
        );
        assert_eq!(
            chord_label(Chord::Global(KeyCode::Enter, KeyModifiers::CONTROL)),
            "Ctrl+Enter"
        );
        assert_eq!(
            chord_label(Chord::Global(KeyCode::Enter, KeyModifiers::ALT)),
            "Alt+Enter"
        );
        assert_eq!(
            chord_label(Chord::Global(KeyCode::Char('/'), KeyModifiers::CONTROL)),
            "Ctrl+/"
        );
        assert_eq!(
            chord_label(Chord::Leader(KeyCode::Char('q'))),
            "Q".to_string()
        );
    }

    #[test]
    fn merge_overlay_replaces_one_binding_keeps_rest() {
        let mut map = default_keymap();
        let mut leader = BTreeMap::new();
        leader.insert("x".to_string(), "quit".to_string());
        let warning = map.merge_overlay(&BTreeMap::new(), &leader, None);
        assert_eq!(warning, None);
        // 'x' now quits; the rest of the leader table is untouched.
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(BindAction::Quit)
        );
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(BindAction::Quit)
        );
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('z'), KeyModifiers::NONE)),
            Some(BindAction::Zoom)
        );
    }

    #[test]
    fn unknown_action_name_in_overlay_drops_and_warns() {
        let mut map = default_keymap();
        let mut leader = BTreeMap::new();
        leader.insert("x".to_string(), "frobnicate".to_string());
        let warning = map.merge_overlay(&BTreeMap::new(), &leader, None).unwrap();
        assert!(warning.contains("frobnicate"), "{warning}");
        // 'x' keeps its default Close binding.
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(BindAction::Close)
        );
    }

    #[test]
    fn unknown_key_name_in_overlay_drops_and_warns() {
        let mut map = default_keymap();
        let mut leader = BTreeMap::new();
        leader.insert("notakey".to_string(), "quit".to_string());
        let warning = map.merge_overlay(&BTreeMap::new(), &leader, None).unwrap();
        assert!(warning.contains("notakey"), "{warning}");
    }

    #[test]
    fn default_keymap_overlay_with_no_entries_has_no_warning() {
        let mut map = default_keymap();
        let warning = map.merge_overlay(&BTreeMap::new(), &BTreeMap::new(), None);
        assert_eq!(warning, None);
    }

    #[test]
    fn global_parse_keeps_modifiers_leader_drops_them() {
        assert_eq!(
            parse_chord("ctrl-enter", ChordMode::Global),
            Some(Chord::Global(KeyCode::Enter, KeyModifiers::CONTROL))
        );
        // Leader parse keeps only the code.
        assert_eq!(
            parse_chord("ctrl-q", ChordMode::Leader),
            Some(Chord::Leader(KeyCode::Char('q')))
        );
        assert_eq!(
            parse_chord("f5", ChordMode::Global),
            Some(Chord::Global(KeyCode::F(5), KeyModifiers::NONE))
        );
    }

    #[test]
    fn duplicate_chord_in_one_table_warns_and_names_chord() {
        // `esc` and `escape` are distinct spellings that normalize to ONE Chord, so
        // the second one in a single table is the residual in-table conflict (TOML
        // rejects identical-string keys, never these distinct spellings).
        let mut map = default_keymap();
        let mut global = BTreeMap::new();
        global.insert("esc".to_string(), "help".to_string());
        global.insert("escape".to_string(), "quit".to_string());
        let warning = map
            .merge_overlay(&global, &BTreeMap::new(), None)
            .expect("duplicate chord should warn");
        assert!(warning.contains("duplicate"), "{warning}");
        // BTreeMap orders "esc" before "escape": first wins, the duplicate drops.
        assert_eq!(
            map.lookup_global(false, ev(KeyCode::Esc, KeyModifiers::NONE)),
            Some(BindAction::Help)
        );
    }

    #[test]
    fn leader_override_colliding_with_configured_leader_warns() {
        // Default leader CtrlSpace → representative Char(' ') (decision 13). A
        // [keymap].leader "space"=... entry collides and is dropped with a warning.
        let leader_chord = leader_representative_chord("ctrl-space");
        assert_eq!(leader_chord, Some(Chord::Leader(KeyCode::Char(' '))));
        let mut map = default_keymap();
        let mut leader = BTreeMap::new();
        leader.insert("space".to_string(), "quit".to_string());
        let warning = map
            .merge_overlay(&BTreeMap::new(), &leader, leader_chord)
            .expect("leader collision should warn");
        assert!(warning.contains("leader"), "{warning}");
        // The colliding chord was dropped: Char(' ') is not bound to Quit.
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char(' '), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn valid_binding_alongside_conflicting_one_keeps_valid() {
        // A leader-colliding entry drops, but a sibling valid override in the same
        // pass still applies (a single conflict never wedges the rest of the table).
        let leader_chord = leader_representative_chord("ctrl-space");
        let mut map = default_keymap();
        let mut leader = BTreeMap::new();
        leader.insert("space".to_string(), "quit".to_string()); // collides → dropped
        leader.insert("x".to_string(), "quit".to_string()); // valid override
        let warning = map.merge_overlay(&BTreeMap::new(), &leader, leader_chord);
        assert!(warning.is_some());
        assert_eq!(
            map.lookup_leader(ev(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(BindAction::Quit)
        );
    }

    #[test]
    fn default_keymap_has_no_conflicts() {
        // An empty overlay against the default leader produces no warning: the
        // built-in tables carry no in-table duplicate or leader collision.
        let mut map = default_keymap();
        let leader_chord = leader_representative_chord("ctrl-space");
        let warning = map.merge_overlay(&BTreeMap::new(), &BTreeMap::new(), leader_chord);
        assert_eq!(warning, None);
    }
}
