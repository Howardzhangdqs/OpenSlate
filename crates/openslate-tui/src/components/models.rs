//! `/provider` management overlay (model-mgmt-2) — Providers / Models /
//! Levels over the layered config (local active + global library).
//!
//! Architecture: the component is a self-contained UI state machine
//! (list ↔ form ↔ entry picker ↔ delete confirm) that holds a SNAPSHOT
//! of the config rows. It never mutates config state and never touches
//! the filesystem — [`Self::on_key`] returns plain-data
//! [`ModelsIntent`]s and the App executes the `openslate_core::config`
//! persist writers, reloads the merged config and hot-swaps
//! `self.config` (write routing: provider/model → global library,
//! levels → active file, `.env` → global config dir; fallback to the
//! active file when there is no global library).
//!
//! Keys (all swallowed while the overlay is up — same modal-guard shape
//! as the agents panel): `1/2/3` switch sections, `↑/↓` move the row
//! cursor, `a` add, `e` edit, `d` delete (two-step `y` confirm),
//! `Enter`/`r` rebind a level, Tab/`↑↓` walk form fields, `空格`
//! toggles booleans, `←→` cycles choice fields, Esc backs out of a
//! sub-mode first and closes the overlay from the list.
//! adapter-combo-1: the provider form's Adapter field is a COMBO —
//! its candidate dropdown opens on focus / text change (`↑↓`/滚轮
//! move the selection instead of the field, Enter/Tab apply it, a
//! click applies without submitting, Esc closes the dropdown before
//! the form).
//!
//! Rendering uses theme slots + IconSet glyphs only (no hardcoded
//! colors, no PUA codepoints).

use std::cell::RefCell;

use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use openslate_core::config::{ModelConfig, OpenSlateConfig, ProviderConfig};

use super::AppCtx;
use crate::action::Action;
use crate::panel::clip;

/// Levels the config semantics REQUIRE (`main` always; `fast` for
/// compaction). Rendered first, marked `required`, and not deletable.
pub const REQUIRED_LEVELS: [&str; 2] = ["main", "fast"];

/// adapter-combo-1: the provider form's Adapter combo candidates —
/// the four PROTOCOL adapters of `AdapterKind::from_lower_str`
/// (openslate-app/src/provider.rs; the remaining genai `AdapterKind`
/// names are openai wire-protocol variants, not protocol choices). An
/// EMPTY field value stays `None` in `ProviderConfig::adapter`, which
/// the provider build defaults to `openai` — hence the「（默认）」hint
/// on the first row while the text is empty.
const ADAPTER_PROTOCOLS: [&str; 4] = ["openai", "anthropic", "gemini", "ollama"];

/// adapter-combo-1: the form's leading columns (marker 2 + label clip
/// 16) — the dropdown's left edge aligns with the field VALUE column
/// (self-consistent with [`ModelsComponent::form_lines`]' fixed
/// leading layout; the value itself starts right after the ragged
/// label, so a fixed column is the stable choice).
const COMBO_INSET_COLS: u16 = 18;

/// Which section of the overlay is on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Providers,
    Models,
    Levels,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Providers, Tab::Models, Tab::Levels];

    fn label(self) -> &'static str {
        match self {
            Tab::Providers => "Providers",
            Tab::Models => "Models",
            Tab::Levels => "Levels",
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Providers => 0,
            Tab::Models => 1,
            Tab::Levels => 2,
        }
    }
}

/// Overlay interaction mode (one level deep — every sub-mode returns to
/// [`Mode::List`] on Esc before the overlay closes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    ProviderForm,
    ModelForm,
    LevelNameForm,
    PickEntry,
    ConfirmDelete,
}

/// What a delete confirmation will remove (plain data; the App runs the
/// reference guards).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingDelete {
    Provider(String),
    Model(String),
    Level(String),
}

impl PendingDelete {
    fn subject(&self) -> &str {
        match self {
            PendingDelete::Provider(n) | PendingDelete::Model(n) | PendingDelete::Level(n) => n,
        }
    }

    fn into_change(self) -> ModelsChange {
        match self {
            PendingDelete::Provider(n) => ModelsChange::RemoveProvider { name: n },
            PendingDelete::Model(n) => ModelsChange::RemoveModel { entry: n },
            PendingDelete::Level(n) => ModelsChange::RemoveLevel { level: n },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Toggle,
    Choice,
    /// adapter-combo-1: a text field whose value feeds a candidate
    /// dropdown ([`ADAPTER_PROTOCOLS`]) — editable text (the
    /// free-text escape hatch) plus the open/close selection state in
    /// [`FormState`].
    Combo,
}

/// One form field. `value` is the text content (char-indexed `cursor`);
/// `on`/`options`+`selected` serve the Toggle/Choice kinds.
#[derive(Debug, Clone)]
struct Field {
    label: &'static str,
    kind: FieldKind,
    value: String,
    cursor: usize,
    on: bool,
    options: Vec<String>,
    selected: usize,
    secret: bool,
    locked: bool,
}

impl Field {
    fn text(label: &'static str, value: &str) -> Self {
        let cursor = value.chars().count();
        Self {
            label,
            kind: FieldKind::Text,
            value: value.to_owned(),
            cursor,
            on: false,
            options: Vec::new(),
            selected: 0,
            secret: false,
            locked: false,
        }
    }

    fn toggle(label: &'static str, on: bool) -> Self {
        Self {
            label,
            kind: FieldKind::Toggle,
            value: String::new(),
            cursor: 0,
            on,
            options: Vec::new(),
            selected: 0,
            secret: false,
            locked: false,
        }
    }

    fn choice(label: &'static str, options: Vec<String>, selected: usize) -> Self {
        Self {
            label,
            kind: FieldKind::Choice,
            value: String::new(),
            cursor: 0,
            on: false,
            options,
            selected,
            secret: false,
            locked: false,
        }
    }

    /// adapter-combo-1: an editable text field with the adapter
    /// candidate dropdown. `value` prefills as-is — a value outside
    /// [`ADAPTER_PROTOCOLS`] still displays and submits (the dropdown
    /// simply filters to nothing).
    fn combo(label: &'static str, value: &str) -> Self {
        let cursor = value.chars().count();
        Self {
            label,
            kind: FieldKind::Combo,
            value: value.to_owned(),
            cursor,
            on: false,
            options: Vec::new(),
            selected: 0,
            secret: false,
            locked: false,
        }
    }

    fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    fn locked(mut self) -> Self {
        self.locked = true;
        self
    }
}

/// An open form (provider / model / new-level-name).
#[derive(Debug, Clone)]
struct FormState {
    kind: Mode,
    /// The provider/entry name being edited (`None` = add form).
    editing: Option<String>,
    fields: Vec<Field>,
    focus: usize,
    error: Option<String>,
    /// adapter-combo-1: the FOCUSED Combo field's dropdown is open.
    /// Only meaningful while the focused field is a Combo — every
    /// focus change re-normalizes it ([`combo_focus_resync`]).
    combo_open: bool,
    /// adapter-combo-1: the selected row, indexing the focused Combo
    /// field's CURRENT filtered candidate list
    /// ([`combo_candidates`]) — clamped on every text change
    /// ([`combo_clamp`]).
    combo_selected: usize,
}

/// The entry picker for level (re)binding.
#[derive(Debug, Clone)]
struct PickState {
    level: String,
    cursor: usize,
}

/// One flattened level row (required levels first, then sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LevelRow {
    name: String,
    entry: Option<String>,
    required: bool,
    /// `(provider, model_id)` when the entry resolves.
    resolved: Option<(String, String)>,
}

/// Plain-data change requests the App executes through the persist
/// layer (`openslate_core::config::persist`). No `PartialEq`: the
/// config structs do not implement it (tests assert via `matches!`).
#[derive(Debug, Clone)]
pub enum ModelsChange {
    /// `env_key` = `(var, value)` — a pasted API key to store in the
    /// global config dir's `.env` alongside the upsert.
    UpsertProvider {
        name: String,
        cfg: ProviderConfig,
        env_key: Option<(String, String)>,
    },
    UpsertModel {
        entry: String,
        cfg: ModelConfig,
    },
    SetLevel {
        level: String,
        entry: String,
    },
    RemoveProvider {
        name: String,
    },
    RemoveModel {
        entry: String,
    },
    RemoveLevel {
        level: String,
    },
}

/// Follow-up request from [`ModelsComponent::on_key`].
#[derive(Debug, Clone)]
pub enum ModelsIntent {
    /// Handled internally (navigation/editing); keep the overlay open.
    None,
    /// Close the overlay.
    Close,
    /// Persist this change through the App's save flow.
    Commit(ModelsChange),
}

/// The `/provider` overlay component. App-driven (inherent methods, like
/// the agents panel's `set_root`/`on_delegate_start` seam) — it is
/// never focus-routed, so it does not implement [`super::Component`].
#[derive(Debug)]
pub struct ModelsComponent {
    tab: Tab,
    cursors: [usize; 3],
    mode: Mode,
    providers: Vec<(String, ProviderConfig)>,
    models: Vec<(String, ModelConfig)>,
    levels: Vec<LevelRow>,
    form: Option<FormState>,
    pick: Option<PickState>,
    confirm: Option<PendingDelete>,
    /// In-panel refusal message (warning, rendered in the footer slot);
    /// cleared by the next state-changing key.
    flash: Option<String>,
    /// adapter-combo-1: TERMINAL-ABSOLUTE hit rects of the combo
    /// dropdown rows the LAST RENDER drew (empty while the dropdown
    /// is closed) — the mouse handlers hit-test against them. Interior
    /// mutability because `render` takes `&self` (the transcript's
    /// `hint_hit_rect` pattern: render records, `on_key` consumes).
    combo_row_rects: RefCell<Vec<Rect>>,
    /// adapter-combo-1: the dropdown row under the mouse (`None` when
    /// not hovering one) — pure presentation for the next render; set
    /// and cleared by [`Self::on_form_mouse`].
    combo_hover: Option<usize>,
    /// adapter-combo-1: the dropdown row a left-press landed on; the
    /// release applies it iff it lands on the SAME row and the gesture
    /// never dragged (interactive-1's button contract).
    combo_press: Option<usize>,
}

impl Default for ModelsComponent {
    fn default() -> Self {
        Self {
            tab: Tab::Providers,
            cursors: [0; 3],
            mode: Mode::List,
            providers: Vec::new(),
            models: Vec::new(),
            levels: Vec::new(),
            form: None,
            pick: None,
            confirm: None,
            flash: None,
            combo_row_rects: RefCell::new(Vec::new()),
            combo_hover: None,
            combo_press: None,
        }
    }
}

impl ModelsComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the row snapshot from the (merged) config. Called on
    /// open and after every commit attempt (success or failure) so the
    /// snapshot always matches the config the App holds.
    pub fn sync(&mut self, config: &OpenSlateConfig) {
        let mut providers: Vec<(String, ProviderConfig)> = config
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        providers.sort_by(|a, b| a.0.cmp(&b.0));
        let mut models: Vec<(String, ModelConfig)> = config
            .models
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        models.sort_by(|a, b| a.0.cmp(&b.0));

        let mut levels: Vec<LevelRow> = Vec::new();
        for name in REQUIRED_LEVELS {
            levels.push(Self::level_row(config, name, true));
        }
        let mut custom: Vec<&String> = config
            .levels
            .keys()
            .filter(|n| !REQUIRED_LEVELS.contains(&n.as_str()))
            .collect();
        custom.sort();
        for name in custom {
            levels.push(Self::level_row(config, name, false));
        }

        self.providers = providers;
        self.models = models;
        self.levels = levels;
    }

