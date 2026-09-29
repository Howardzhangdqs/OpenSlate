//! Action — the single frozen vocabulary of everything the TUI can do.
//!
//! EVERY input source (crossterm events, engine bridge events, timers) is
//! translated into an [`Action`] and funneled through one dispatcher; the
//! App routes actions to components by focus and modal state. This enum is
//! **interface-frozen as of P2b**: the P3 lanes may add component internals
//! but must not change these variants (they are already a superset of what
//! P3 needs).
//!
//! Two mapping rules are intentionally state-dependent and live in the App
//! dispatcher rather than here (so [`map_event`] stays a pure, testable
//! table):
//!
//! * `Ctrl+C` maps to [`Action::CancelTurn`]. The App interprets it per the
//!   design brief: approval modal active → deny the pending approval AND
//!   cancel the turn; turn running → cancel the turn; idle → escalate to
//!   [`Action::RequestQuit`] (exit-confirmation modal).
//! * Arrow keys map to the *input* interpretation
//!   ([`Action::InputHistoryPrev`]/[`Action::InputHistoryNext`]). When the
//!   transcript holds focus, the App re-routes them to
//!   [`Action::ScrollUp`]/[`Action::ScrollDown`]; likewise `Home`/`End`
//!   become `ScrollTop`/`ScrollBottom` and `g`/`G` (typed as
//!   [`Action::InputChar`]) become `ScrollTop`/`ScrollBottom`.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// The user's answer to a pending tool-approval request (y/n/a, mirroring
/// the REPL's `InteractiveApproval`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// `y` — approve this one call.
    Approve,
    /// `n` — deny this one call (the engine continues with a denial tool
    /// result).
    Deny,
    /// `a` — approve this call and allowlist the tool for the rest of the
    /// session (later calls to the same tool stop prompting).
    ApproveAll,
}