    fn level_row(config: &OpenSlateConfig, name: &str, required: bool) -> LevelRow {
        let entry = config.levels.get(name).cloned();
        let resolved = entry.as_ref().and_then(|e| {
            config
                .models
                .get(e)
                .map(|m| (m.provider.clone(), m.model.clone()))
        });
        LevelRow {
            name: name.to_owned(),
            entry,
            required,
            resolved,
        }
    }

    /// Fresh-open presentation: back to the list, first section.
    pub fn reset_view(&mut self) {
        self.tab = Tab::Providers;
        self.cursors = [0, 0, 0];
        self.mode = Mode::List;
        self.form = None;
        self.pick = None;
        self.confirm = None;
        self.flash = None;
        // adapter-combo-1: no dropdown rows survive a view reset.
        self.combo_hover = None;
        self.combo_press = None;
    }

    // ── row accessors ───────────────────────────────────────────────────

    /// adapter-combo-1: the combo dropdown's last-rendered row rects
    /// (dispatch-level test observer — the App forwards MouseMoves
    /// into the component; this is how app.rs tests see the rows).
    pub fn combo_row_rects(&self) -> Vec<Rect> {
        self.combo_row_rects.borrow().clone()
    }

    /// adapter-combo-1: the dropdown row currently under the mouse
    /// (dispatch-level test observer).
    pub fn combo_hover(&self) -> Option<usize> {
        self.combo_hover
    }

    /// The open form's field value at `index` (dispatch-level test
    /// observer; `None` when no form is open or the index is out of
    /// range).
    pub fn form_field_value(&self, index: usize) -> Option<String> {
        self.form
            .as_ref()?
            .fields
            .get(index)
            .map(|f| f.value.clone())
    }

    fn cursor(&self) -> usize {
        self.cursors[self.tab.index()]
    }

    fn set_cursor(&mut self, value: usize) {
        self.cursors[self.tab.index()] = value;
    }

    fn row_count(&self) -> usize {
        match self.tab {
            Tab::Providers => self.providers.len(),
            Tab::Models => self.models.len(),
            Tab::Levels => self.levels.len(),
        }
    }

    /// The selected row's primary name (provider name / entry name /
    /// level name), if any.
    fn selected_name(&self) -> Option<String> {
        let c = self.cursor();
        match self.tab {
            Tab::Providers => self.providers.get(c).map(|(n, _)| n.clone()),
            Tab::Models => self.models.get(c).map(|(n, _)| n.clone()),
            Tab::Levels => self.levels.get(c).map(|r| r.name.clone()),
        }
    }

    // ── key handling ────────────────────────────────────────────────────

    /// Route one modal action. The App calls this for EVERY action while
    /// the overlay is up (nothing leaks beneath it).
    pub fn on_key(&mut self, action: &Action, _ctx: &AppCtx) -> ModelsIntent {
        match self.mode {
            Mode::List => self.on_key_list(action),
            Mode::ProviderForm | Mode::ModelForm | Mode::LevelNameForm => self.on_key_form(action),
            Mode::PickEntry => self.on_key_pick(action),
            Mode::ConfirmDelete => self.on_key_confirm(action),
        }
    }

    fn on_key_list(&mut self, action: &Action) -> ModelsIntent {
        match action {
            Action::DismissOverlay => ModelsIntent::Close,
            Action::InputChar('1') => {
                self.switch_tab(Tab::Providers);
                ModelsIntent::None
            }
            Action::InputChar('2') => {
                self.switch_tab(Tab::Models);
                ModelsIntent::None
            }
            Action::InputChar('3') => {
                self.switch_tab(Tab::Levels);
                ModelsIntent::None
            }
            Action::InputChar('a') => {
                self.open_add_form();
                ModelsIntent::None
            }
            Action::InputChar('e') => {
                self.open_edit_form();
                ModelsIntent::None
            }
            Action::InputChar('d') => {
                self.begin_delete();
                ModelsIntent::None
            }
            // Rebind is a Levels-only verb; Enter mirrors it there.
            Action::InputChar('r') | Action::SubmitInput if self.tab == Tab::Levels => {
                self.open_rebind();
                ModelsIntent::None
            }
            Action::InputHistoryPrev => {
                self.move_cursor(-1);
                ModelsIntent::None
            }
            Action::InputHistoryNext => {
                self.move_cursor(1);
                ModelsIntent::None
            }
            _ => ModelsIntent::None,
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.flash = None;
        // Keep the cursor inside the (new) row count.
        let c = self.cursor().min(self.row_count().saturating_sub(1));
        self.set_cursor(c);
    }

    fn move_cursor(&mut self, delta: isize) {
        self.flash = None;
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let c = self.cursor() as isize + delta;
        self.set_cursor(c.clamp(0, count as isize - 1) as usize);
    }

    fn begin_delete(&mut self) {
        self.flash = None;
        if self.tab == Tab::Levels {
            let Some(row) = self.levels.get(self.cursor()) else {
                return;
            };
            if row.required {
                self.flash = Some("main/fast 为 required 级别，不可删除".to_owned());
                return;
            }
        }
        let Some(name) = self.selected_name() else {
            return;
        };
        self.confirm = Some(match self.tab {
            Tab::Providers => PendingDelete::Provider(name),
            Tab::Models => PendingDelete::Model(name),
            Tab::Levels => PendingDelete::Level(name),
        });
        self.mode = Mode::ConfirmDelete;
    }

    fn on_key_confirm(&mut self, action: &Action) -> ModelsIntent {
        match action {
            Action::InputChar('y') => match self.confirm.take() {
                Some(pending) => {
                    self.mode = Mode::List;
                    ModelsIntent::Commit(pending.into_change())
                }
                None => ModelsIntent::None,
            },
            Action::InputChar('n') | Action::DismissOverlay => {
                self.confirm = None;
                self.mode = Mode::List;
                ModelsIntent::None
            }
            _ => ModelsIntent::None,
        }
    }

    fn open_rebind(&mut self) {
        self.flash = None;
        if self.models.is_empty() {
            self.flash = Some("模型库为空，无法重绑定".to_owned());
            return;
        }
        let Some(level) = self.levels.get(self.cursor()).map(|r| r.name.clone()) else {
            return;
        };
        self.pick = Some(PickState { level, cursor: 0 });
        self.mode = Mode::PickEntry;
    }

    fn on_key_pick(&mut self, action: &Action) -> ModelsIntent {
        let Some(pick) = self.pick.as_mut() else {
            self.mode = Mode::List;
            return ModelsIntent::None;
        };
        match action {
            Action::DismissOverlay => {
                self.pick = None;
                self.mode = Mode::List;
                ModelsIntent::None
            }
            Action::InputHistoryPrev => {
                pick.cursor = pick.cursor.saturating_sub(1);
                ModelsIntent::None
            }
            Action::InputHistoryNext => {
                pick.cursor = (pick.cursor + 1).min(self.models.len().saturating_sub(1));
                ModelsIntent::None
            }
            Action::SubmitInput => {
                let entry = self.models.get(pick.cursor).map(|(n, _)| n.clone());
                let level = pick.level.clone();
                self.pick = None;
                self.mode = Mode::List;
                match entry {
                    Some(entry) => ModelsIntent::Commit(ModelsChange::SetLevel { level, entry }),
                    None => ModelsIntent::None,
                }
            }
            _ => ModelsIntent::None,
        }
    }

    // ── forms ───────────────────────────────────────────────────────────

    fn open_add_form(&mut self) {
        self.flash = None;
        match self.tab {
            Tab::Providers => {
                self.form = Some(FormState {
                    kind: Mode::ProviderForm,
                    editing: None,
                    fields: vec![
                        Field::text("名称", ""),
                        Field::text("Base URL", ""),
                        Field::text("API key env", ""),
                        Field::text("API key", "").secret(),
                        Field::combo("Adapter", ""),
                    ],
                    focus: 0,
                    error: None,
                    combo_open: false,
                    combo_selected: 0,
                });
                self.mode = Mode::ProviderForm;
            }
            Tab::Models => {
                if self.providers.is_empty() {
                    self.flash = Some("先在 Providers 页添加 provider".to_owned());
                    return;
                }
                self.form = Some(self.build_model_form(None));
                self.mode = Mode::ModelForm;
            }
            Tab::Levels => {
                self.form = Some(FormState {
                    kind: Mode::LevelNameForm,
                    editing: None,
                    fields: vec![Field::text("级别名", "")],
                    focus: 0,
                    error: None,
                    combo_open: false,
                    combo_selected: 0,
                });
                self.mode = Mode::LevelNameForm;
            }
        }
    }

    fn open_edit_form(&mut self) {
        self.flash = None;
        let Some(name) = self.selected_name() else {
            return;
        };
        match self.tab {
            Tab::Providers => {
                let Some((_, cfg)) = self.providers.iter().find(|(n, _)| *n == name) else {
                    return;
                };
                self.form = Some(FormState {
                    kind: Mode::ProviderForm,
                    editing: Some(name.clone()),
                    fields: vec![
                        Field::text("名称", &name).locked(),
                        Field::text("Base URL", &cfg.base_url),
                        Field::text("API key env", &cfg.api_key_env),
                        Field::text("API key", "").secret(),
                        Field::combo("Adapter", cfg.adapter.as_deref().unwrap_or("")),
                    ],
                    focus: 1, // the name is locked; start on Base URL
                    error: None,
                    combo_open: false,
                    combo_selected: 0,
                });
                self.mode = Mode::ProviderForm;
            }
            Tab::Models => {
                let Some((_, cfg)) = self.models.iter().find(|(n, _)| *n == name) else {
                    return;
                };
                self.form = Some(self.build_model_form(Some((&name, cfg))));
                self.mode = Mode::ModelForm;
            }
            // Levels have no edit form — rebinding IS the edit.
            Tab::Levels => self.open_rebind(),
        }
    }

    fn build_model_form(&self, editing: Option<(&String, &ModelConfig)>) -> FormState {
        let options: Vec<String> = self.providers.iter().map(|(n, _)| n.clone()).collect();
        let (entry, provider, model, tool, vision, reasoning, selected) = match editing {
            Some((name, cfg)) => {
                let selected = options.iter().position(|o| *o == cfg.provider).unwrap_or(0);
                (
                    name.clone(),
                    cfg.provider.clone(),
                    cfg.model.clone(),
                    cfg.supports_tool_call,
                    cfg.supports_vision,
                    cfg.supports_reasoning,
                    selected,
                )
            }
            None => (
                String::new(),
                String::new(),
                String::new(),
                true,
                false,
                false,
                0,
            ),
        };
        let mut provider_field = Field::choice("Provider", options, selected);
        if !provider.is_empty() {
            // Editing a model whose provider vanished from the snapshot
            // (layered drift): surface the dangling name as a phantom
            // option so the preselection stays honest.
            if !provider_field.options.contains(&provider) {
                provider_field.options.insert(0, provider.clone());
            }
            provider_field.selected = provider_field
                .options
                .iter()
                .position(|o| *o == provider)
                .unwrap_or(0);
        }
        FormState {
            kind: Mode::ModelForm,
            editing: editing.map(|(n, _)| n.clone()),
            fields: vec![
                Field::text("条目名", &entry).locked_if(editing.is_some()),
                provider_field,
                Field::text("Model ID", &model),
                Field::toggle("tool_call", tool),
                Field::toggle("vision", vision),
                Field::toggle("reasoning", reasoning),
            ],
            focus: if editing.is_some() { 1 } else { 0 },
            error: None,
            combo_open: false,
            combo_selected: 0,
        }
    }

    fn on_key_form(&mut self, action: &Action) -> ModelsIntent {
        // adapter-combo-1: mouse gestures talk to the dropdown's
        // last-rendered row rects (hover / press / release-apply) —
        // handled ahead of the form borrow; they never submit.
        if matches!(
            action,
            Action::MouseMove(..)
                | Action::MouseDown(..)
                | Action::MouseDrag(..)
                | Action::MouseUp(..)
        ) {
            self.on_form_mouse(action);
            return ModelsIntent::None;
        }
        let Some(form) = self.form.as_mut() else {
            self.mode = Mode::List;
            return ModelsIntent::None;
        };
        form.error = None;
        // adapter-combo-1: while the focused Combo field's dropdown is
        // OPEN, the navigation keys drive the CANDIDATE list first —
        // field switching only resumes once the dropdown closes (Esc).
        if combo_live(form) {
            match action {
                // ↑/↓ (all four direction actions) and the wheel move
                // the selection. The ≤4-row list WRAPS — a clamp would
                // dead-end ↑ on the first row with no other meaning.
                Action::InputHistoryPrev | Action::InputCursorUp | Action::WheelScrollUp(..) => {
                    let len = combo_candidates(&form.fields[form.focus].value).len();
                    if len > 0 {
                        form.combo_selected = (form.combo_selected + len - 1) % len;
                    }
                    return ModelsIntent::None;
                }
                Action::InputHistoryNext
                | Action::InputCursorDown
                | Action::WheelScrollDown(..) => {
                    let len = combo_candidates(&form.fields[form.focus].value).len();
                    if len > 0 {
                        form.combo_selected = (form.combo_selected + 1) % len;
                    }
                    return ModelsIntent::None;
                }
                // Esc is two-stage: dropdown open → close ONLY the
                // dropdown (the text stays; the next Esc cancels the
                // form as before).
                Action::DismissOverlay => {
                    form.combo_open = false;
                    return ModelsIntent::None;
                }
                // Enter applies the selected candidate, then the
                // UNIFIED submit (any-field Enter, unchanged). An empty
                // filter applies nothing — the typed text submits as-is
                // (free-text escape hatch; an unknown adapter fails at
                // provider build time, exactly as today).
                Action::SubmitInput => {
                    apply_combo_selection(form);
                    return self.submit_form();
                }
                // Tab applies the selection, then falls through to the
                // field-advance arm below; leaving the field closes
                // the dropdown (combo_focus_resync).
                Action::FocusNext => {
                    apply_combo_selection(form);
                }
                _ => {}
            }
        }
        match action {
            Action::DismissOverlay => {
                self.form = None;
                self.mode = Mode::List;
                ModelsIntent::None
            }
            // Tab and ↓ advance; ↑ goes back (all wrap).
            Action::FocusNext | Action::InputHistoryNext | Action::InputCursorDown => {
                form.focus = (form.focus + 1) % form.fields.len();
                combo_focus_resync(form);
                ModelsIntent::None
            }
            Action::InputHistoryPrev | Action::InputCursorUp => {
                form.focus = (form.focus + form.fields.len() - 1) % form.fields.len();
                combo_focus_resync(form);
                ModelsIntent::None
            }
            Action::InputChar(' ') if matches!(form.fields[form.focus].kind, FieldKind::Toggle) => {
                form.fields[form.focus].on = !form.fields[form.focus].on;
                ModelsIntent::None
            }
            Action::InputCursorLeft
                if matches!(form.fields[form.focus].kind, FieldKind::Choice) =>
            {
                cycle_choice(form, -1);
                ModelsIntent::None
            }
            Action::InputCursorRight
                if matches!(form.fields[form.focus].kind, FieldKind::Choice) =>
            {
                cycle_choice(form, 1);
                ModelsIntent::None
            }
            Action::SubmitInput => self.submit_form(),
            Action::PasteText(payload) => {
                insert_text(
                    form,
                    &payload
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect::<String>(),
                );
                combo_text_resync(form);
                ModelsIntent::None
            }
            Action::InputChar(c) => {
                insert_text(form, &c.to_string());
                combo_text_resync(form);
                ModelsIntent::None
            }
            Action::InputBackspace => {
                backspace(form);
                combo_text_resync(form);
                ModelsIntent::None
            }
            Action::InputDeleteWord => {
                delete_word(form);
                combo_text_resync(form);
                ModelsIntent::None
            }
            // A closed-dropdown Combo degrades to a plain text field:
            // ←/→ move the text cursor (as on Text).
            Action::InputCursorLeft => {
                let f = &mut form.fields[form.focus];
                if matches!(f.kind, FieldKind::Text | FieldKind::Combo) {
                    f.cursor = f.cursor.saturating_sub(1);
                }
                ModelsIntent::None
            }
            Action::InputCursorRight => {
                let f = &mut form.fields[form.focus];
                if matches!(f.kind, FieldKind::Text | FieldKind::Combo) {
                    let len = f.value.chars().count();
                    f.cursor = (f.cursor + 1).min(len);
                }
                ModelsIntent::None
            }
            Action::InputHome => {
                form.fields[form.focus].cursor = 0;
                ModelsIntent::None
            }
            Action::InputEnd => {
                let f = &mut form.fields[form.focus];
                f.cursor = f.value.chars().count();
                ModelsIntent::None
            }
            _ => ModelsIntent::None,
        }
    }

    // ── adapter-combo-1: dropdown mouse ────────────────────────────────

    /// The dropdown row index at a terminal cell, hit-tested against
    /// the LAST RENDER's recorded row rects (`None` on a miss).
    fn combo_row_hit(&self, column: u16, row: u16) -> Option<usize> {
        self.combo_row_rects.borrow().iter().position(|r| {
            column >= r.x
                && column < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// Mouse gestures while a form is open: hover tracks the dropdown
    /// rows; press + release on the SAME row applies that candidate to
    /// the field text (NO submit — the user may still edit other
    /// fields); a drag cancels the press (interactive-1's button
    /// contract). Gestures outside the rows stay swallowed by the
    /// panel, and a closed dropdown ignores everything.
    fn on_form_mouse(&mut self, action: &Action) {
        // The dropdown must be live for any row to be a target (the
        // rects may be one frame stale — e.g. Esc closed the dropdown
        // after the last render).
        let live = self.form.as_ref().is_some_and(combo_live);
        match action {
            Action::MouseMove(column, row) => {
                self.combo_hover = if live {
                    self.combo_row_hit(*column, *row)
                } else {
                    None
                };
            }
            Action::MouseDown(column, row) => {
                self.combo_press = if live {
                    self.combo_row_hit(*column, *row)
                } else {
                    None
                };
            }
            Action::MouseDrag(..) => {
                // A drag is not a click — drop the pending press.
                self.combo_press = None;
            }
            Action::MouseUp(column, row) => {
                let released = if live {
                    self.combo_row_hit(*column, *row)
                } else {
                    None
                };
                if let (Some(pressed), Some(on_row)) = (self.combo_press.take(), released) {
                    if pressed == on_row {
                        self.apply_combo_row(pressed);
                    }
                }
                // The pointer is still on that row — keep it hovered.
                self.combo_hover = released;
            }
            _ => {}
        }
    }

    /// Apply candidate `index` (of the CURRENT filter) to the focused
    /// Combo field — click semantics: value only, the form stays open
    /// and unsubmitted, the dropdown stays open (the text converges
    /// the filter onto the applied candidate).
    fn apply_combo_row(&mut self, index: usize) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !matches!(
            form.fields.get(form.focus).map(|f| f.kind),
            Some(FieldKind::Combo)
        ) {
            return;
        }
        let f = &mut form.fields[form.focus];
        if let Some(c) = combo_candidates(&f.value).get(index).copied() {
            f.value = c.to_owned();
            f.cursor = f.value.chars().count();
        }
        combo_clamp(form);
    }

    fn submit_form(&mut self) -> ModelsIntent {
        let kind = match self.form.as_ref() {
            Some(f) => f.kind,
            None => {
                self.mode = Mode::List;
                return ModelsIntent::None;
            }
        };
        match kind {
            Mode::ProviderForm => self.submit_provider_form(),
            Mode::ModelForm => self.submit_model_form(),
            Mode::LevelNameForm => self.submit_level_name_form(),
            _ => ModelsIntent::None,
        }
    }

    fn submit_provider_form(&mut self) -> ModelsIntent {
        let editing = self
            .form
            .as_ref()
            .and_then(|f| f.editing.clone())
            .unwrap_or_default();
        let name = field_text(self.form.as_ref(), 0);
        let base_url = field_text(self.form.as_ref(), 1);
        let key_env = field_text(self.form.as_ref(), 2);
        let pasted = field_text(self.form.as_ref(), 3);
        let adapter = field_text(self.form.as_ref(), 4);

        if !valid_bare_key(&name) {
            self.set_form_error("名称必填且只能含字母/数字/-/_");
            return ModelsIntent::None;
        }
        if editing.is_empty() && self.providers.iter().any(|(n, _)| *n == name) {
            self.set_form_error(format!("provider '{name}' 已存在"));
            return ModelsIntent::None;
        }
        if base_url.trim().is_empty() {
            self.set_form_error("Base URL 必填");
            return ModelsIntent::None;
        }
        let (api_key_env, env_key) = if !pasted.is_empty() {
            let var = suggest_env_key(&name);
            (var.clone(), Some((var, pasted)))
        } else if !key_env.trim().is_empty() {
            (key_env.trim().to_owned(), None)
        } else {
            self.set_form_error("api_key_env 必填（或直接粘贴 API key）");
            return ModelsIntent::None;
        };

        let cfg = ProviderConfig {
            base_url: base_url.trim().to_owned(),
            api_key_env,
            adapter: (!adapter.trim().is_empty()).then(|| adapter.trim().to_owned()),
            max_attempts: 3,
            retry_base_ms: 500,
        };
        self.form = None;
        self.mode = Mode::List;
        ModelsIntent::Commit(ModelsChange::UpsertProvider { name, cfg, env_key })
    }

    fn submit_model_form(&mut self) -> ModelsIntent {
        let editing = self
            .form
            .as_ref()
            .and_then(|f| f.editing.clone())
            .unwrap_or_default();
        let entry = field_text(self.form.as_ref(), 0);
        let provider = choice_text(self.form.as_ref(), 1);
        let model = field_text(self.form.as_ref(), 2);
        let tool = self.form.as_ref().is_some_and(|f| f.fields[3].on);
        let vision = self.form.as_ref().is_some_and(|f| f.fields[4].on);
        let reasoning = self.form.as_ref().is_some_and(|f| f.fields[5].on);

        if !valid_bare_key(&entry) {
            self.set_form_error("条目名必填且只能含字母/数字/-/_");
            return ModelsIntent::None;
        }
        if editing.is_empty() && self.models.iter().any(|(n, _)| *n == entry) {
            self.set_form_error(format!("模型条目 '{entry}' 已存在"));
            return ModelsIntent::None;
        }
        if provider.trim().is_empty() {
            self.set_form_error("Provider 必选");
            return ModelsIntent::None;
        }
        if model.trim().is_empty() {
            self.set_form_error("Model ID 必填");
            return ModelsIntent::None;
        }

        let cfg = ModelConfig {
            provider: provider.trim().to_owned(),
            model: model.trim().to_owned(),
            max_context_tokens: None,
            max_output_tokens: None,
            supports_tool_call: tool,
            supports_vision: vision,
            supports_reasoning: reasoning,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
        };
        self.form = None;
        self.mode = Mode::List;
        ModelsIntent::Commit(ModelsChange::UpsertModel { entry, cfg })
    }

    fn submit_level_name_form(&mut self) -> ModelsIntent {
        let name = field_text(self.form.as_ref(), 0).trim().to_owned();
        if !valid_bare_key(&name) {
            self.set_form_error("级别名必填且不能含空白/特殊字符");
            return ModelsIntent::None;
        }
        if self.levels.iter().any(|r| r.name == name) {
            self.set_form_error(format!("级别 '{name}' 已存在"));
            return ModelsIntent::None;
        }
        if self.models.is_empty() {
            self.set_form_error("模型库为空，无法绑定");
            return ModelsIntent::None;
        }
        // Level names may collide with entry names (levels resolve
        // first) — deliberately allowed.
        self.form = None;
        self.pick = Some(PickState {
            level: name,
            cursor: 0,
        });
        self.mode = Mode::PickEntry;
        ModelsIntent::None
    }

    fn set_form_error(&mut self, msg: impl Into<String>) {
        if let Some(form) = self.form.as_mut() {
            form.error = Some(msg.into());
        }
    }

    // ── rendering ───────────────────────────────────────────────────────

    pub fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // adapter-combo-1: per-pass record hygiene — every path below
        // either re-records the combo dropdown rows or leaves them
        // empty (the transcript's layout-pass pattern).
        self.combo_row_rects.borrow_mut().clear();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = &ctx.theme;
        let [tab_row, head_row, body, foot_row] =
            area.layout(&ratatui::layout::Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ]));

        // Tab bar: `1 Providers   2 Models   3 Levels`, active bold.
        let mut tabs: Vec<Span<'static>> = Vec::new();
        for (i, t) in Tab::ALL.iter().enumerate() {
            if i > 0 {
                tabs.push(Span::styled("   ", theme.fine));
            }
            let style = if *t == self.tab {
                theme.header
            } else {
                theme.muted
            };
            tabs.push(Span::styled(format!("{} {}", i + 1, t.label()), style));
        }
        f.render_widget(Paragraph::new(Line::from(tabs)), tab_row);

        // Section header (with the sub-mode context when active).
        let head: Vec<Span<'static>> = match (&self.mode, &self.pick, &self.confirm) {
            (Mode::PickEntry, Some(pick), _) => vec![Span::styled(
                clip(
                    &format!("选择模型条目 → {}", pick.level),
                    area.width as usize,
                ),
                theme.muted,
            )],
            (Mode::ConfirmDelete, _, Some(pending)) => vec![Span::styled(
                format!("确认删除 {}", pending.subject()),
                theme.warning,
            )],
            _ => vec![Span::styled(
                format!("{} · {} 项", self.tab.label(), self.row_count()),
                theme.muted,
            )],
        };
        f.render_widget(Paragraph::new(Line::from(head)), head_row);