/// One unit of work for the dispatcher. See the module docs for the frozen
/// contract.
// The Engine variant necessarily carries the (large) engine payload; the
// enum is interface-frozen, and actions are dispatched at most a few
// dozen per wake — the size difference is not worth boxing the surface.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Action {
    // ── Lifecycle ────────────────────────────────────────────────────────
    /// Exit immediately (no confirmation): `/exit`, confirmed quit.
    Quit,
    /// Ask to quit → enter the exit-confirmation modal (idle Ctrl+C).
    RequestQuit,
    /// Dismiss the exit-confirmation modal (stay in the session).
    CancelQuit,
    /// Timer tick (250 ms idle / 100 ms while a turn is running). Advances
    /// the spinner; drives nothing else directly.
    Tick,
    /// Force a redraw (Ctrl+L, terminal resize).
    Redraw,

    // ── Input editing ────────────────────────────────────────────────────
    /// Insert one character at the cursor (char-boundary safe).
    InputChar(char),
    /// Insert a line break (Alt+Enter / Ctrl+J).
    InputNewline,
    /// Delete the character before the cursor (joins lines at col 0).
    InputBackspace,
    /// Delete the word before the cursor (Ctrl+W).
    InputDeleteWord,
    /// Move the cursor one cell left (across line boundaries).
    InputCursorLeft,
    /// Move the cursor one cell right (across line boundaries).
    InputCursorRight,
    /// Move the cursor one display row up.
    InputCursorUp,
    /// Move the cursor one display row down.
    InputCursorDown,
    /// Move the cursor to the start of the current line.
    InputHome,
    /// Move the cursor to the end of the current line.
    InputEnd,
    /// Previous input-history entry (only fires on the editor's first row;
    /// the editor falls back to an in-text cursor move otherwise).
    InputHistoryPrev,
    /// Next input-history entry (only fires on the editor's last row).
    InputHistoryNext,
    /// Raw bracketed-paste payload (may contain newlines; inserted verbatim).
    PasteText(String),
    /// Submit the editor contents as a new turn.
    SubmitInput,

    // ── Focus & overlays ─────────────────────────────────────────────────
    /// Cycle focus: input ↔ transcript ↔ sidebar (non-modal only).
    FocusNext,
    /// Toggle the right sidebar (Ctrl+T; auto-hidden under 100 columns).
    ToggleSidebar,
    /// Toggle terminal mouse capture at runtime (Ctrl+M / `/mouse`).
    /// Capture ON (default) → the wheel scrolls the chat; OFF → the
    /// terminal's native text selection works (the wheel degrades to
    /// ↑/↓ keys). The App owns the flag; the run loop applies the
    /// Enable/DisableMouseCapture escape to stdout on flip.
    ToggleMouseCapture,
    /// Copy the LAST assistant message's raw markdown to the system
    /// clipboard via an OSC 52 escape written to stdout (Ctrl+Y /
    /// `/copy`) — selection-free copy that works through SSH/tmux.
    CopyLast,
    /// Open the help overlay (`?` with an empty input).
    ToggleHelp,
    /// Close the topmost overlay (Esc), or cancel scroll pinning.
    DismissOverlay,

    // ── Transcript scrolling ─────────────────────────────────────────────
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    /// Mouse wheel notch up at terminal `(column, row)`. ALWAYS targets
    /// the transcript regardless of focus — the wheel is positional,
    /// unlike keyboard arrows which keep their focus-routed
    /// history/scroll duality. slash-3: the App swallows the notch
    /// when it lands inside the completion overlay rect (the list is
    /// not scrollable-by-wheel and must not scroll the covered
    /// transcript beneath).
    WheelScrollUp(u16, u16),
    /// Mouse wheel notch down at `(column, row)`. See
    /// [`Action::WheelScrollUp`].
    WheelScrollDown(u16, u16),
    /// Left-button mouse click at terminal `(column, row)`. Positional
    /// like the wheel: the App routes it straight to the transcript —
    /// a hit on the pinned `+N ↓ 回到底部` hint jumps back to the bottom
    /// (re-follow), a hit on a reasoning block or tool row toggles its
    /// expansion — never through focus routing. Since select-1 this is
    /// the DERIVED click: the App fires it from a
    /// press ([`Action::MouseDown`]) + release ([`Action::MouseUp`])
    /// pair that never dragged, so every click behavior survives
    /// drag-to-select unchanged.
    Click(u16, u16),
    /// Left-button mouse press at terminal `(column, row)` — the
    /// select-1 drag-to-select anchor. The App records the anchor in
    /// the transcript and SUPPRESSES the legacy [`Action::Click`]
    /// until the release proves the press never dragged.
    MouseDown(u16, u16),
    /// Left-button drag (motion while pressed) at terminal
    /// `(column, row)`. Updates the selection cursor; the first
    /// ≥1-cell displacement promotes the press into a live
    /// selection (click suppression locks in for the gesture).
    MouseDrag(u16, u16),
    /// Left-button release at terminal `(column, row)`. No drag →
    /// the legacy [`Action::Click`] fires; a completed selection →
    /// the App extracts the covered text and copies it through the
    /// existing chain (auto-copy on release).
    MouseUp(u16, u16),
    /// Plain mouse motion (no button) at terminal `(column, row)` —
    /// interactive-1 hover tracking. `EnableMouseCapture` carries
    /// `?1003h` (any-event tracking), so crossterm delivers
    /// `MouseEventKind::Moved` for every cell crossing; the App
    /// hit-tests the last-rendered button rectangles and repaints the
    /// hovered segment (a miss clears). Coalesced batch-wise — only
    /// the final resting position of a run matters.
    MouseMove(u16, u16),

    // ── Engine turn lifecycle ────────────────────────────────────────────
    /// Start a turn with the given prompt (also used by the input component
    /// to hand a submitted buffer to the App, and by pending-input
    /// auto-submit after `TurnDone`).
    StartTurn(String),
    /// Cancel the running turn (Ctrl+C while running; see module docs for
    /// the modal-dependent interpretations).
    CancelTurn,
    /// An event from the engine bridge (streaming deltas, tool progress,
    /// approval requests, turn completion).
    Engine(crate::event::TuiEvent),
    /// Answer the front approval request (y/n/a).
    ApprovalRespond(ApprovalChoice),
}