        let lines = match self.mode {
            Mode::List => self.list_lines(area.width as usize, body.height as usize, ctx),
            Mode::ProviderForm | Mode::ModelForm | Mode::LevelNameForm => {
                self.form_lines(area.width as usize, ctx)
            }
            Mode::PickEntry => self.pick_lines(area.width as usize, body.height as usize, ctx),
            Mode::ConfirmDelete => self.confirm_lines(area.width as usize, ctx),
        };
        if body.height > 0 {
            f.render_widget(Paragraph::new(lines), body);
        }
        // adapter-combo-1: the focused Combo field's dropdown paints
        // OVER the form rows below it (form modes only; records the
        // row hit rects the mouse handlers consume).
        if matches!(
            self.mode,
            Mode::ProviderForm | Mode::ModelForm | Mode::LevelNameForm
        ) {
            self.render_combo_dropdown(f, body, ctx);
        }

        // Footer slot: hints, or the in-panel refusal (flash) / form
        // error in warning style.
        let footer = match (
            &self.mode,
            &self.flash,
            self.form.as_ref().and_then(|f| f.error.clone()),
        ) {
            (Mode::ProviderForm | Mode::ModelForm | Mode::LevelNameForm, _, Some(err)) => {
                Line::from(Span::styled(clip(&err, area.width as usize), theme.warning))
            }
            (Mode::ProviderForm | Mode::ModelForm | Mode::LevelNameForm, _, None) => hint_line(
                "Tab/↑↓ 切字段 · 空格 切换 · ←→ 选项 · Enter 提交 · Esc 取消",
                theme,
                area.width as usize,
            ),
            (Mode::PickEntry, _, _) => hint_line(
                "↑↓ 选择 · Enter 确认 · Esc 取消",
                theme,
                area.width as usize,
            ),
            (Mode::ConfirmDelete, _, _) => {
                hint_line("y 确认删除 · Esc 取消", theme, area.width as usize)
            }
            (Mode::List, Some(msg), _) => {
                Line::from(Span::styled(clip(msg, area.width as usize), theme.warning))
            }
            (Mode::List, None, _) => {
                let hints = match self.tab {
                    Tab::Levels => "Enter/r 重绑定 · a 新增级别 · d 删除 · 1/2/3 切换 · Esc 关闭",
                    _ => "a 添加 · e 编辑 · d 删除 · 1/2/3 切换 · Esc 关闭",
                };
                hint_line(hints, theme, area.width as usize)
            }
        };
        f.render_widget(Paragraph::new(footer), foot_row);
    }

    /// Sliding-window start so the cursor row stays visible.
    fn window_start(cursor: usize, visible: usize) -> usize {
        if visible == 0 {
            0
        } else {
            cursor.saturating_sub(visible - 1)
        }
    }

    fn list_lines(&self, width: usize, visible: usize, ctx: &AppCtx) -> Vec<Line<'static>> {
        let theme = &ctx.theme;
        let set = theme.icons.set();
        if self.row_count() == 0 {
            let msg = match self.tab {
                Tab::Providers => "无 provider · a 添加",
                Tab::Models => "无模型条目 · a 添加",
                Tab::Levels => "无级别映射 · a 新增",
            };
            return vec![Line::from(Span::styled(msg, theme.fine))];
        }
        let start = Self::window_start(self.cursor(), visible.max(1));
        let end = (start + visible.max(1)).min(self.row_count());
        let mut lines = Vec::with_capacity(end - start);
        for i in start..end {
            let selected = i == self.cursor();
            let marker = if selected {
                format!("{} ", set.right)
            } else {
                "  ".to_owned()
            };
            let row_style = if selected {
                theme.header
            } else {
                theme.assistant
            };
            let content: Vec<Span<'static>> = match self.tab {
                Tab::Providers => {
                    let (name, cfg) = &self.providers[i];
                    vec![
                        Span::styled(marker, row_style),
                        Span::styled(clip(name, 18), row_style),
                        Span::styled(
                            clip(&format!("  {}", cfg.base_url), width.saturating_sub(24)),
                            theme.muted,
                        ),
                        Span::styled(clip(&format!("  key:{}", cfg.api_key_env), 28), theme.fine),
                    ]
                }
                Tab::Models => {
                    let (entry, cfg) = &self.models[i];
                    let cap = |flag: bool, letter: &str| {
                        Span::styled(
                            letter.to_owned(),
                            if flag { theme.assistant } else { theme.fine },
                        )
                    };
                    vec![
                        Span::styled(marker, row_style),
                        Span::styled(clip(entry, 16), row_style),
                        Span::styled(
                            clip(
                                &format!("  {}/{}", cfg.provider, cfg.model),
                                width.saturating_sub(26),
                            ),
                            theme.muted,
                        ),
                        Span::styled("  ", theme.fine),
                        cap(cfg.supports_tool_call, "t"),
                        Span::styled(" ", theme.fine),
                        cap(cfg.supports_vision, "v"),
                        Span::styled(" ", theme.fine),
                        cap(cfg.supports_reasoning, "r"),
                    ]
                }
                Tab::Levels => {
                    let row = &self.levels[i];
                    let name_style = if row.required {
                        theme.header
                    } else {
                        row_style
                    };
                    let mut spans = vec![
                        Span::styled(marker, row_style),
                        Span::styled(clip(&row.name, 14), name_style),
                    ];
                    if row.required {
                        spans.push(Span::styled(" (required)", theme.fine));
                    }
                    match (&row.entry, &row.resolved) {
                        (Some(entry), Some((provider, model))) => {
                            spans.push(Span::styled(
                                clip(&format!(" {right} {entry}", right = set.right), 24),
                                theme.assistant,
                            ));
                            spans.push(Span::styled(
                                clip(&format!("  {provider}/{model}"), width.saturating_sub(44)),
                                theme.muted,
                            ));
                        }
                        (Some(entry), None) => {
                            spans.push(Span::styled(
                                clip(&format!(" {right} {entry}", right = set.right), 24),
                                theme.assistant,
                            ));
                            spans.push(Span::styled("  悬空：条目不存在", theme.warning));
                        }
                        (None, _) => {
                            spans.push(Span::styled("  未设置", theme.warning));
                        }
                    }
                    spans
                }
            };
            lines.push(Line::from(content));
        }
        lines
    }

    fn form_lines(&self, width: usize, ctx: &AppCtx) -> Vec<Line<'static>> {
        let theme = &ctx.theme;
        let set = theme.icons.set();
        let Some(form) = &self.form else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let focus = form.focus;
        for (i, field) in form.fields.iter().enumerate() {
            let marker = if i == focus {
                format!("{} ", set.right)
            } else {
                "  ".to_owned()
            };
            let label_style = if i == focus {
                theme.header
            } else {
                theme.muted
            };
            let mut spans = vec![
                Span::styled(marker, theme.line),
                Span::styled(clip(&format!("{}: ", field.label), 16), label_style),
            ];
            // adapter-combo-1: a Combo field renders exactly like Text
            // (the dropdown is a separate overlay below the row).
            match field.kind {
                FieldKind::Text | FieldKind::Combo => {
                    let display = if field.secret {
                        mask_secret(&field.value)
                    } else if field.locked {
                        format!("{}（固定）", field.value)
                    } else if field.value.is_empty() && i == focus {
                        "…".to_owned()
                    } else {
                        field.value.clone()
                    };
                    let shown = if i == focus && !field.locked {
                        with_cursor(&display, field.cursor, set.cursor)
                    } else {
                        display
                    };
                    let style = if field.locked {
                        theme.fine
                    } else {
                        theme.assistant
                    };
                    spans.push(Span::styled(clip(&shown, width.saturating_sub(20)), style));
                    if i == 3 {
                        spans.push(Span::styled("（粘贴即存 .env）", theme.fine));
                    }
                    if i == 4 {
                        spans.push(Span::styled("（空=openai 缺省）", theme.fine));
                    }
                }
                FieldKind::Toggle => {
                    let (glyph, word) = if field.on {
                        (set.check, "on")
                    } else {
                        (set.cross, "off")
                    };
                    spans.push(Span::styled(
                        format!("{glyph} {word}"),
                        if field.on {
                            theme.assistant
                        } else {
                            theme.fine
                        },
                    ));
                }
                FieldKind::Choice => {
                    let cur = field
                        .options
                        .get(field.selected)
                        .cloned()
                        .unwrap_or_default();
                    spans.push(Span::styled(
                        format!("{} {} {}", set.left, cur, set.right),
                        theme.assistant,
                    ));
                }
            }
            lines.push(Line::from(spans));
        }
        if form.kind == Mode::LevelNameForm {
            lines.push(Line::from(Span::styled(
                "允许与模型条目同名（levels 解析优先）",
                theme.fine,
            )));
        }
        lines
    }

    /// adapter-combo-1: paint the focused Combo field's candidate
    /// dropdown over the rows BELOW it — Clear + the user-message
    /// surface (the slash completion's overlay family: no frame, no
    /// push-down of the covered rows). One row per filtered candidate
    /// ([`combo_candidates`]), clipped to the panel body (≤4
    /// candidates — no scrollbar). The selected row carries the `→ `
    /// marker + signal/BOLD ([`Theme::user_label`] — the completion
    /// list's selected-row semantics); the HOVERED row's label alone
    /// takes [`Theme::hover`] and loses to the selection
    /// (interactive-1's two-style contract). Records each row's
    /// terminal rect for the mouse hit-tests.
    fn render_combo_dropdown(&self, f: &mut Frame, body: Rect, ctx: &AppCtx) {
        let Some(form) = &self.form else {
            return;
        };
        if !combo_live(form) {
            return;
        }
        let field = &form.fields[form.focus];
        let cands = combo_candidates(&field.value);
        if cands.is_empty() {
            return;
        }
        // Field row `i` renders at body row `i` (form_lines emits
        // exactly one line per field, top-anchored) — the dropdown
        // starts right below the focused field's row.
        let top = body.y.saturating_add(form.focus as u16 + 1);
        let avail = (body.y + body.height).saturating_sub(top) as usize;
        let shown = cands.len().min(avail);
        if shown == 0 {
            return;
        }
        let x = body.x.saturating_add(COMBO_INSET_COLS);
        let width = body.width.saturating_sub(COMBO_INSET_COLS).max(1);
        let theme = &ctx.theme;
        let set = theme.icons.set();
        let empty_text = field.value.trim().is_empty();
        for (d, cand) in cands[..shown].iter().enumerate() {
            let row = Rect {
                x,
                y: top + d as u16,
                width,
                height: 1,
            };
            f.render_widget(Clear, row);
            // The surface: the user-message band slot (bg-only Style —
            // the same low-key surface the completion overlay rides;
            // Cell::set_style patches, so the spans keep their fg).
            f.buffer_mut().set_style(row, theme.user_message_bg);
            let selected = d == form.combo_selected;
            let hovered = self.combo_hover == Some(d);
            let marker = if selected {
                Span::styled(format!("{} ", set.right), theme.user_label)
            } else {
                // No marker for plain OR hovered rows — the marker is
                // the keyboard selection's alone.
                Span::raw("  ")
            };
            let label_style = if selected {
                theme.user_label
            } else if hovered {
                theme.hover
            } else {
                theme.assistant
            };
            let mut spans = vec![marker, Span::styled((*cand).to_owned(), label_style)];
            // Empty text → the first row is the effective default.
            if empty_text && d == 0 {
                spans.push(Span::styled("（默认）", theme.muted));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), row);
            self.combo_row_rects.borrow_mut().push(row);
        }
    }

    fn pick_lines(&self, width: usize, visible: usize, ctx: &AppCtx) -> Vec<Line<'static>> {
        let theme = &ctx.theme;
        let set = theme.icons.set();
        let Some(pick) = &self.pick else {
            return Vec::new();
        };
        let start = Self::window_start(pick.cursor, visible.max(1));
        let end = (start + visible.max(1)).min(self.models.len());
        let mut lines = Vec::new();
        for i in start..end {
            let (entry, cfg) = &self.models[i];
            let selected = i == pick.cursor;
            let marker = if selected {
                format!("{} ", set.right)
            } else {
                "  ".to_owned()
            };
            let style = if selected {
                theme.header
            } else {
                theme.assistant
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(clip(entry, 18), style),
                Span::styled(
                    clip(
                        &format!("  {}/{}", cfg.provider, cfg.model),
                        width.saturating_sub(22),
                    ),
                    theme.muted,
                ),
            ]));
        }
        lines
    }

    fn confirm_lines(&self, width: usize, ctx: &AppCtx) -> Vec<Line<'static>> {
        let theme = &ctx.theme;
        let set = theme.icons.set();
        let Some(pending) = &self.confirm else {
            return Vec::new();
        };
        let (kind, name) = match pending {
            PendingDelete::Provider(n) => ("provider", n),
            PendingDelete::Model(n) => ("模型条目", n),
            PendingDelete::Level(n) => ("级别", n),
        };
        vec![
            Line::from(Span::styled(
                clip(&format!("{} 删除 {kind} {name}？", set.warn), width),
                theme.warning,
            )),
            Line::from(Span::styled("引用检查在保存时执行", theme.fine)),
        ]
    }
}

impl Field {
    fn locked_if(self, locked: bool) -> Self {
        if locked {
            self.locked()
        } else {
            self
        }
    }
}

// ── form field helpers ──────────────────────────────────────────────

/// adapter-combo-1: the focused Combo field's candidate list —
/// [`ADAPTER_PROTOCOLS`] filtered by a case-insensitive SUBSTRING of
/// the current text (the needle is trimmed, matching the submit path's
/// `adapter.trim()`; empty → all four).
fn combo_candidates(text: &str) -> Vec<&'static str> {
    let needle = text.trim().to_lowercase();
    ADAPTER_PROTOCOLS
        .iter()
        .copied()
        .filter(|c| needle.is_empty() || c.to_lowercase().contains(&needle))
        .collect()
}

/// Whether the focused field is a Combo with its dropdown OPEN — the
/// guard for every dropdown-key interaction.
fn combo_live(form: &FormState) -> bool {
    form.combo_open
        && matches!(
            form.fields.get(form.focus).map(|f| f.kind),
            Some(FieldKind::Combo)
        )
}

/// Clamp the selection into the current filter — the kept-when-in-range
/// policy (simple and self-consistent for a ≤4-row list).
fn combo_clamp(form: &mut FormState) {
    let len = form
        .fields
        .get(form.focus)
        .map(|f| combo_candidates(&f.value).len())
        .unwrap_or(0);
    form.combo_selected = form.combo_selected.min(len.saturating_sub(1));
}

/// Focus landed on a new field: a Combo field's dropdown OPENS on
/// focus; any other field closes it.
fn combo_focus_resync(form: &mut FormState) {
    if matches!(
        form.fields.get(form.focus).map(|f| f.kind),
        Some(FieldKind::Combo)
    ) {
        form.combo_open = true;
        combo_clamp(form);
    } else {
        form.combo_open = false;
    }
}

/// Text changed on a Combo field: keep the dropdown open (re-open it
/// if Esc had closed it) and re-clamp the selection into the new
/// filter.
fn combo_text_resync(form: &mut FormState) {
    if matches!(
        form.fields.get(form.focus).map(|f| f.kind),
        Some(FieldKind::Combo)
    ) {
        form.combo_open = true;
        combo_clamp(form);
    }
}

/// Apply the dropdown's selected candidate to the focused Combo
/// field's text (Enter/Tab). No-op when the filter left nothing — the
/// free-text escape hatch keeps whatever was typed. After the apply
/// the filter converges onto the candidate (substring of itself), so
/// the clamp lands the selection back on it.
fn apply_combo_selection(form: &mut FormState) {
    if !matches!(
        form.fields.get(form.focus).map(|f| f.kind),
        Some(FieldKind::Combo)
    ) {
        return;
    }
    let sel = form.combo_selected;
    let f = &mut form.fields[form.focus];
    if let Some(c) = combo_candidates(&f.value).get(sel) {
        f.value = (*c).to_owned();
        f.cursor = f.value.chars().count();
    }
    combo_clamp(form);
}

fn cycle_choice(form: &mut FormState, delta: isize) {
    let f = &mut form.fields[form.focus];
    if f.options.is_empty() {
        return;
    }
    let len = f.options.len() as isize;
    f.selected = ((f.selected as isize + delta).rem_euclid(len)) as usize;
}

fn insert_text(form: &mut FormState, text: &str) {
    let f = &mut form.fields[form.focus];
    // Combo edits its text like Text (the dropdown filter follows).
    if !matches!(f.kind, FieldKind::Text | FieldKind::Combo) || f.locked {
        return;
    }
    let chars: Vec<char> = f.value.chars().collect();
    let at = f.cursor.min(chars.len());
    let mut out: String = chars[..at].iter().collect();
    out.push_str(text);
    out.extend(chars[at..].iter());
    f.cursor = at + text.chars().count();
    f.value = out;
}

fn backspace(form: &mut FormState) {
    let f = &mut form.fields[form.focus];
    if !matches!(f.kind, FieldKind::Text | FieldKind::Combo) || f.locked {
        return;
    }
    let chars: Vec<char> = f.value.chars().collect();
    if f.cursor == 0 || chars.is_empty() {
        return;
    }
    let at = (f.cursor - 1).min(chars.len() - 1);
    let mut out: String = chars[..at].iter().collect();
    out.extend(chars[at + 1..].iter());
    f.value = out;
    f.cursor = at;
}

fn delete_word(form: &mut FormState) {
    let f = &mut form.fields[form.focus];
    if !matches!(f.kind, FieldKind::Text | FieldKind::Combo) || f.locked {
        return;
    }
    let chars: Vec<char> = f.value.chars().collect();
    let at = f.cursor.min(chars.len());
    let mut start = at;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut out: String = chars[..start].iter().collect();
    out.extend(chars[at..].iter());
    f.value = out;
    f.cursor = start;
}

fn field_text(form: Option<&FormState>, index: usize) -> String {
    form.and_then(|f| f.fields.get(index))
        .map(|f| f.value.clone())
        .unwrap_or_default()
}