/// Manual `PartialEq`: `Engine(TurnDone)` carries a non-comparable
/// `RunManager`, so that arm (and the enum as a whole) cannot derive.
/// `TurnDone` variants compare equal only within the same Ok/Err arm —
/// payload equality is intentionally not required (tests assert the
/// variant, then inspect the payload).
impl PartialEq for Action {
    fn eq(&self, other: &Self) -> bool {
        use Action::*;
        match (self, other) {
            (Quit, Quit)
            | (RequestQuit, RequestQuit)
            | (CancelQuit, CancelQuit)
            | (Tick, Tick)
            | (Redraw, Redraw)
            | (InputNewline, InputNewline)
            | (InputBackspace, InputBackspace)
            | (InputDeleteWord, InputDeleteWord)
            | (InputCursorLeft, InputCursorLeft)
            | (InputCursorRight, InputCursorRight)
            | (InputCursorUp, InputCursorUp)
            | (InputCursorDown, InputCursorDown)
            | (InputHome, InputHome)
            | (InputEnd, InputEnd)
            | (InputHistoryPrev, InputHistoryPrev)
            | (InputHistoryNext, InputHistoryNext)
            | (SubmitInput, SubmitInput)
            | (FocusNext, FocusNext)
            | (ToggleSidebar, ToggleSidebar)
            | (ToggleMouseCapture, ToggleMouseCapture)
            | (CopyLast, CopyLast)
            | (ToggleHelp, ToggleHelp)
            | (DismissOverlay, DismissOverlay)
            | (ScrollUp, ScrollUp)
            | (ScrollDown, ScrollDown)
            | (ScrollPageUp, ScrollPageUp)
            | (ScrollPageDown, ScrollPageDown)
            | (ScrollTop, ScrollTop)
            | (ScrollBottom, ScrollBottom)
            | (CancelTurn, CancelTurn) => true,
            (WheelScrollUp(ac, ar), WheelScrollUp(bc, br))
            | (WheelScrollDown(ac, ar), WheelScrollDown(bc, br)) => ac == bc && ar == br,
            (Click(ac, ar), Click(bc, br)) => ac == bc && ar == br,
            (MouseDown(ac, ar), MouseDown(bc, br)) => ac == bc && ar == br,
            (MouseDrag(ac, ar), MouseDrag(bc, br)) => ac == bc && ar == br,
            (MouseUp(ac, ar), MouseUp(bc, br)) => ac == bc && ar == br,
            (MouseMove(ac, ar), MouseMove(bc, br)) => ac == bc && ar == br,
            (InputChar(a), InputChar(b)) => a == b,
            (PasteText(a), PasteText(b)) => a == b,
            (StartTurn(a), StartTurn(b)) => a == b,
            (ApprovalRespond(a), ApprovalRespond(b)) => a == b,
            (Engine(a), Engine(b)) => a == b,
            _ => false,
        }
    }
}