fn choice_text(form: Option<&FormState>, index: usize) -> String {
    form.and_then(|f| f.fields.get(index))
        .and_then(|f| f.options.get(f.selected))
        .cloned()
        .unwrap_or_default()
}

/// TOML bare-key safety for names that land as table keys
/// (`[providers.<name>]` / `[models.<entry>]` / `[levels]` keys).
fn valid_bare_key(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `<NAME 大写化>_API_KEY`: non-alphanumerics fold to `_`.
fn suggest_env_key(name: &str) -> String {
    let upper: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_uppercase();
    format!("{upper}_API_KEY")
}

/// `sk-****` style mask: keep the first 3 chars, star the rest.
fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let mut out: String = chars[..3].iter().collect();
    let stars = (chars.len() - 3).clamp(4, 12);
    out.push_str(&"*".repeat(stars));
    out
}

/// Insert the terminal cursor glyph at the char position (for the
/// focused text field).
fn with_cursor(display: &str, cursor: usize, glyph: &str) -> String {
    let chars: Vec<char> = display.chars().collect();
    let at = cursor.min(chars.len());
    let mut out: String = chars[..at].iter().collect();
    out.push_str(glyph);
    out.extend(chars[at..].iter());
    out
}

/// A dim ` · `-separated hint line, clipped to the panel width.
fn hint_line(hints: &str, theme: &crate::theme::Theme, width: usize) -> Line<'static> {
    let set = theme.icons.set();
    Line::from(Span::styled(
        clip(&hints.replace(" · ", &format!(" {} ", set.dot)), width),
        theme.fine,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{ConfigSummary, Focus, RunInfo, RunState};
    use crate::theme::Theme;

    fn ctx() -> AppCtx {
        AppCtx {
            theme: Theme::new(),
            focus: Focus::Input,
            run: RunInfo {
                state: RunState::Idle,
                spinner_frame: 0,
                model_label: "main@mock".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: 0,
                context_remaining: None,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "m".into(),
                provider_name: "mock".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
                model_aliases: Vec::new(),
            },
            size: (100, 26),
            notice: None,
        }
    }

    /// Minimal layered config for the row tests (written to a tempfile
    /// and parsed through the real loader — `OpenSlateConfig` has no
    /// public TOML surface of its own).
    fn config() -> OpenSlateConfig {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[providers.zhipu]
base_url = "https://api.zhipu.example"
api_key_env = "ZHIPU_KEY"
adapter = "anthropic"

[models.fast]
provider = "mock"
model = "mock-model"

[models.main]
provider = "zhipu"
model = "glm-x"
supports_vision = true

[levels]
fast = "fast"
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("openslate.toml");
        std::fs::write(&path, toml).expect("write toml");
        openslate_app::wiring::load_config(&path).expect("parse test config")
    }

    fn component() -> ModelsComponent {
        let mut m = ModelsComponent::new();
        m.reset_view();
        m.sync(&config());
        m
    }

    fn type_str(m: &mut ModelsComponent, s: &str) {
        for c in s.chars() {
            m.on_key(&Action::InputChar(c), &ctx());
        }
    }

    fn tab_to(m: &mut ModelsComponent, tab: Tab) {
        let key = match tab {
            Tab::Providers => '1',
            Tab::Models => '2',
            Tab::Levels => '3',
        };
        m.on_key(&Action::InputChar(key), &ctx());
    }

    #[test]
    fn sync_flattens_sorted_rows_and_required_levels() {
        let m = component();
        assert_eq!(
            m.providers
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>(),
            vec!["mock", "zhipu"]
        );
        assert_eq!(
            m.models.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            vec!["fast", "main"]
        );
        // main/fast first (required), custom levels after.
        assert_eq!(m.levels[0].name, "main");
        assert!(m.levels[0].required);
        assert!(m.levels[1].required && m.levels[1].name == "fast");
        assert_eq!(
            m.levels[1]
                .resolved
                .as_ref()
                .map(|(a, b)| (a.as_str(), b.as_str())),
            Some(("mock", "mock-model"))
        );
        // `main` has no [levels] mapping — it resolves through the
        // entry-name fallback, so it shows as 未设置 in this overlay.
        assert_eq!(m.levels[0].entry, None);
    }

    #[test]
    fn tab_switch_and_cursor_navigation() {
        let mut m = component();
        tab_to(&mut m, Tab::Models);
        m.on_key(&Action::InputHistoryNext, &ctx());
        assert_eq!(m.selected_name().as_deref(), Some("main"));
        m.on_key(&Action::InputHistoryPrev, &ctx());
        assert_eq!(m.selected_name().as_deref(), Some("fast"));
        // ↑ at the top stays clamped.
        m.on_key(&Action::InputHistoryPrev, &ctx());
        assert_eq!(m.selected_name().as_deref(), Some("fast"));
        // Cursor is per-tab: back to Providers starts at 0.
        tab_to(&mut m, Tab::Providers);
        assert_eq!(m.selected_name().as_deref(), Some("mock"));
    }

    #[test]
    fn provider_add_with_pasted_key_routes_env_and_field() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "deepseek");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "https://api.deepseek.example");
        m.on_key(&Action::FocusNext, &ctx());
        // api_key_env left empty — the pasted key wins.
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::PasteText("sk-secret-123".into()), &ctx());
        // Adapter skipped; Enter submits from any field.
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::UpsertProvider { name, cfg, env_key }) => {
                assert_eq!(name, "deepseek");
                assert_eq!(cfg.base_url, "https://api.deepseek.example");
                assert_eq!(cfg.api_key_env, "DEEPSEEK_API_KEY");
                assert_eq!(
                    env_key,
                    Some(("DEEPSEEK_API_KEY".into(), "sk-secret-123".into()))
                );
                assert_eq!(cfg.adapter, None);
            }
            other => panic!("expected UpsertProvider commit, got {other:?}"),
        }
    }

    #[test]
    fn provider_add_requires_base_url_and_key() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "x1");
        m.on_key(&Action::FocusNext, &ctx());
        // no base_url, no key → error, no commit
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        assert!(matches!(intent, ModelsIntent::None));
        assert_eq!(
            m.form.as_ref().and_then(|f| f.error.clone()).as_deref(),
            Some("Base URL 必填")
        );
        // fill base_url, still no key → api_key_env error
        type_str(&mut m, "http://h");
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::FocusNext, &ctx());
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        assert!(matches!(intent, ModelsIntent::None));
        assert_eq!(
            m.form.as_ref().and_then(|f| f.error.clone()).as_deref(),
            Some("api_key_env 必填（或直接粘贴 API key）")
        );
    }

    #[test]
    fn provider_edit_prefills_and_locks_the_name() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputHistoryNext, &ctx()); // zhipu
        m.on_key(&Action::InputChar('e'), &ctx());
        let form = m.form.as_ref().expect("form open");
        assert_eq!(form.editing.as_deref(), Some("zhipu"));
        assert!(form.fields[0].locked);
        assert_eq!(form.fields[0].value, "zhipu");
        assert_eq!(form.fields[1].value, "https://api.zhipu.example");
        assert_eq!(form.fields[2].value, "ZHIPU_KEY");
        assert_eq!(form.fields[4].value, "anthropic");
        // Locked name ignores typing.
        type_str(&mut m, "XXX");
        assert_eq!(m.form.as_ref().unwrap().fields[0].value, "zhipu");
        // Submit unchanged → same values round-trip.
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::UpsertProvider { name, cfg, env_key }) => {
                assert_eq!(name, "zhipu");
                assert_eq!(cfg.api_key_env, "ZHIPU_KEY");
                assert_eq!(cfg.adapter.as_deref(), Some("anthropic"));
                assert_eq!(env_key, None);
            }
            other => panic!("expected commit, got {other:?}"),
        }
    }

    #[test]
    fn model_add_flow_with_capability_toggles() {
        let mut m = component();
        tab_to(&mut m, Tab::Models);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "glm-air");
        m.on_key(&Action::FocusNext, &ctx()); // provider choice (mock)
        m.on_key(&Action::InputCursorRight, &ctx()); // → zhipu
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "glm-air-1");
        m.on_key(&Action::FocusNext, &ctx()); // tool_call (on)
        m.on_key(&Action::InputChar(' '), &ctx()); // → off
        m.on_key(&Action::FocusNext, &ctx()); // vision
        m.on_key(&Action::InputChar(' '), &ctx()); // → on
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::UpsertModel { entry, cfg }) => {
                assert_eq!(entry, "glm-air");
                assert_eq!(cfg.provider, "zhipu");
                assert_eq!(cfg.model, "glm-air-1");
                assert!(!cfg.supports_tool_call);
                assert!(cfg.supports_vision);
                assert!(!cfg.supports_reasoning);
            }
            other => panic!("expected UpsertModel commit, got {other:?}"),
        }
    }

    #[test]
    fn model_add_refused_without_providers() {
        let mut m = component();
        m.providers.clear();
        tab_to(&mut m, Tab::Models);
        m.on_key(&Action::InputChar('a'), &ctx());
        assert!(m.form.is_none(), "no form without providers");
        assert!(m.flash.is_some(), "flash explains the refusal");
    }

    #[test]
    fn duplicate_names_are_rejected_in_add_forms() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "mock");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "http://h");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "K");
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        assert!(matches!(intent, ModelsIntent::None));
        assert_eq!(
            m.form.as_ref().unwrap().error.as_deref(),
            Some("provider 'mock' 已存在")
        );
    }

    #[test]
    fn level_rebind_emits_set_level() {
        let mut m = component();
        tab_to(&mut m, Tab::Levels);
        // Row 1 = fast (required). Rebind it to entry `main`.
        m.on_key(&Action::InputHistoryNext, &ctx());
        m.on_key(&Action::InputChar('r'), &ctx());
        assert_eq!(m.mode, Mode::PickEntry);
        m.on_key(&Action::InputHistoryNext, &ctx()); // → main
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::SetLevel { level, entry }) => {
                assert_eq!(level, "fast");
                assert_eq!(entry, "main");
            }
            other => panic!("expected SetLevel commit, got {other:?}"),
        }
    }

    #[test]
    fn level_name_form_validates_and_flows_into_the_picker() {
        let mut m = component();
        tab_to(&mut m, Tab::Levels);
        m.on_key(&Action::InputChar('a'), &ctx());
        // duplicate level name rejected
        type_str(&mut m, "fast");
        m.on_key(&Action::SubmitInput, &ctx());
        assert_eq!(
            m.form.as_ref().unwrap().error.as_deref(),
            Some("级别 'fast' 已存在")
        );
        // whitespace name rejected
        m.form.as_mut().unwrap().fields[0].value.clear();
        type_str(&mut m, "a b");
        m.on_key(&Action::SubmitInput, &ctx());
        assert!(m.form.as_ref().unwrap().error.is_some());
        // valid name → picker opens
        m.form.as_mut().unwrap().fields[0].value.clear();
        type_str(&mut m, "draft");
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        assert!(matches!(intent, ModelsIntent::None));
        assert_eq!(m.mode, Mode::PickEntry);
        assert_eq!(m.pick.as_ref().unwrap().level, "draft");
        // pick the first entry
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::SetLevel { level, entry }) => {
                assert_eq!(level, "draft");
                assert_eq!(entry, "fast");
            }
            other => panic!("expected SetLevel commit, got {other:?}"),
        }
    }

    #[test]
    fn required_level_delete_is_refused_custom_flows() {
        let mut m = component();
        tab_to(&mut m, Tab::Levels);
        m.on_key(&Action::InputChar('d'), &ctx()); // cursor on main (required)
        assert_eq!(m.mode, Mode::List, "required level refuses the form");
        assert!(m.flash.is_some());
        // Add a custom level, then delete it through the confirm.
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "draft");
        m.on_key(&Action::SubmitInput, &ctx());
        m.on_key(&Action::SubmitInput, &ctx()); // pick first entry
        m.sync(&{
            let mut cfg = config();
            cfg.levels.insert("draft".into(), "fast".into());
            cfg
        });
        // cursor: custom levels render after main/fast → index 2
        m.set_cursor(2);
        m.on_key(&Action::InputChar('d'), &ctx());
        assert_eq!(m.mode, Mode::ConfirmDelete);
        let intent = m.on_key(&Action::InputChar('y'), &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::RemoveLevel { level }) => {
                assert_eq!(level, "draft");
            }
            other => panic!("expected RemoveLevel commit, got {other:?}"),
        }
    }

    #[test]
    fn esc_backs_out_of_submodes_before_closing() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        assert_eq!(m.mode, Mode::ProviderForm);
        // Esc leaves the form; a second Esc closes the overlay.
        assert!(matches!(
            m.on_key(&Action::DismissOverlay, &ctx()),
            ModelsIntent::None
        ));
        assert_eq!(m.mode, Mode::List);
        assert!(matches!(
            m.on_key(&Action::DismissOverlay, &ctx()),
            ModelsIntent::Close
        ));
    }

    #[test]
    fn delete_confirm_cancel_keeps_the_list() {
        let mut m = component();
        m.on_key(&Action::InputChar('d'), &ctx());
        assert_eq!(m.mode, Mode::ConfirmDelete);
        assert!(matches!(
            m.on_key(&Action::InputChar('n'), &ctx()),
            ModelsIntent::None
        ));
        assert_eq!(m.mode, Mode::List);
        assert!(m.confirm.is_none());
    }

    #[test]
    fn render_smoke_all_modes_and_masks_the_secret() {
        let mut m = component();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
        // List (providers).
        terminal
            .draw(|f| m.render(f, f.area(), &ctx()))
            .expect("draw list");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("Providers"), "{screen}");
        assert!(screen.contains("zhipu"), "{screen}");
        // Secret masking in the provider form.
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "acme");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "http://h");
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::PasteText("sk-1234567890".into()), &ctx());
        terminal
            .draw(|f| m.render(f, f.area(), &ctx()))
            .expect("draw form");
        let screen = terminal.backend().to_string();
        assert!(
            !screen.contains("sk-1234567890"),
            "secret must be masked: {screen}"
        );
        assert!(screen.contains("sk-***"), "mask shape: {screen}");
        assert!(screen.contains("粘贴即存 .env"), "{screen}");
        // Models list shows capability flags.
        let mut m2 = component();
        tab_to(&mut m2, Tab::Models);
        terminal
            .draw(|f| m2.render(f, f.area(), &ctx()))
            .expect("draw models");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("t v r"), "capability flags: {screen}");
        // Levels list marks required + missing.
        let mut m3 = component();
        tab_to(&mut m3, Tab::Levels);
        terminal
            .draw(|f| m3.render(f, f.area(), &ctx()))
            .expect("draw levels");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("(required)"), "{screen}");
        assert!(screen.contains("未设置"), "{screen}");
    }

    #[test]
    fn ascii_tier_renders_without_unicode_glyphs() {
        let m = component();
        let ascii_ctx = AppCtx {
            theme: Theme::new().with_icons(crate::icons::Icons::Ascii),
            ..ctx()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 20)).expect("terminal");
        terminal
            .draw(|f| m.render(f, f.area(), &ascii_ctx))
            .expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("Providers"), "{screen}");
    }

    #[test]
    fn empty_config_renders_empty_state_hints() {
        let mut m = ModelsComponent::new();
        m.reset_view();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("openslate.toml");
        std::fs::write(&path, "").expect("write empty toml");
        let cfg = openslate_app::wiring::load_config(&path).expect("parse empty config");
        m.sync(&cfg);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 18)).expect("terminal");
        terminal
            .draw(|f| m.render(f, f.area(), &ctx()))
            .expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("无 provider"), "{screen}");
    }

    #[test]
    fn suggest_env_key_folds_non_alphanumerics() {
        assert_eq!(suggest_env_key("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(suggest_env_key("my-prov.v2"), "MY_PROV_V2_API_KEY");
    }

    #[test]
    fn valid_bare_key_rules() {
        assert!(valid_bare_key("main"));
        assert!(valid_bare_key("gpt-4o_mini"));
        assert!(!valid_bare_key(""));
        assert!(!valid_bare_key("a b"));
        assert!(!valid_bare_key("速度"));
        assert!(!valid_bare_key("a.b"));
    }

    #[test]
    fn mask_secret_shapes() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("abc"), "***");
        assert_eq!(mask_secret("sk-1234567890abcdef"), "sk-************");
    }

    // ── adapter-combo-1 ────────────────────────────────────────────────

    use ratatui::style::{Color, Modifier};

    /// Provider ADD form with the focus already on the Adapter combo
    /// (field 4) — the dropdown must be open with all candidates.
    fn provider_form_at_adapter() -> ModelsComponent {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        for _ in 0..4 {
            m.on_key(&Action::FocusNext, &ctx());
        }
        assert!(m.form.as_ref().expect("form open").combo_open);
        m
    }

    /// Render the component at `w x h` and return the buffer (the
    /// draw_completion pattern from input.rs).
    fn draw_form(m: &ModelsComponent, theme: crate::theme::Theme) -> ratatui::buffer::Buffer {
        let c = AppCtx { theme, ..ctx() };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|f| m.render(f, f.area(), &c)).expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn combo_candidates_filter_is_substring_case_insensitive() {
        // Empty (or blank) text shows all four, openai first.
        assert_eq!(combo_candidates(""), ADAPTER_PROTOCOLS.to_vec());
        assert_eq!(combo_candidates("  "), ADAPTER_PROTOCOLS.to_vec());
        assert_eq!(ADAPTER_PROTOCOLS[0], "openai", "default first");
        // Case-insensitive substring ("anthropic" itself contains an
        // `o` — narrow needles match it too).
        assert_eq!(combo_candidates("AN"), vec!["anthropic"]);
        assert_eq!(combo_candidates("O"), vec!["openai", "anthropic", "ollama"]);
        assert_eq!(combo_candidates("pen"), vec!["openai"]);
        assert_eq!(combo_candidates("ll"), vec!["ollama"]);
        assert_eq!(combo_candidates("Gem"), vec!["gemini"]);
        // No match → empty (the free-text escape hatch).
        assert!(combo_candidates("custom-proto").is_empty());
    }

    #[test]
    fn combo_focus_opens_esc_two_stage_arrows_and_wheel() {
        let mut m = provider_form_at_adapter();
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 0);

        // ↓/↑ move the selection, NOT the field focus (wrap on ≤4).
        m.on_key(&Action::InputHistoryNext, &ctx());
        m.on_key(&Action::InputCursorDown, &ctx());
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 2);
        assert_eq!(m.form.as_ref().unwrap().focus, 4, "field focus stays");
        m.on_key(&Action::InputHistoryPrev, &ctx());
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 1);
        m.on_key(&Action::InputCursorUp, &ctx());
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 0);
        m.on_key(&Action::InputHistoryPrev, &ctx());
        assert_eq!(
            m.form.as_ref().unwrap().combo_selected,
            ADAPTER_PROTOCOLS.len() - 1,
            "↑ wraps to the last candidate"
        );
        // The wheel mirrors the arrows.
        m.on_key(&Action::WheelScrollUp(0, 0), &ctx());
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 2);
        m.on_key(&Action::WheelScrollDown(0, 0), &ctx());
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 3);

        // Esc stage 1: close ONLY the dropdown (the form stays).
        assert!(matches!(
            m.on_key(&Action::DismissOverlay, &ctx()),
            ModelsIntent::None
        ));
        assert_eq!(m.mode, Mode::ProviderForm);
        assert!(!m.form.as_ref().unwrap().combo_open);
        // Closed dropdown: ↑/↓ switch fields again (wrap to field 0).
        m.on_key(&Action::InputHistoryNext, &ctx());
        assert_eq!(m.form.as_ref().unwrap().focus, 0);
        // Esc stage 2 cancels the form.
        assert!(matches!(
            m.on_key(&Action::DismissOverlay, &ctx()),
            ModelsIntent::None
        ));
        assert_eq!(m.mode, Mode::List);
        assert!(m.form.is_none());
    }

    #[test]
    fn combo_typing_filters_live_and_reopens_after_esc() {
        let mut m = provider_form_at_adapter();
        type_str(&mut m, "oll");
        let form = m.form.as_ref().unwrap();
        assert_eq!(form.fields[4].value, "oll");
        assert!(form.combo_open, "typing keeps the dropdown open");
        assert_eq!(combo_candidates(&form.fields[4].value), vec!["ollama"]);

        // Esc closes; the next text change re-opens.
        m.on_key(&Action::DismissOverlay, &ctx());
        assert!(!m.form.as_ref().unwrap().combo_open);
        type_str(&mut m, "a");
        assert!(m.form.as_ref().unwrap().combo_open, "text change re-opens");
        m.on_key(&Action::InputBackspace, &ctx());
        assert!(
            m.form.as_ref().unwrap().combo_open,
            "backspace keeps it open"
        );
        assert_eq!(m.form.as_ref().unwrap().fields[4].value, "oll");
    }

    #[test]
    fn combo_selection_clamps_when_the_filter_shrinks() {
        let mut m = provider_form_at_adapter();
        m.on_key(&Action::InputHistoryPrev, &ctx()); // wrap → 3 (ollama)
        assert_eq!(m.form.as_ref().unwrap().combo_selected, 3);
        type_str(&mut m, "ge"); // filter → [gemini]
        let form = m.form.as_ref().unwrap();
        assert_eq!(combo_candidates(&form.fields[4].value), vec!["gemini"]);
        assert_eq!(form.combo_selected, 0, "clamped into the shorter list");
    }

    #[test]
    fn combo_enter_applies_selected_and_submits() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "acme");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "http://h");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "K");
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::FocusNext, &ctx()); // Adapter combo, open
        m.on_key(&Action::InputHistoryNext, &ctx()); // → anthropic
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::UpsertProvider { name, cfg, .. }) => {
                assert_eq!(name, "acme");
                assert_eq!(cfg.adapter.as_deref(), Some("anthropic"));
            }
            other => panic!("expected commit, got {other:?}"),
        }
        assert_eq!(m.mode, Mode::List);
    }

    #[test]
    fn combo_tab_applies_and_moves_to_the_next_field() {
        let mut m = provider_form_at_adapter();
        m.on_key(&Action::InputHistoryNext, &ctx()); // → anthropic
        m.on_key(&Action::FocusNext, &ctx()); // apply + wrap to field 0
        let form = m.form.as_ref().unwrap();
        assert_eq!(form.fields[4].value, "anthropic");
        assert_eq!(form.focus, 0);
        assert!(!form.combo_open, "leaving the combo field closes it");
    }

    #[test]
    fn combo_empty_filter_submits_the_typed_text() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx());
        type_str(&mut m, "acme");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "http://h");
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "K");
        m.on_key(&Action::FocusNext, &ctx());
        m.on_key(&Action::FocusNext, &ctx());
        type_str(&mut m, "custom-proto"); // no candidate matches
        let intent = m.on_key(&Action::SubmitInput, &ctx());
        match intent {
            ModelsIntent::Commit(ModelsChange::UpsertProvider { cfg, .. }) => {
                assert_eq!(cfg.adapter.as_deref(), Some("custom-proto"));
            }
            other => panic!("expected commit, got {other:?}"),
        }
    }

    #[test]
    fn combo_edit_prefill_converges_and_keeps_unknown_values() {
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputHistoryNext, &ctx()); // zhipu (anthropic)
        m.on_key(&Action::InputChar('e'), &ctx());
        let form = m.form.as_ref().unwrap();
        assert_eq!(form.fields[4].value, "anthropic");
        assert_eq!(form.focus, 1);
        assert!(!form.combo_open, "no dropdown before the field gains focus");
        for _ in 0..3 {
            m.on_key(&Action::FocusNext, &ctx());
        }
        let form = m.form.as_ref().unwrap();
        assert!(form.combo_open);
        assert_eq!(combo_candidates(&form.fields[4].value), vec!["anthropic"]);
        assert_eq!(form.combo_selected, 0);
        // A value outside the candidates still displays (free text).
        assert!(combo_candidates("weird-proto").is_empty());
    }

    #[test]
    fn combo_click_applies_without_submit_and_drag_cancels() {
        let mut m = provider_form_at_adapter();
        let _ = draw_form(&m, crate::theme::Theme::new()); // record rects
        let rects = m.combo_row_rects.borrow().clone();
        assert_eq!(rects.len(), 4, "all four rows render");
        assert_eq!(
            (rects[0].x, rects[0].y),
            (18, 7),
            "value column, below the field"
        );
        let (x, y) = (rects[1].x, rects[1].y); // anthropic row

        // Hover lands on row 1.
        m.on_key(&Action::MouseMove(x + 2, y), &ctx());
        assert_eq!(m.combo_hover, Some(1));
        // Press + release on the same row applies WITHOUT submitting.
        m.on_key(&Action::MouseDown(x + 2, y), &ctx());
        m.on_key(&Action::MouseUp(x + 2, y), &ctx());
        let form = m.form.as_ref().unwrap();
        assert_eq!(form.fields[4].value, "anthropic");
        assert_eq!(m.mode, Mode::ProviderForm, "a click never submits");
        assert!(form.combo_open, "the dropdown stays open after the apply");
        assert_eq!(
            form.combo_selected, 0,
            "converged onto the applied candidate"
        );

        // A drag between press and release cancels the click.
        m.form.as_mut().unwrap().fields[4].value.clear();
        m.form.as_mut().unwrap().fields[4].cursor = 0;
        let _ = draw_form(&m, crate::theme::Theme::new()); // 4 rows again
        let rects = m.combo_row_rects.borrow().clone();
        let (x2, y2) = (rects[2].x, rects[2].y); // gemini row
        m.on_key(&Action::MouseDown(x2 + 2, y2), &ctx());
        m.on_key(&Action::MouseDrag(x2 + 10, y2), &ctx());
        m.on_key(&Action::MouseUp(x2 + 2, y2), &ctx());
        assert_eq!(
            m.form.as_ref().unwrap().fields[4].value,
            "",
            "drag cancels the apply (gemini never lands)"
        );
    }

    #[test]
    fn combo_mouse_misses_and_closed_dropdown_are_inert() {
        let mut m = provider_form_at_adapter();
        let _ = draw_form(&m, crate::theme::Theme::new());
        let rects = m.combo_row_rects.borrow().clone();
        // A move outside the rows: no hover, and the panel swallows it.
        m.on_key(&Action::MouseMove(2, 2), &ctx());
        assert_eq!(m.combo_hover, None);
        // Close the dropdown (Esc): stale rects must not react.
        m.on_key(&Action::DismissOverlay, &ctx());
        m.on_key(&Action::MouseMove(rects[0].x + 2, rects[0].y), &ctx());
        assert_eq!(m.combo_hover, None, "closed dropdown ignores moves");
        m.on_key(&Action::MouseDown(rects[0].x + 2, rects[0].y), &ctx());
        m.on_key(&Action::MouseUp(rects[0].x + 2, rects[0].y), &ctx());
        assert_eq!(
            m.form.as_ref().unwrap().fields[4].value,
            "",
            "no apply while closed"
        );
        // A press on a row released OUTSIDE the rows drops the press.
        let mut m2 = provider_form_at_adapter();
        let _ = draw_form(&m2, crate::theme::Theme::new());
        let rects = m2.combo_row_rects.borrow().clone();
        m2.on_key(&Action::MouseDown(rects[1].x, rects[1].y), &ctx());
        m2.on_key(&Action::MouseUp(rects[1].x, rects[1].y + 40), &ctx());
        assert_eq!(
            m2.form.as_ref().unwrap().fields[4].value,
            "",
            "mismatched release never applies"
        );
    }

    #[test]
    fn combo_dropdown_renders_selection_surface_and_default_suffix() {
        let m = provider_form_at_adapter();
        let buf = draw_form(&m, crate::theme::Theme::new());
        let signal = Color::Rgb(0x67, 0xE8, 0xF9);
        // Selected row 0 (y=7): `→ ` marker + signal/BOLD label.
        assert_eq!(buf[(18, 7)].symbol(), "→");
        assert_eq!(buf[(20, 7)].symbol(), "o");
        assert_eq!(buf[(20, 7)].style().fg, Some(signal));
        assert!(buf[(20, 7)].style().add_modifier.contains(Modifier::BOLD));
        // The empty-text default suffix, muted, after the label.
        assert_eq!(buf[(26, 7)].symbol(), "（");
        assert_eq!(buf[(26, 7)].style().fg, Some(Color::Rgb(0xAD, 0xAD, 0xAD)));
        // (Wide-glyph tails render as spaces — compact before matching.)
        let row: String = (18..44u16).map(|x| buf[(x, 7)].symbol()).collect();
        let compact = row.replace(' ', "");
        assert!(compact.contains("openai"), "{row}");
        assert!(compact.contains("（默认）"), "{row}");
        // The whole row carries the user-message surface background.
        assert_eq!(buf[(18, 7)].style().bg, Some(Color::Rgb(0x26, 0x26, 0x26)));
        assert_eq!(buf[(60, 7)].style().bg, Some(Color::Rgb(0x26, 0x26, 0x26)));
        // Plain row 1 (y=8): no marker, assistant fg, no suffix.
        assert_eq!(buf[(18, 8)].symbol(), " ", "no marker on plain rows");
        assert_eq!(buf[(20, 8)].style().fg, Some(Color::Rgb(0xD6, 0xD6, 0xD6)));
        let row1: String = (18..44u16).map(|x| buf[(x, 8)].symbol()).collect();
        let compact1 = row1.replace(' ', "");
        assert!(compact1.contains("anthropic"), "{row1}");
        assert!(
            !compact1.contains("（默认）"),
            "suffix only on empty text: {row1}"
        );
        // Non-empty text drops the suffix.
        let mut m2 = provider_form_at_adapter();
        type_str(&mut m2, "pen"); // [openai]
        let buf = draw_form(&m2, crate::theme::Theme::new());
        let row: String = (18..44u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert!(row.contains("openai"), "{row}");
        assert!(
            !row.replace(' ', "").contains("（默认）"),
            "no suffix once text exists: {row}"
        );
    }

    #[test]
    fn combo_dropdown_hover_style_and_selection_wins() {
        let mut m = provider_form_at_adapter();
        m.combo_hover = Some(1); // anthropic row
        let buf = draw_form(&m, crate::theme::Theme::new());
        let signal = Color::Rgb(0x67, 0xE8, 0xF9);
        // Hovered label: hover slot (signal + UNDERLINED + BOLD), but
        // NO marker — that is the keyboard selection's alone.
        assert_eq!(buf[(20, 8)].style().fg, Some(signal));
        assert!(buf[(20, 8)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(buf[(20, 8)].style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(18, 8)].symbol(), " ", "no marker on hover");
        // The selected row keeps its style (no underline).
        assert_eq!(buf[(18, 7)].symbol(), "→");
        assert!(!buf[(20, 7)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        // Hover on the SELECTED row: the selection style wins.
        m.combo_hover = Some(0);
        let buf = draw_form(&m, crate::theme::Theme::new());
        assert_eq!(buf[(18, 7)].symbol(), "→", "marker stays");
        assert!(!buf[(20, 7)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn combo_dropdown_ascii_tier_marker() {
        let m = provider_form_at_adapter();
        let buf = draw_form(
            &m,
            crate::theme::Theme::new().with_icons(crate::icons::Icons::Ascii),
        );
        assert_eq!(buf[(18, 7)].symbol(), ">", "ascii marker");
        let row: String = (18..44u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert!(row.contains("openai"), "{row}");
        assert!(
            row.replace(' ', "").contains("（默认）"),
            "CJK copy stays as-is: {row}"
        );
    }

    #[test]
    fn combo_dropdown_not_rendered_off_the_combo_field() {
        // Focus on a non-Combo field: no dropdown rows, no rects.
        let mut m = component();
        tab_to(&mut m, Tab::Providers);
        m.on_key(&Action::InputChar('a'), &ctx()); // focus 0 (名称)
        let buf = draw_form(&m, crate::theme::Theme::new());
        assert_eq!(m.combo_row_rects.borrow().len(), 0);
        // The rows below the form keep the untouched background (no
        // band painted).
        assert_eq!(buf[(60, 7)].style().bg, Some(Color::Reset));
        // Esc closed on the combo field: same after a re-render.
        let mut m2 = provider_form_at_adapter();
        m2.on_key(&Action::DismissOverlay, &ctx());
        let _ = draw_form(&m2, crate::theme::Theme::new());
        assert_eq!(
            m2.combo_row_rects.borrow().len(),
            0,
            "closed records nothing"
        );
    }

    #[test]
    fn combo_dropdown_clips_to_the_panel_body() {
        // A 5-row body: field 4 renders on the last visible row, so the
        // dropdown has zero rows below it inside the body.
        let m = provider_form_at_adapter();
        let c = AppCtx {
            size: (60, 8),
            ..ctx()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 8)).expect("terminal");
        terminal.draw(|f| m.render(f, f.area(), &c)).expect("draw");
        assert_eq!(m.combo_row_rects.borrow().len(), 0, "clipped away");
    }
}