/// Translate a crossterm event into an [`Action`].
///
/// Pure table lookup — no state. Key events are filtered through
/// [`Event::as_key_press_event`] so key-repeat/release kinds (kitty
/// protocol) are ignored, and bracketed paste arrives as `Event::Paste`.
/// Wheel events map to [`Action::WheelScrollUp`]/[`Action::WheelScrollDown`];
/// the left button's press/drag/release map to
/// [`Action::MouseDown`]/[`Action::MouseDrag`]/[`Action::MouseUp`]
/// (carrying the terminal column/row — the select-1 drag-to-select
/// gesture; the App decides at release whether the press was a click
/// or a selection) and plain motion maps to
/// [`Action::MouseMove`] (interactive-1 hover). Other mouse kinds
/// (other buttons), focus changes and unrecognized keys return
/// `None`.
pub fn map_event(event: &Event) -> Option<Action> {
    match event {
        Event::Paste(text) => Some(Action::PasteText(text.clone())),
        Event::Resize(_, _) => Some(Action::Redraw),
        Event::Key(_) => map_key(&event.as_key_press_event()?),
        Event::Mouse(mouse) => match mouse.kind {
            crossterm::event::MouseEventKind::ScrollUp => {
                Some(Action::WheelScrollUp(mouse.column, mouse.row))
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                Some(Action::WheelScrollDown(mouse.column, mouse.row))
            }
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                Some(Action::MouseDown(mouse.column, mouse.row))
            }
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                Some(Action::MouseDrag(mouse.column, mouse.row))
            }
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                Some(Action::MouseUp(mouse.column, mouse.row))
            }
            crossterm::event::MouseEventKind::Moved => {
                Some(Action::MouseMove(mouse.column, mouse.row))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Key-level mapping (Press kind already filtered). See the module docs for
/// the state-dependent reinterpretations the App applies on top.
fn map_key(key: &KeyEvent) -> Option<Action> {
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT);
    let alt = mods.contains(KeyModifiers::ALT) && !mods.contains(KeyModifiers::CONTROL);

    match key.code {
        // ── Global keys ──
        KeyCode::Tab | KeyCode::BackTab => Some(Action::FocusNext),
        KeyCode::Esc => Some(Action::DismissOverlay),
        KeyCode::Enter if alt || ctrl => Some(Action::InputNewline),
        KeyCode::Enter => Some(Action::SubmitInput),
        KeyCode::Char('c') if ctrl => Some(Action::CancelTurn),
        KeyCode::Char('j') if ctrl => Some(Action::InputNewline),
        KeyCode::Char('l') if ctrl => Some(Action::Redraw),
        KeyCode::Char('t') if ctrl => Some(Action::ToggleSidebar),
        KeyCode::Char('m') if ctrl => Some(Action::ToggleMouseCapture),
        KeyCode::Char('y') if ctrl => Some(Action::CopyLast),
        KeyCode::Char('w') if ctrl => Some(Action::InputDeleteWord),

        // ── Cursor / history (input interpretation; re-routed by focus) ──
        KeyCode::Left => Some(Action::InputCursorLeft),
        KeyCode::Right => Some(Action::InputCursorRight),
        KeyCode::Up => Some(Action::InputHistoryPrev),
        KeyCode::Down => Some(Action::InputHistoryNext),
        KeyCode::Home => Some(Action::InputHome),
        KeyCode::End => Some(Action::InputEnd),
        KeyCode::Backspace => Some(Action::InputBackspace),

        // ── Scrolling (transcript-global) ──
        KeyCode::PageUp => Some(Action::ScrollPageUp),
        KeyCode::PageDown => Some(Action::ScrollPageDown),

        // ── Plain characters (Alt+char still types: IME sequences arrive
        //    as plain Char runs; crossterm has no preedit). Unmapped
        //    Ctrl+char combinations type nothing. ──
        KeyCode::Char(_) if ctrl => None,
        KeyCode::Char(c) => Some(Action::InputChar(c)),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn plain(code: KeyCode) -> Event {
        key(code, KeyModifiers::NONE)
    }

    fn char(c: char) -> Event {
        plain(KeyCode::Char(c))
    }

    #[test]
    fn plain_chars_map_to_input_char() {
        assert_eq!(map_event(&char('a')), Some(Action::InputChar('a')));
        assert_eq!(map_event(&char('?')), Some(Action::InputChar('?')));
        assert_eq!(map_event(&char('Z')), Some(Action::InputChar('Z')));
        assert_eq!(map_event(&char('你')), Some(Action::InputChar('你')));
    }

    #[test]
    fn alt_char_still_types() {
        assert_eq!(
            map_event(&key(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(Action::InputChar('x'))
        );
    }

    #[test]
    fn enter_submits_and_alt_enter_newlines() {
        assert_eq!(map_event(&plain(KeyCode::Enter)), Some(Action::SubmitInput));
        assert_eq!(
            map_event(&key(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Action::InputNewline)
        );
    }

    #[test]
    fn ctrl_shortcuts() {
        let ctrl = |c: char| key(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(map_event(&ctrl('c')), Some(Action::CancelTurn));
        assert_eq!(map_event(&ctrl('j')), Some(Action::InputNewline));
        assert_eq!(map_event(&ctrl('l')), Some(Action::Redraw));
        assert_eq!(map_event(&ctrl('t')), Some(Action::ToggleSidebar));
        assert_eq!(map_event(&ctrl('m')), Some(Action::ToggleMouseCapture));
        assert_eq!(map_event(&ctrl('y')), Some(Action::CopyLast));
        assert_eq!(map_event(&ctrl('w')), Some(Action::InputDeleteWord));
        // Ctrl+anything-else does not type.
        assert_eq!(map_event(&ctrl('a')), None);
    }

    #[test]
    fn arrows_map_to_input_semantics() {
        assert_eq!(
            map_event(&plain(KeyCode::Up)),
            Some(Action::InputHistoryPrev)
        );
        assert_eq!(
            map_event(&plain(KeyCode::Down)),
            Some(Action::InputHistoryNext)
        );
        assert_eq!(
            map_event(&plain(KeyCode::Left)),
            Some(Action::InputCursorLeft)
        );
        assert_eq!(
            map_event(&plain(KeyCode::Right)),
            Some(Action::InputCursorRight)
        );
        assert_eq!(map_event(&plain(KeyCode::Home)), Some(Action::InputHome));
        assert_eq!(map_event(&plain(KeyCode::End)), Some(Action::InputEnd));
    }

    #[test]
    fn scroll_and_navigation_keys() {
        assert_eq!(
            map_event(&plain(KeyCode::PageUp)),
            Some(Action::ScrollPageUp)
        );
        assert_eq!(
            map_event(&plain(KeyCode::PageDown)),
            Some(Action::ScrollPageDown)
        );
        assert_eq!(map_event(&plain(KeyCode::Tab)), Some(Action::FocusNext));
        assert_eq!(
            map_event(&key(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(Action::FocusNext)
        );
        assert_eq!(
            map_event(&plain(KeyCode::Esc)),
            Some(Action::DismissOverlay)
        );
        assert_eq!(
            map_event(&plain(KeyCode::Backspace)),
            Some(Action::InputBackspace)
        );
    }

    #[test]
    fn paste_maps_to_paste_text() {
        assert_eq!(
            map_event(&Event::Paste("line1\nline2".into())),
            Some(Action::PasteText("line1\nline2".into()))
        );
    }

    #[test]
    fn resize_maps_to_redraw() {
        assert_eq!(map_event(&Event::Resize(80, 24)), Some(Action::Redraw));
    }

    #[test]
    fn wheel_maps_to_scroll_and_other_mouse_is_ignored() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mk = |kind: MouseEventKind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(
            map_event(&mk(MouseEventKind::ScrollUp)),
            Some(Action::WheelScrollUp(1, 1))
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::ScrollDown)),
            Some(Action::WheelScrollDown(1, 1))
        );
        // Other buttons stay unmapped; so do focus changes. (The LEFT
        // button's press/drag/release map — see the test below — and
        // plain motion maps to the interactive-1 hover action.)
        assert_eq!(
            map_event(&mk(MouseEventKind::Down(MouseButton::Right))),
            None
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Drag(MouseButton::Right))),
            None
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Moved)),
            Some(Action::MouseMove(1, 1))
        );
        assert_eq!(map_event(&Event::FocusGained), None);
    }

    #[test]
    fn left_mouse_maps_down_drag_up_with_coordinates() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mk = |kind: MouseEventKind, column: u16, row: u16| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // The select-1 gesture carries the terminal column/row
        // through verbatim on all three phases.
        assert_eq!(
            map_event(&mk(MouseEventKind::Down(MouseButton::Left), 7, 3)),
            Some(Action::MouseDown(7, 3))
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Down(MouseButton::Left), 0, 0)),
            Some(Action::MouseDown(0, 0))
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Drag(MouseButton::Left), 9, 4)),
            Some(Action::MouseDrag(9, 4))
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Up(MouseButton::Left), 7, 3)),
            Some(Action::MouseUp(7, 3))
        );
        // Plain motion carries its coordinates too (interactive-1
        // hover hit-testing).
        assert_eq!(
            map_event(&mk(MouseEventKind::Moved, 11, 5)),
            Some(Action::MouseMove(11, 5))
        );
        // Every other button stays unmapped on every phase.
        assert_eq!(
            map_event(&mk(MouseEventKind::Up(MouseButton::Right), 7, 3)),
            None
        );
        assert_eq!(
            map_event(&mk(MouseEventKind::Down(MouseButton::Middle), 7, 3)),
            None
        );
    }

    #[test]
    fn key_release_kinds_are_filtered() {
        // as_key_press_event() drops non-Press kinds (kitty protocol).
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        ));
        assert_eq!(map_event(&release), None);
    }
}
