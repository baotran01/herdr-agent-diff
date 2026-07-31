use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::{RecursiveMode, Watcher};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Tabs, Wrap,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::diff::{DiffLine, DiffLineKind, render_change};
use crate::git::{GitChange, diff as render_git_diff, scan as scan_git};
use crate::herdr::{Herdr, pane_exists};
use crate::model::{Change, ChangeKind, CurrentFile, Manifest, TextEligibility};
use crate::snapshot::{classify, safe_read, scan};
use crate::state::StateStore;
use crate::{Error, Result};

const CACHE_LIMIT: usize = 32 * 1024 * 1024;
const PANEL_BACKGROUND: Color = Color::Reset;
const PANEL_FOREGROUND: Color = Color::White;
const PANEL_SELECTION: Color = Color::Rgb(56, 64, 78);
const DIFF_BACKGROUND: Color = Color::Reset;
const DIFF_GUTTER_BACKGROUND: Color = Color::Reset;
const DIFF_HEADER_BACKGROUND: Color = Color::Reset;
const DIFF_ADDITION_BACKGROUND: Color = Color::Rgb(12, 67, 48);
const DIFF_DELETION_BACKGROUND: Color = Color::Rgb(76, 31, 43);
const DIFF_TEXT: Color = Color::Rgb(208, 220, 238);
const DIFF_LINE_NUMBER: Color = Color::Rgb(105, 137, 177);
const DIFF_ADDITION: Color = Color::Rgb(105, 224, 150);
const DIFF_DELETION: Color = Color::Rgb(246, 120, 134);
const DIFF_HUNK: Color = Color::Rgb(149, 180, 224);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Tab {
    Changes,
    Files,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangesMode {
    Agent,
    Git,
}

impl ChangesMode {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent diff",
            Self::Git => "Git diff",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitState {
    Unloaded,
    Loading,
    Loaded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Navigation,
    Content,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScrollbarAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScrollbarDrag {
    axis: ScrollbarAxis,
    grab_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScrollbarGeometry {
    bar: Rect,
    track_start: u16,
    track_length: usize,
    thumb_start: usize,
    thumb_length: usize,
    max_scroll: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContentScrollbarMetrics {
    vertical: Option<ScrollbarGeometry>,
    horizontal: Option<ScrollbarGeometry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Normal,
    Help,
    Filter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NavigationRow {
    Group {
        path: PathBuf,
        label: String,
    },
    File {
        index: usize,
        label: String,
        depth: usize,
    },
}

#[derive(Clone, Debug, Default)]
struct TabState {
    selected: usize,
    list_scroll_y: usize,
    scroll_y: usize,
    scroll_x: usize,
    filter: String,
}

#[derive(Clone, Debug)]
struct ColoredSpan {
    text: String,
    foreground: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorCursor {
    tab: Tab,
    line: usize,
    column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorSelection {
    tab: Tab,
    anchor: EditorCursor,
    active: EditorCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UiLayout {
    tabs: Rect,
    body: Rect,
    status: Rect,
    navigation: Option<Rect>,
    content: Option<Rect>,
}

type DiffView<'a> = (
    ChangeKind,
    &'a Path,
    Option<&'a Path>,
    Option<&'a [DiffLine]>,
);

enum Task {
    Scan(u64),
    Diff {
        generation: u64,
        index: usize,
        change: Change,
    },
    Highlight {
        generation: u64,
        index: usize,
        file: CurrentFile,
    },
    GitScan {
        generation: u64,
    },
    GitDiff {
        generation: u64,
        index: usize,
        change: GitChange,
    },
}

struct ScanResult {
    generation: u64,
    baseline: Option<Manifest>,
    capturing: bool,
    files: Vec<CurrentFile>,
    changes: Vec<Change>,
    notices: Vec<String>,
    error: Option<String>,
}

enum WorkResult {
    Scan(Box<ScanResult>),
    Diff {
        generation: u64,
        index: usize,
        lines: Vec<DiffLine>,
    },
    Highlight {
        generation: u64,
        index: usize,
        lines: Vec<Vec<ColoredSpan>>,
        notice: Option<String>,
    },
    GitScan {
        generation: u64,
        changes: Vec<GitChange>,
        error: Option<String>,
    },
    GitDiff {
        generation: u64,
        index: usize,
        lines: Vec<DiffLine>,
    },
}

struct Cache<K, V> {
    entries: VecDeque<(K, V, usize)>,
    bytes: usize,
}

impl<K: Eq, V> Cache<K, V> {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) {
        if let Some(position) = self.entries.iter().position(|entry| entry.0 == key)
            && let Some((_, _, old_bytes)) = self.entries.remove(position)
        {
            self.bytes = self.bytes.saturating_sub(old_bytes);
        }
        self.entries.push_back((key, value, bytes));
        self.bytes = self.bytes.saturating_add(bytes);
        while self.bytes > CACHE_LIMIT {
            let Some((_, _, evicted)) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted);
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.entries
            .iter()
            .find(|entry| &entry.0 == key)
            .map(|entry| &entry.1)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

pub fn run(
    store: &StateStore,
    manifest: Option<Manifest>,
    root: &Path,
    target_pane_id: String,
    herdr: &impl Herdr,
) -> Result<()> {
    let canonical_root = root.canonicalize()?;
    let (watch_tx, watch_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = watch_tx.send(event);
    })?;
    let watcher_notice = watcher
        .watch(&canonical_root, RecursiveMode::Recursive)
        .err()
        .map(|error| format!("watcher unavailable: {error}; press r to refresh"));

    let newest_generation = Arc::new(AtomicU64::new(1));
    let (task_tx, task_rx) = mpsc::sync_channel(8);
    let (result_tx, result_rx) = mpsc::channel();
    spawn_worker(
        store.clone(),
        canonical_root.clone(),
        target_pane_id.clone(),
        Arc::clone(&newest_generation),
        task_rx,
        result_tx,
    );
    task_tx
        .send(Task::Scan(1))
        .map_err(|_| Error::Message("background worker stopped".into()))?;

    let mut terminal = TerminalSession::start()?;
    let capturing = store.capturing(&target_pane_id);
    let mut app = App::new(manifest, capturing, target_pane_id);
    if let Some(notice) = watcher_notice {
        app.notices.push(notice);
    }
    let mut dirty_since: Option<Instant> = None;
    let mut last_liveness = Instant::now();
    let mut last_capture_poll = Instant::now();

    loop {
        while let Ok(result) = result_rx.try_recv() {
            app.apply_result(result);
            app.request_selected(&task_tx);
        }
        app.request_selected(&task_tx);
        while let Ok(event) = watch_rx.try_recv() {
            match event {
                Ok(_) => dirty_since = Some(Instant::now()),
                Err(error) => {
                    app.notices
                        .push(format!("watcher error: {error}; press r to recover"));
                    dirty_since = Some(Instant::now());
                }
            }
        }
        if dirty_since.is_some_and(|instant| instant.elapsed() >= Duration::from_millis(150)) {
            app.refresh(&newest_generation, &task_tx);
            dirty_since = None;
        }
        if app.capturing && last_capture_poll.elapsed() >= Duration::from_millis(250) {
            app.refresh(&newest_generation, &task_tx);
            last_capture_poll = Instant::now();
        }
        if last_liveness.elapsed() >= Duration::from_secs(2) {
            if !pane_exists(herdr, &app.target_pane_id) {
                break;
            }
            last_liveness = Instant::now();
        }

        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if app.handle_key(key, &newest_generation, &task_tx) => break,
                Event::Mouse(mouse) => app.handle_mouse(mouse, &newest_generation, &task_tx),
                Event::FocusGained => app.refresh(&newest_generation, &task_tx),
                _ => {}
            }
        }
    }
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn start() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableFocusChange,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            SetCursorStyle::SteadyBar,
        ) {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableMouseCapture,
                DisableFocusChange,
                PopKeyboardEnhancementFlags,
                SetCursorStyle::DefaultUserShape,
                LeaveAlternateScreen
            );
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(
                    io::stdout(),
                    DisableMouseCapture,
                    DisableFocusChange,
                    PopKeyboardEnhancementFlags,
                    SetCursorStyle::DefaultUserShape,
                    LeaveAlternateScreen
                );
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }

    fn draw(&mut self, draw: impl FnOnce(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal.draw(draw)?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            DisableFocusChange,
            PopKeyboardEnhancementFlags,
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

struct App {
    tab: Tab,
    changes_mode: ChangesMode,
    focus: Focus,
    changes_state: TabState,
    files_state: TabState,
    manifest: Option<Manifest>,
    target_pane_id: String,
    changes: Vec<Change>,
    git_changes: Vec<GitChange>,
    files: Vec<CurrentFile>,
    notices: Vec<String>,
    loading: bool,
    capturing: bool,
    mode: Mode,
    generation: u64,
    render_generation: u64,
    requested: BTreeSet<(Tab, usize, u64)>,
    diff_cache: Cache<usize, Vec<DiffLine>>,
    git_diff_cache: Cache<usize, Vec<DiffLine>>,
    source_cache: Cache<usize, Vec<Vec<ColoredSpan>>>,
    source_notices: Cache<usize, String>,
    scan_error: Option<String>,
    git_error: Option<String>,
    git_state: GitState,
    cursor: Option<EditorCursor>,
    selection: Option<EditorSelection>,
    scrollbar_drag: Option<ScrollbarDrag>,
    collapsed_groups: BTreeSet<(Tab, PathBuf)>,
    cursor_blink_started: Instant,
    viewport: Rect,
}

impl App {
    fn new(manifest: Option<Manifest>, capturing: bool, target_pane_id: String) -> Self {
        Self {
            tab: Tab::Changes,
            changes_mode: ChangesMode::Agent,
            focus: Focus::Navigation,
            changes_state: TabState::default(),
            files_state: TabState::default(),
            manifest,
            target_pane_id,
            changes: Vec::new(),
            git_changes: Vec::new(),
            files: Vec::new(),
            notices: Vec::new(),
            loading: true,
            capturing,
            mode: Mode::Normal,
            generation: 1,
            render_generation: 0,
            requested: BTreeSet::new(),
            diff_cache: Cache::new(),
            git_diff_cache: Cache::new(),
            source_cache: Cache::new(),
            source_notices: Cache::new(),
            scan_error: None,
            git_error: None,
            git_state: GitState::Unloaded,
            cursor: None,
            selection: None,
            scrollbar_drag: None,
            collapsed_groups: BTreeSet::new(),
            cursor_blink_started: Instant::now(),
            viewport: Rect::default(),
        }
    }

    fn apply_result(&mut self, result: WorkResult) {
        match result {
            WorkResult::Scan(result) if result.generation == self.generation => {
                let ScanResult {
                    baseline,
                    capturing,
                    files,
                    changes,
                    notices,
                    error,
                    ..
                } = *result;
                self.render_generation = self.render_generation.saturating_add(1);
                self.requested.clear();
                if error.is_none() {
                    self.manifest = baseline;
                    self.capturing = capturing;
                    self.files = files;
                    self.changes = changes;
                    self.diff_cache.clear();
                    self.source_cache.clear();
                    self.source_notices.clear();
                    self.git_changes.clear();
                    self.git_diff_cache.clear();
                    self.git_error = None;
                    self.git_state = GitState::Unloaded;
                    self.clamp_selections();
                }
                self.scan_error = error;
                self.notices = notices;
                self.loading = false;
            }
            WorkResult::Diff {
                generation,
                index,
                lines,
            } if generation == self.render_generation => {
                let bytes = lines.iter().map(|line| line.text.len()).sum();
                self.diff_cache.insert(index, lines, bytes);
                self.requested.remove(&(Tab::Changes, index, generation));
            }
            WorkResult::Highlight {
                generation,
                index,
                lines,
                notice,
            } if generation == self.render_generation => {
                let bytes = lines.iter().flatten().map(|span| span.text.len()).sum();
                self.source_cache.insert(index, lines, bytes);
                if let Some(notice) = notice {
                    let bytes = notice.len();
                    self.source_notices.insert(index, notice, bytes);
                }
                self.requested.remove(&(Tab::Files, index, generation));
            }
            WorkResult::GitScan {
                generation,
                changes,
                error,
            } if generation == self.render_generation => {
                self.git_state = GitState::Loaded;
                self.requested.clear();
                if error.is_none() {
                    self.git_changes = changes;
                    self.git_diff_cache.clear();
                    self.clamp_selections();
                }
                self.git_error = error;
            }
            WorkResult::GitDiff {
                generation,
                index,
                lines,
            } if generation == self.render_generation => {
                let bytes = lines.iter().map(|line| line.text.len()).sum();
                self.git_diff_cache.insert(index, lines, bytes);
                self.requested.remove(&(Tab::Changes, index, generation));
            }
            _ => {}
        }
    }

    fn refresh(&mut self, newest: &AtomicU64, tasks: &mpsc::SyncSender<Task>) {
        if self.loading {
            return;
        }
        let generation = self.generation.saturating_add(1);
        if tasks.try_send(Task::Scan(generation)).is_ok() {
            self.generation = generation;
            newest.store(generation, Ordering::Release);
            self.loading = true;
            self.render_generation = self.render_generation.saturating_add(1);
            self.requested.clear();
            self.git_changes.clear();
            self.git_diff_cache.clear();
            self.git_error = None;
            self.git_state = GitState::Unloaded;
        }
    }

    fn request_selected(&mut self, tasks: &mpsc::SyncSender<Task>) {
        if self.loading {
            return;
        }
        if self.tab == Tab::Changes
            && self.changes_mode == ChangesMode::Git
            && self.git_state != GitState::Loaded
        {
            if self.git_state == GitState::Loading {
                return;
            }
            self.render_generation = self.render_generation.saturating_add(1);
            let generation = self.render_generation;
            self.requested.clear();
            if tasks.try_send(Task::GitScan { generation }).is_ok() {
                self.git_state = GitState::Loading;
            }
            return;
        }
        let Some(selected) = self.actual_selected() else {
            self.requested.clear();
            return;
        };
        if self.requested.iter().any(|(tab, index, generation)| {
            *tab == self.tab && *index == selected && *generation == self.render_generation
        }) {
            return;
        }
        let needs_task = match self.tab {
            Tab::Changes if self.changes_mode == ChangesMode::Agent => {
                self.diff_cache.get(&selected).is_none()
            }
            Tab::Changes => self.git_diff_cache.get(&selected).is_none(),
            Tab::Files => self.source_cache.get(&selected).is_none(),
        };
        if !needs_task {
            return;
        }

        self.render_generation = self.render_generation.saturating_add(1);
        let generation = self.render_generation;
        self.requested.clear();
        match self.tab {
            Tab::Changes if self.changes_mode == ChangesMode::Agent => {
                if let Some(change) = self.changes.get(selected).cloned()
                    && tasks
                        .try_send(Task::Diff {
                            generation,
                            index: selected,
                            change,
                        })
                        .is_ok()
                {
                    self.requested.insert((Tab::Changes, selected, generation));
                }
            }
            Tab::Changes => {
                if let Some(change) = self.git_changes.get(selected).cloned()
                    && tasks
                        .try_send(Task::GitDiff {
                            generation,
                            index: selected,
                            change,
                        })
                        .is_ok()
                {
                    self.requested.insert((Tab::Changes, selected, generation));
                }
            }
            Tab::Files => {
                if let Some(file) = self.files.get(selected).cloned()
                    && tasks
                        .try_send(Task::Highlight {
                            generation,
                            index: selected,
                            file,
                        })
                        .is_ok()
                {
                    self.requested.insert((Tab::Files, selected, generation));
                }
            }
        }
    }

    fn toggle_changes_mode(&mut self) {
        if self.tab != Tab::Changes {
            return;
        }
        self.changes_mode = match self.changes_mode {
            ChangesMode::Agent => ChangesMode::Git,
            ChangesMode::Git => ChangesMode::Agent,
        };
        self.changes_state.selected = 0;
        self.changes_state.scroll_y = 0;
        self.changes_state.scroll_x = 0;
        self.render_generation = self.render_generation.saturating_add(1);
        self.requested.clear();
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        newest: &AtomicU64,
        tasks: &mpsc::SyncSender<Task>,
    ) -> bool {
        if self.mode == Mode::Filter {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.active_state_mut().filter.pop();
                    self.active_state_mut().selected = 0;
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.active_state_mut().filter.push(character);
                    self.active_state_mut().selected = 0;
                }
                _ => {}
            }
            self.request_selected(tasks);
            return false;
        }
        match key.code {
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'c')
                    && key
                        .modifiers
                        .intersects(KeyModifiers::SUPER | KeyModifiers::META) =>
            {
                if self.tab == Tab::Files && self.focus == Focus::Content {
                    self.copy_selection();
                }
            }
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => {
                self.mode = if self.mode == Mode::Help {
                    Mode::Normal
                } else {
                    Mode::Help
                };
            }
            KeyCode::Char('1') => {
                self.tab = Tab::Changes;
                self.selection = None;
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Files;
                self.selection = None;
            }
            KeyCode::Char('g') => {
                if self.tab == Tab::Changes {
                    self.toggle_changes_mode();
                    self.selection = None;
                }
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Navigation => Focus::Content,
                    Focus::Content => Focus::Navigation,
                };
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('r') => self.refresh(newest, tasks),
            KeyCode::Up | KeyCode::Char('k') => self.move_vertical(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_vertical(1),
            KeyCode::Left | KeyCode::Char('h') => {
                if self.focus == Focus::Content {
                    self.active_state_mut().scroll_x =
                        self.active_state().scroll_x.saturating_sub(4);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.focus == Focus::Content {
                    self.active_state_mut().scroll_x =
                        self.active_state().scroll_x.saturating_add(4);
                }
            }
            KeyCode::PageUp => {
                self.active_state_mut().scroll_y = self.active_state().scroll_y.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.active_state_mut().scroll_y = self.active_state().scroll_y.saturating_add(10);
            }
            KeyCode::Home => self.active_state_mut().scroll_y = 0,
            _ => {}
        }
        self.clamp_selections();
        self.request_selected(tasks);
        false
    }

    fn move_vertical(&mut self, delta: isize) {
        if self.focus == Focus::Content {
            let state = self.active_state_mut();
            state.scroll_y = state.scroll_y.saturating_add_signed(delta);
            return;
        }
        let length = self.filtered_indices().len();
        if length == 0 {
            self.active_state_mut().selected = 0;
        } else {
            let current = self.active_state().selected.min(length - 1);
            self.active_state_mut().selected = current.saturating_add_signed(delta).min(length - 1);
            self.active_state_mut().scroll_y = 0;
        }
    }

    fn clamp_selections(&mut self) {
        let change_count = match self.changes_mode {
            ChangesMode::Agent => {
                filtered_change_indices(&self.changes, &self.changes_state.filter).len()
            }
            ChangesMode::Git => {
                filtered_git_indices(&self.git_changes, &self.changes_state.filter).len()
            }
        };
        let file_count = filtered_file_indices(&self.files, &self.files_state.filter).len();
        self.changes_state.selected = self
            .changes_state
            .selected
            .min(change_count.saturating_sub(1));
        self.files_state.selected = self.files_state.selected.min(file_count.saturating_sub(1));
    }

    fn active_state(&self) -> &TabState {
        match self.tab {
            Tab::Changes => &self.changes_state,
            Tab::Files => &self.files_state,
        }
    }

    fn active_state_mut(&mut self) -> &mut TabState {
        match self.tab {
            Tab::Changes => &mut self.changes_state,
            Tab::Files => &mut self.files_state,
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        match self.tab {
            Tab::Changes if self.changes_mode == ChangesMode::Agent => {
                filtered_change_indices(&self.changes, &self.changes_state.filter)
            }
            Tab::Changes => filtered_git_indices(&self.git_changes, &self.changes_state.filter),
            Tab::Files => filtered_file_indices(&self.files, &self.files_state.filter),
        }
    }

    fn actual_selected(&self) -> Option<usize> {
        self.filtered_indices()
            .get(self.active_state().selected)
            .copied()
    }

    fn ui_layout(&self, area: Rect) -> UiLayout {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(2),
                Constraint::Length(1),
            ])
            .split(area);
        let (navigation, content) = if self.mode == Mode::Help {
            (None, None)
        } else if area.width < 56 {
            match self.focus {
                Focus::Navigation => (Some(vertical[1]), None),
                Focus::Content => (None, Some(vertical[1])),
            }
        } else {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
                .split(vertical[1]);
            (Some(columns[0]), Some(columns[1]))
        };
        UiLayout {
            tabs: vertical[0],
            body: vertical[1],
            status: vertical[2],
            navigation,
            content,
        }
    }

    fn cursor_at_text(&self, area: Rect, column: u16, row: u16) -> Option<EditorCursor> {
        let inner = bordered_inner(area);
        let [content_area, _] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        if content_area.is_empty() {
            return None;
        }
        let selected = self.actual_selected()?;
        let lines = self.source_cache.get(&selected)?;
        let y = row.clamp(content_area.y, content_area.bottom().saturating_sub(1));
        let line =
            usize::from(y.saturating_sub(content_area.y)).saturating_add(self.files_state.scroll_y);
        let spans = lines.get(line)?;
        let line_number_width = decimal_width(lines.len());
        let code_start = line_number_width.saturating_add(3);
        let x = column.clamp(content_area.x, content_area.right().saturating_sub(1));
        let visual_column =
            usize::from(x.saturating_sub(content_area.x)).saturating_add(self.files_state.scroll_x);
        let code_end = code_start.saturating_add(source_text_width(spans));
        if visual_column < code_start {
            return None;
        }
        Some(EditorCursor {
            tab: self.tab,
            line,
            column: visual_column.min(code_end),
        })
    }

    fn set_cursor_at(&mut self, area: Rect, column: u16, row: u16) -> Option<EditorCursor> {
        let cursor = self.cursor_at_text(area, column, row);
        self.cursor = cursor;
        cursor
    }

    fn begin_selection_at(&mut self, area: Rect, column: u16, row: u16) {
        if let Some(cursor) = self.set_cursor_at(area, column, row) {
            self.cursor_blink_started = Instant::now();
            self.selection = Some(EditorSelection {
                tab: self.tab,
                anchor: cursor,
                active: cursor,
            });
        } else {
            self.selection = None;
        }
    }

    fn extend_selection_at(&mut self, area: Rect, column: u16, row: u16) {
        let Some(cursor) = self.cursor_at_text(area, column, row) else {
            return;
        };
        self.cursor = Some(cursor);
        self.cursor_blink_started = Instant::now();
        if let Some(selection) = &mut self.selection {
            if selection.tab == self.tab {
                selection.active = cursor;
                return;
            }
        }
        self.selection = Some(EditorSelection {
            tab: self.tab,
            anchor: cursor,
            active: cursor,
        });
    }

    fn selection_bounds(&self) -> Option<(EditorCursor, EditorCursor)> {
        let selection = self.selection?;
        if selection.tab != Tab::Files
            || self.tab != Tab::Files
            || selection.anchor == selection.active
        {
            return None;
        }
        let anchor = (selection.anchor.line, selection.anchor.column);
        let active = (selection.active.line, selection.active.column);
        if anchor <= active {
            Some((selection.anchor, selection.active))
        } else {
            Some((selection.active, selection.anchor))
        }
    }

    fn selection_range_for_line(&self, line: usize) -> Option<(usize, usize)> {
        let (start, end) = self.selection_bounds()?;
        if line < start.line || line > end.line {
            return None;
        }
        let start_column = if line == start.line { start.column } else { 0 };
        let end_column = if line == end.line {
            end.column.saturating_add(self.selection_cursor_width(end))
        } else {
            usize::MAX
        };
        (start_column < end_column).then_some((start_column, end_column))
    }

    fn selection_cursor_width(&self, cursor: EditorCursor) -> usize {
        let Some(selected) = self.actual_selected() else {
            return 1;
        };
        let Some(lines) = self.source_cache.get(&selected) else {
            return 1;
        };
        let line_number_width = decimal_width(lines.len());
        let code_start = line_number_width.saturating_add(3);
        let column = cursor.column.saturating_sub(code_start);
        lines
            .get(cursor.line)
            .and_then(|spans| source_char_width_at(spans, column))
            .unwrap_or(1)
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selected_source_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Err(error) = copy_to_clipboard(&text) {
            self.notices.push(format!("clipboard copy failed: {error}"));
        }
    }

    fn selected_source_text(&self) -> Option<String> {
        if self.tab != Tab::Files {
            return None;
        }
        let selected = self.actual_selected()?;
        let lines = self.source_cache.get(&selected)?;
        let (start, end) = self.selection_bounds()?;
        let line_number_width = decimal_width(lines.len());
        let code_start = line_number_width.saturating_add(3);
        let mut text = String::new();
        for line in start.line..=end.line {
            if line > start.line {
                text.push('\n');
            }
            let spans = lines.get(line)?;
            let line_start = if line == start.line {
                start.column.saturating_sub(code_start)
            } else {
                0
            };
            let line_end = if line == end.line {
                end.column
                    .saturating_add(self.selection_cursor_width(end))
                    .saturating_sub(code_start)
            } else {
                usize::MAX
            };
            text.push_str(&source_text_range(spans, line_start, line_end));
        }
        Some(text)
    }

    fn navigation_row_at(&self, area: Rect, column: u16, row: u16) -> Option<NavigationRow> {
        if !contains(area, column, row) {
            return None;
        }
        let inner = bordered_inner(area);
        if inner.is_empty() || row < inner.y || row >= inner.bottom() {
            return None;
        }
        let rows = self.navigation_rows();
        let offset = self.navigation_scroll_offset(area);
        let index = offset.saturating_add(usize::from(row.saturating_sub(inner.y)));
        rows.get(index).cloned()
    }

    fn toggle_group(&mut self, path: PathBuf) {
        let key = (self.tab, path);
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
        let max_scroll = self.navigation_rows().len().saturating_sub(1);
        let state = self.active_state_mut();
        state.list_scroll_y = state.list_scroll_y.min(max_scroll);
    }

    fn group_collapsed(&self, path: &Path) -> bool {
        self.collapsed_groups
            .contains(&(self.tab, path.to_path_buf()))
    }

    fn navigation_scroll_offset(&self, area: Rect) -> usize {
        let inner = bordered_inner(area);
        self.active_state().list_scroll_y.min(
            self.navigation_rows()
                .len()
                .saturating_sub(usize::from(inner.height)),
        )
    }

    fn navigation_rows(&self) -> Vec<NavigationRow> {
        let indices = self.filtered_indices();
        let mut rows = Vec::with_capacity(indices.len().saturating_mul(2));
        let mut current_group = None;
        for index in indices {
            let path = match self.tab {
                Tab::Changes => match self.changes_mode {
                    ChangesMode::Agent => self.changes[index].path.as_path(),
                    ChangesMode::Git => self.git_changes[index].path.as_path(),
                },
                Tab::Files => self.files[index].relative.as_path(),
            };
            let group = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf);
            if group != current_group {
                if let Some(group) = &group {
                    rows.push(NavigationRow::Group {
                        path: group.clone(),
                        label: format!("{}/", group.display()),
                    });
                }
                current_group.clone_from(&group);
            }
            if group
                .as_deref()
                .is_some_and(|group| self.group_collapsed(group))
            {
                continue;
            }
            let label = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            rows.push(NavigationRow::File {
                index,
                label,
                depth: usize::from(current_group.is_some()),
            });
        }
        rows
    }

    fn navigation_stats(&self, index: usize) -> Option<(usize, usize)> {
        if self.tab != Tab::Changes {
            return None;
        }
        match self.changes_mode {
            ChangesMode::Agent => self.diff_cache.get(&index).map(|lines| diff_stats(lines)),
            ChangesMode::Git => self
                .git_diff_cache
                .get(&index)
                .map(|lines| diff_stats(lines)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        _newest: &AtomicU64,
        tasks: &mpsc::SyncSender<Task>,
    ) {
        let layout = self.ui_layout(self.viewport);
        let left_click = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let left_drag = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left));
        let left_release = matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
        if self.mode == Mode::Help {
            if left_click {
                self.mode = Mode::Normal;
            }
            return;
        }
        if self.mode == Mode::Filter && left_click {
            self.mode = Mode::Normal;
        }

        if self.handle_active_scrollbar_drag(mouse) {
            return;
        }

        if left_click || left_drag {
            if let Some(tab) = tab_at(layout.tabs, mouse.column, mouse.row) {
                if left_click {
                    self.tab = tab;
                    self.focus = Focus::Navigation;
                    self.cursor = None;
                    self.selection = None;
                    self.clamp_selections();
                    self.request_selected(tasks);
                }
                return;
            }
        }

        if let Some(area) = layout.navigation {
            if (left_click || left_drag)
                && let Some(row) = self.navigation_row_at(area, mouse.column, mouse.row)
            {
                self.focus = Focus::Navigation;
                self.cursor = None;
                self.selection = None;
                match row {
                    NavigationRow::File { index, .. } => {
                        let selected = self
                            .filtered_indices()
                            .iter()
                            .position(|candidate| *candidate == index)
                            .unwrap_or(index);
                        self.active_state_mut().selected = selected;
                        self.active_state_mut().scroll_y = 0;
                        self.request_selected(tasks);
                    }
                    NavigationRow::Group { path, .. } if left_click => {
                        self.toggle_group(path);
                    }
                    NavigationRow::Group { .. } => {}
                }
                return;
            }
            if contains(area, mouse.column, mouse.row) {
                self.focus = Focus::Navigation;
                match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let max_offset = self
                            .navigation_rows()
                            .len()
                            .saturating_sub(usize::from(bordered_inner(area).height));
                        let state = self.active_state_mut();
                        state.list_scroll_y = match mouse.kind {
                            MouseEventKind::ScrollUp => state.list_scroll_y.saturating_sub(1),
                            MouseEventKind::ScrollDown => {
                                state.list_scroll_y.saturating_add(1).min(max_offset)
                            }
                            _ => state.list_scroll_y,
                        };
                    }
                    _ => return,
                }
                return;
            }
        }

        if let Some(area) = layout.content {
            if self.handle_content_scrollbar_mouse(area, mouse) {
                return;
            }
            if (left_click || left_drag || left_release) && contains(area, mouse.column, mouse.row)
            {
                self.focus = Focus::Content;
                if left_click {
                    self.begin_selection_at(area, mouse.column, mouse.row);
                } else {
                    self.extend_selection_at(area, mouse.column, mouse.row);
                    if left_release {
                        self.copy_selection();
                    }
                }
                self.request_selected(tasks);
                return;
            }
            if contains(area, mouse.column, mouse.row) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.active_state_mut().scroll_y =
                            self.active_state().scroll_y.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.active_state_mut().scroll_y =
                            self.active_state().scroll_y.saturating_add(3);
                    }
                    MouseEventKind::ScrollLeft => {
                        self.active_state_mut().scroll_x =
                            self.active_state().scroll_x.saturating_sub(3);
                    }
                    MouseEventKind::ScrollRight => {
                        self.active_state_mut().scroll_x =
                            self.active_state().scroll_x.saturating_add(3);
                    }
                    _ => return,
                }
                self.focus = Focus::Content;
            }
        }
    }

    fn handle_active_scrollbar_drag(&mut self, mouse: MouseEvent) -> bool {
        if self.scrollbar_drag.is_none() {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                self.update_scrollbar_drag(mouse.column, mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scrollbar_drag = None;
                true
            }
            _ => {
                self.scrollbar_drag = None;
                false
            }
        }
    }

    fn handle_content_scrollbar_mouse(&mut self, area: Rect, mouse: MouseEvent) -> bool {
        if !matches!(self.tab, Tab::Changes | Tab::Files) {
            return false;
        }
        let left_click = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let left_drag = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left));
        let left_release = matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
        if !(left_click || left_drag || left_release) {
            return false;
        }
        if left_click && let Some(drag) = self.content_scrollbar_at(area, mouse.column, mouse.row) {
            self.focus = Focus::Content;
            self.scrollbar_drag = Some(drag);
            return true;
        }
        if self.content_scrollbar_contains(area, mouse.column, mouse.row) {
            self.focus = Focus::Content;
            return true;
        }
        false
    }

    fn content_scrollbar_at(
        &self,
        panel_area: Rect,
        column: u16,
        row: u16,
    ) -> Option<ScrollbarDrag> {
        let selected = self.actual_selected()?;
        let metrics = self.content_scrollbar_metrics(panel_area, selected)?;
        for (axis, geometry) in [
            (ScrollbarAxis::Vertical, metrics.vertical),
            (ScrollbarAxis::Horizontal, metrics.horizontal),
        ] {
            let Some(geometry) = geometry else {
                continue;
            };
            if !contains(geometry.bar, column, row) {
                continue;
            }
            let pointer = match axis {
                ScrollbarAxis::Vertical => row,
                ScrollbarAxis::Horizontal => column,
            };
            let thumb_start = geometry
                .track_start
                .saturating_add(saturating_u16(geometry.thumb_start));
            let thumb_end = thumb_start.saturating_add(saturating_u16(geometry.thumb_length));
            if pointer >= thumb_start && pointer < thumb_end {
                return Some(ScrollbarDrag {
                    axis,
                    grab_offset: usize::from(pointer.saturating_sub(thumb_start)),
                });
            }
        }
        None
    }

    fn content_scrollbar_contains(&self, panel_area: Rect, column: u16, row: u16) -> bool {
        let Some(selected) = self.actual_selected() else {
            return false;
        };
        let Some(metrics) = self.content_scrollbar_metrics(panel_area, selected) else {
            return false;
        };
        metrics
            .vertical
            .is_some_and(|geometry| contains(geometry.bar, column, row))
            || metrics
                .horizontal
                .is_some_and(|geometry| contains(geometry.bar, column, row))
    }

    fn update_scrollbar_drag(&mut self, column: u16, row: u16) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let layout = self.ui_layout(self.viewport);
        let Some(panel_area) = layout.content else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(selected) = self.actual_selected() else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(metrics) = self.content_scrollbar_metrics(panel_area, selected) else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(geometry) = (match drag.axis {
            ScrollbarAxis::Vertical => metrics.vertical,
            ScrollbarAxis::Horizontal => metrics.horizontal,
        }) else {
            self.scrollbar_drag = None;
            return;
        };
        let pointer = match drag.axis {
            ScrollbarAxis::Vertical => row,
            ScrollbarAxis::Horizontal => column,
        };
        let max_thumb_start = geometry.track_length.saturating_sub(geometry.thumb_length);
        let thumb_start = usize::from(pointer.saturating_sub(geometry.track_start))
            .saturating_sub(drag.grab_offset)
            .min(max_thumb_start);
        let scroll = if max_thumb_start == 0 {
            0
        } else {
            thumb_start
                .saturating_mul(geometry.max_scroll)
                .saturating_add(max_thumb_start / 2)
                .checked_div(max_thumb_start)
                .unwrap_or_default()
        };
        match drag.axis {
            ScrollbarAxis::Vertical => self.active_state_mut().scroll_y = scroll,
            ScrollbarAxis::Horizontal => self.active_state_mut().scroll_x = scroll,
        }
    }

    fn content_scrollbar_metrics(
        &self,
        panel_area: Rect,
        selected: usize,
    ) -> Option<ContentScrollbarMetrics> {
        let area = bordered_inner(panel_area);
        if area.is_empty() {
            return None;
        }
        let [content_area, horizontal_scrollbar_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let (content_length_y, content_length_x) = match self.tab {
            Tab::Changes => {
                let (kind, path, old_path, lines) = self.selected_diff(selected)?;
                diff_content_dimensions(kind, path, old_path, lines)
            }
            Tab::Files => {
                if self.source_notices.get(&selected).is_some() {
                    return None;
                }
                let lines = self.source_cache.get(&selected)?;
                source_content_dimensions(lines)
            }
        };
        Some(ContentScrollbarMetrics {
            vertical: scrollbar_geometry(
                ScrollbarAxis::Vertical,
                content_area,
                content_length_y,
                usize::from(content_area.height),
                usize::from(content_area.height),
                self.active_state().scroll_y,
            ),
            horizontal: scrollbar_geometry(
                ScrollbarAxis::Horizontal,
                horizontal_scrollbar_area,
                content_length_x,
                usize::from(content_area.width),
                compact_horizontal_scrollbar_viewport(usize::from(content_area.width)),
                self.active_state().scroll_x,
            ),
        })
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        self.viewport = area;
        let layout = self.ui_layout(area);
        self.clamp_source_scroll(layout.content);
        let tab_index = usize::from(self.tab == Tab::Files);
        frame.render_widget(
            Tabs::new(["1 Changes", "2 Files"])
                .select(tab_index)
                .highlight_style(Style::default().fg(Color::Cyan).bold()),
            layout.tabs,
        );
        if self.mode == Mode::Help {
            Self::draw_help(frame, layout.body);
        } else if let Some(navigation) = layout.navigation {
            self.draw_navigation(frame, navigation);
            if let Some(content) = layout.content {
                self.draw_content(frame, content);
            }
        } else if let Some(content) = layout.content {
            self.draw_content(frame, content);
        }
        let status = if self.mode == Mode::Filter {
            format!("filter: {}_", self.active_state().filter)
        } else if self.loading {
            "Refreshing…".into()
        } else if let Some(error) = &self.scan_error {
            format!("Refresh failed: {error}  r retry  ? help  q close")
        } else if let Some(notice) = self.notices.first() {
            format!(
                "{} notice(s): {notice}  click/wheel/drag scrollbar  Tab focus  r refresh  ? help  q close",
                self.notices.len()
            )
        } else {
            "g: Git diff / Agent  click folders  drag scrollbars  Tab focus  / filter  ? help  q close".into()
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
            layout.status,
        );
        self.draw_cursor(frame, layout.content);
    }

    fn clamp_source_scroll(&mut self, content: Option<Rect>) {
        if self.tab != Tab::Files {
            return;
        }
        let Some(area) = content else {
            return;
        };
        let inner = bordered_inner(area);
        let Some(selected) = self.actual_selected() else {
            self.files_state.scroll_y = 0;
            self.files_state.scroll_x = 0;
            return;
        };
        let Some(lines) = self.source_cache.get(&selected) else {
            return;
        };
        let [content_area, _] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        let (_, content_width) = source_content_dimensions(lines);
        self.files_state.scroll_y = self
            .files_state
            .scroll_y
            .min(lines.len().saturating_sub(usize::from(content_area.height)));
        self.files_state.scroll_x = self
            .files_state
            .scroll_x
            .min(content_width.saturating_sub(usize::from(content_area.width)));
    }

    fn draw_cursor(&self, frame: &mut ratatui::Frame<'_>, content: Option<Rect>) {
        if self.mode == Mode::Help || self.focus != Focus::Content || self.tab == Tab::Changes {
            return;
        }
        let blink_phase = self.cursor_blink_started.elapsed().as_millis() % 1_000;
        if blink_phase >= 600 {
            return;
        }
        let Some(area) = content else {
            return;
        };
        let inner = bordered_inner(area);
        let [content_area, _] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        if content_area.is_empty() {
            return;
        }
        let state = self.active_state();
        let Some(cursor) = self.cursor.filter(|cursor| cursor.tab == self.tab) else {
            return;
        };
        let Some(line) = cursor.line.checked_sub(state.scroll_y) else {
            return;
        };
        let Some(column) = cursor.column.checked_sub(state.scroll_x) else {
            return;
        };
        if line >= usize::from(content_area.height) || column >= usize::from(content_area.width) {
            return;
        }
        frame.set_cursor_position((
            content_area.x.saturating_add(saturating_u16(column)),
            content_area.y.saturating_add(saturating_u16(line)),
        ));
    }

    fn navigation_item(
        &self,
        row: &NavigationRow,
        row_width: usize,
        selected: bool,
    ) -> ListItem<'static> {
        let gutter = if selected { "▎ " } else { "  " };
        let gutter_style = if selected {
            Style::default().fg(Color::Cyan).bold()
        } else {
            Style::default()
        };
        match row {
            NavigationRow::Group { path, label } => {
                let marker = if self.group_collapsed(path) {
                    "▸ "
                } else {
                    "▾ "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(gutter, gutter_style),
                    Span::styled(marker, Style::default().fg(Color::Cyan)),
                    Span::styled(label.clone(), Style::default().fg(Color::Cyan).bold()),
                ]))
            }
            NavigationRow::File {
                index,
                label,
                depth,
            } => {
                let (symbol, color) = if self.tab == Tab::Changes {
                    let kind = match self.changes_mode {
                        ChangesMode::Agent => self.changes[*index].kind,
                        ChangesMode::Git => self.git_changes[*index].kind,
                    };
                    change_symbol(kind)
                } else {
                    ("·", Color::DarkGray)
                };
                let stats = self.navigation_stats(*index);
                let stats_width = stats.map_or(0, |(additions, deletions)| {
                    format!("+{additions} -{deletions}").len()
                });
                let prefix = if self.tab == Tab::Changes {
                    format!("{gutter}{}{} ", "  ".repeat(*depth), symbol)
                } else {
                    format!("{gutter}{}", "  ".repeat(*depth))
                };
                let name_width = row_width
                    .saturating_sub(prefix.len())
                    .saturating_sub(stats_width)
                    .saturating_sub(1);
                let label = truncate_label(label, name_width);
                let padding = row_width
                    .saturating_sub(prefix.len())
                    .saturating_sub(label.len())
                    .saturating_sub(stats_width)
                    .max(1);
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(color).bold()),
                    Span::raw(label),
                    Span::raw(" ".repeat(padding)),
                ];
                if let Some((additions, deletions)) = stats {
                    spans.push(Span::styled(
                        format!("+{additions}"),
                        Style::default().fg(Color::Green),
                    ));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        format!("-{deletions}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
        }
    }

    fn draw_navigation(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let panel = panel_style();
        let rows = self.navigation_rows();
        let inner = bordered_inner(area);
        let row_width = usize::from(inner.width);
        let offset = self.navigation_scroll_offset(area);
        let selected_row = self.actual_selected().and_then(|selected| {
            rows.iter().position(
                |row| matches!(row, NavigationRow::File { index, .. } if *index == selected),
            )
        });
        let items: Vec<ListItem<'_>> = rows
            .iter()
            .enumerate()
            .map(|(row, item)| self.navigation_item(item, row_width, selected_row == Some(row)))
            .collect();
        let title = if self.active_state().filter.is_empty() {
            match self.tab {
                Tab::Changes => {
                    format!(
                        " {} · {} files ",
                        self.changes_mode.label(),
                        self.filtered_indices().len()
                    )
                }
                Tab::Files => " Files ".to_owned(),
            }
        } else {
            format!(" /{} ", self.active_state().filter)
        };
        let list = List::new(items)
            .style(panel)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .style(panel)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .highlight_style(
                Style::default()
                    .fg(PANEL_FOREGROUND)
                    .bg(PANEL_SELECTION)
                    .bold(),
            )
            .highlight_symbol("");
        let mut state = ListState::default().with_offset(offset);
        if let Some(row) = selected_row
            && row >= offset
            && row < offset.saturating_add(usize::from(inner.height))
        {
            state.select(Some(row));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_content(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let panel = panel_style();
        let title = match self.tab {
            Tab::Changes => self.actual_selected().map_or_else(
                || format!(" {} ", self.changes_mode.label()),
                |selected| {
                    let path = match self.changes_mode {
                        ChangesMode::Agent => &self.changes[selected].path,
                        ChangesMode::Git => &self.git_changes[selected].path,
                    };
                    format!(" {} — {} ", self.changes_mode.label(), path.display())
                },
            ),
            Tab::Files => " Read only ".to_owned(),
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .style(panel)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(selected) = self.actual_selected() else {
            let message = if let Some(error) = &self.scan_error {
                format!("Unable to scan filesystem.\n\n{error}\n\nPress r to retry.")
            } else if self.loading {
                "Scanning filesystem…".into()
            } else if self.tab == Tab::Changes && self.changes_mode == ChangesMode::Git {
                if let Some(error) = &self.git_error {
                    format!("Git diff unavailable.\n\n{error}")
                } else if self.git_state != GitState::Loaded {
                    "Reading Git status…".into()
                } else {
                    "No uncommitted Git changes.".into()
                }
            } else if self.capturing && self.tab == Tab::Changes {
                "Capturing baseline…\n\nChanges will become available when capture completes.\nFiles remains available.".into()
            } else if self.manifest.is_none() && self.tab == Tab::Changes {
                "No baseline exists.\n\nRestart this agent to capture changes from session start.\nFiles remains available.".into()
            } else if self.tab == Tab::Changes {
                "No filesystem changes since this session began.".into()
            } else {
                "No readable files.".into()
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(panel)
                    .wrap(Wrap { trim: false }),
                inner,
            );
            return;
        };
        match self.tab {
            Tab::Changes => self.draw_diff(frame, inner, selected),
            Tab::Files => self.draw_source(frame, inner, selected),
        }
    }

    fn selected_diff(&self, selected: usize) -> Option<DiffView<'_>> {
        match self.changes_mode {
            ChangesMode::Agent => {
                let change = self.changes.get(selected)?;
                Some((
                    change.kind,
                    &change.path,
                    change.old_path.as_deref(),
                    self.diff_cache.get(&selected).map(Vec::as_slice),
                ))
            }
            ChangesMode::Git => {
                let change = self.git_changes.get(selected)?;
                Some((
                    change.kind,
                    &change.path,
                    change.old_path.as_deref(),
                    self.git_diff_cache.get(&selected).map(Vec::as_slice),
                ))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_diff(&self, frame: &mut ratatui::Frame<'_>, area: Rect, selected: usize) {
        let [content_area, horizontal_scrollbar_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let Some((kind, path, old_path, lines)) = self.selected_diff(selected) else {
            frame.render_widget(
                Paragraph::new("No matching changes.").style(panel_style()),
                area,
            );
            return;
        };
        let (symbol, color) = change_symbol(kind);
        let (_, measured_width) = diff_content_dimensions(kind, path, old_path, lines);
        let diff_width = measured_width.max(usize::from(content_area.width));
        let header_style = Style::default().bg(DIFF_HEADER_BACKGROUND);
        let mut rendered = vec![Line::from(vec![
            Span::styled(
                format!("{symbol} "),
                header_style.fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                path.display().to_string(),
                header_style.fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ])];
        if let Some(old_path) = old_path {
            rendered.push(Line::from(vec![
                Span::styled("  from ", header_style.fg(Color::DarkGray)),
                Span::styled(
                    old_path.display().to_string(),
                    header_style.fg(Color::DarkGray),
                ),
            ]));
        }

        let (additions, deletions) = lines.map_or((0, 0), diff_stats);
        rendered[0].spans.push(Span::styled(
            format!("    +{additions} -{deletions}"),
            header_style.fg(Color::DarkGray),
        ));
        rendered.push(Line::styled("", Style::default().bg(DIFF_BACKGROUND)));

        let line_number_width = lines
            .into_iter()
            .flat_map(|lines| lines.iter().flat_map(|line| [line.old_line, line.new_line]))
            .flatten()
            .max()
            .map_or(1, decimal_width);
        rendered.extend(render_diff_rows(lines, line_number_width, diff_width));
        let content_width = rendered
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or(diff_width)
            .max(diff_width);
        let max_scroll_y = rendered
            .len()
            .saturating_sub(usize::from(content_area.height));
        let max_scroll_x = content_width.saturating_sub(usize::from(content_area.width));
        let scroll_y = self.changes_state.scroll_y.min(max_scroll_y);
        let scroll_x = self.changes_state.scroll_x.min(max_scroll_x);
        let rendered_len = rendered.len();
        frame.render_widget(
            Paragraph::new(rendered)
                .style(Style::default().fg(DIFF_TEXT).bg(DIFF_BACKGROUND))
                .scroll((saturating_u16(scroll_y), saturating_u16(scroll_x))),
            content_area,
        );
        if max_scroll_y > 0 {
            let mut scrollbar = ScrollbarState::new(rendered_len)
                .position(scrollbar_render_position(
                    scroll_y,
                    max_scroll_y,
                    rendered_len,
                ))
                .viewport_content_length(usize::from(content_area.height));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(DIFF_HUNK))
                    .track_style(Style::default().fg(DIFF_GUTTER_BACKGROUND)),
                content_area.inner(Margin {
                    vertical: 0,
                    horizontal: 0,
                }),
                &mut scrollbar,
            );
        }
        if max_scroll_x > 0 {
            let mut scrollbar = ScrollbarState::new(content_width)
                .position(scrollbar_render_position(
                    scroll_x,
                    max_scroll_x,
                    content_width,
                ))
                .viewport_content_length(compact_horizontal_scrollbar_viewport(usize::from(
                    content_area.width,
                )));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                    .thumb_symbol("▬")
                    .track_symbol(Some("─"))
                    .thumb_style(Style::default().fg(DIFF_HUNK))
                    .track_style(Style::default().fg(DIFF_GUTTER_BACKGROUND)),
                horizontal_scrollbar_area,
                &mut scrollbar,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_source(&self, frame: &mut ratatui::Frame<'_>, area: Rect, selected: usize) {
        if let Some(notice) = self.source_notices.get(&selected) {
            frame.render_widget(
                Paragraph::new(notice.clone()).style(panel_style().fg(Color::Yellow)),
                area,
            );
            return;
        }
        let Some(lines) = self.source_cache.get(&selected) else {
            frame.render_widget(
                Paragraph::new("Highlighting source…").style(panel_style()),
                area,
            );
            return;
        };
        let width = lines.len().max(1).ilog10() as usize + 1;
        let rendered: Vec<Line<'_>> = lines
            .iter()
            .enumerate()
            .map(|(index, spans)| {
                let mut output = vec![Span::styled(
                    format!("{:>width$} │ ", index + 1),
                    Style::default().fg(Color::DarkGray),
                )];
                let selection = self.selection_range_for_line(index);
                let mut column = width.saturating_add(3);
                for span in spans {
                    let (rendered, next_column) = render_source_span(span, column, selection);
                    output.extend(rendered);
                    column = next_column;
                }
                Line::from(output)
            })
            .collect();
        let [content_area, horizontal_scrollbar_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let (_, content_width) = source_content_dimensions(lines);
        let max_scroll_y = lines.len().saturating_sub(usize::from(content_area.height));
        let max_scroll_x = content_width.saturating_sub(usize::from(content_area.width));
        let scroll_y = self.files_state.scroll_y.min(max_scroll_y);
        let scroll_x = self.files_state.scroll_x.min(max_scroll_x);
        frame.render_widget(
            Paragraph::new(rendered)
                .style(panel_style())
                .scroll((saturating_u16(scroll_y), saturating_u16(scroll_x))),
            content_area,
        );
        if max_scroll_y > 0 {
            let mut scrollbar = ScrollbarState::new(lines.len())
                .position(scrollbar_render_position(
                    scroll_y,
                    max_scroll_y,
                    lines.len(),
                ))
                .viewport_content_length(usize::from(content_area.height));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(Color::Cyan))
                    .track_style(Style::default().fg(Color::DarkGray)),
                content_area.inner(Margin {
                    vertical: 0,
                    horizontal: 0,
                }),
                &mut scrollbar,
            );
        }
        if max_scroll_x > 0 {
            let mut scrollbar = ScrollbarState::new(content_width)
                .position(scrollbar_render_position(
                    scroll_x,
                    max_scroll_x,
                    content_width,
                ))
                .viewport_content_length(compact_horizontal_scrollbar_viewport(usize::from(
                    content_area.width,
                )));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                    .thumb_symbol("▬")
                    .track_symbol(Some("─"))
                    .thumb_style(Style::default().fg(Color::Cyan))
                    .track_style(Style::default().fg(Color::DarkGray)),
                horizontal_scrollbar_area,
                &mut scrollbar,
            );
        }
    }

    fn draw_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(
                "Herdr Agent Diff\n\n\
                 1 / 2       Changes review / Files browser\n\
                 g           Switch to Git diff / Agent diff in Changes\n\
                 Tab         navigation / diff or source focus\n\
                 Mouse       click folders to expand/collapse, tabs/items/content, drag scrollbars, wheel scroll, drag select\n\
                 ↑↓ or jk    select files / scroll content\n\
                 ←→ or hl    horizontal scroll\n\
                 /           filter active list\n\
                 r           full refresh\n\
                 ⌘C          copy selected text; mouse selections copy on release\n\
                 ?           close help\n\
                 q           close viewer\n\n\
                 Read only. Git mode uses read-only Git commands.\n\
                 Results are filesystem changes since this session began.",
            )
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn spawn_worker(
    store: StateStore,
    root: PathBuf,
    target_pane_id: String,
    newest_generation: Arc<AtomicU64>,
    tasks: mpsc::Receiver<Task>,
    results: mpsc::Sender<WorkResult>,
) {
    thread::spawn(move || {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = &themes.themes["base16-ocean.dark"];
        while let Ok(task) = tasks.recv() {
            match task {
                Task::Scan(generation) => {
                    if newest_generation.load(Ordering::Acquire) != generation {
                        continue;
                    }
                    let baseline = store.load_manifest(&target_pane_id).ok().flatten();
                    let capturing = store.capturing(&target_pane_id);
                    let result = match scan(&root) {
                        Ok((map, notices)) => {
                            let changes = baseline
                                .as_ref()
                                .map_or_else(Vec::new, |baseline| classify(baseline, &map));
                            WorkResult::Scan(Box::new(ScanResult {
                                generation,
                                baseline,
                                capturing,
                                files: map.into_values().collect(),
                                changes,
                                notices,
                                error: None,
                            }))
                        }
                        Err(error) => WorkResult::Scan(Box::new(ScanResult {
                            generation,
                            baseline,
                            capturing,
                            files: Vec::new(),
                            changes: Vec::new(),
                            notices: Vec::new(),
                            error: Some(error.to_string()),
                        })),
                    };
                    if newest_generation.load(Ordering::Acquire) != generation {
                        continue;
                    }
                    let _ = results.send(result);
                }
                Task::Diff {
                    generation,
                    index,
                    change,
                } => {
                    let lines = render_change(&store, &root, &change);
                    let _ = results.send(WorkResult::Diff {
                        generation,
                        index,
                        lines,
                    });
                }
                Task::Highlight {
                    generation,
                    index,
                    file,
                } => {
                    let (lines, notice) = highlight_file(&root, &file, &syntaxes, theme);
                    let _ = results.send(WorkResult::Highlight {
                        generation,
                        index,
                        lines,
                        notice,
                    });
                }
                Task::GitScan { generation } => {
                    let _ = results.send(git_scan_result(&root, generation));
                }
                Task::GitDiff {
                    generation,
                    index,
                    change,
                } => {
                    let _ = results.send(git_diff_result(&root, generation, index, &change));
                }
            }
        }
    });
}

fn git_scan_result(root: &Path, generation: u64) -> WorkResult {
    match scan_git(root) {
        Ok(changes) => WorkResult::GitScan {
            generation,
            changes,
            error: None,
        },
        Err(error) => WorkResult::GitScan {
            generation,
            changes: Vec::new(),
            error: Some(error),
        },
    }
}

fn git_diff_result(root: &Path, generation: u64, index: usize, change: &GitChange) -> WorkResult {
    let lines = render_git_diff(root, change).unwrap_or_else(|error| {
        vec![DiffLine {
            kind: DiffLineKind::Notice,
            text: error,
            old_line: None,
            new_line: None,
        }]
    });
    WorkResult::GitDiff {
        generation,
        index,
        lines,
    }
}

fn highlight_file(
    root: &Path,
    file: &CurrentFile,
    syntaxes: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
) -> (Vec<Vec<ColoredSpan>>, Option<String>) {
    if file.text != TextEligibility::Text {
        return (
            Vec::new(),
            Some(format!(
                "{}\n\nMetadata-only file: {:?}, {} bytes",
                file.relative.display(),
                file.text,
                file.size
            )),
        );
    }
    let bytes = match safe_read(root, &file.relative, crate::model::INLINE_TEXT_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return (Vec::new(), Some("file is not valid UTF-8".into()));
    };
    let syntax = syntaxes
        .find_syntax_for_file(&file.relative)
        .ok()
        .flatten()
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let default_foreground = theme.settings.foreground;
    let mut highlighter = HighlightLines::new(syntax, theme);
    let lines = LinesWithEndings::from(&text)
        .map(|line| {
            highlighter
                .highlight_line(line, syntaxes)
                .unwrap_or_default()
                .into_iter()
                .map(|(style, text)| ColoredSpan {
                    text: text.trim_end_matches(['\r', '\n']).to_owned(),
                    foreground: if default_foreground == Some(style.foreground) {
                        Color::White
                    } else {
                        terminal_color(style.foreground.r, style.foreground.g, style.foreground.b)
                    },
                })
                .collect()
        })
        .collect();
    (lines, None)
}

fn filtered_change_indices(changes: &[Change], filter: &str) -> Vec<usize> {
    let needle = filter.to_ascii_lowercase();
    changes
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            needle.is_empty()
                || change
                    .path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

fn filtered_git_indices(changes: &[GitChange], filter: &str) -> Vec<usize> {
    let needle = filter.to_ascii_lowercase();
    changes
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            needle.is_empty()
                || change
                    .path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(&needle)
                || change.old_path.as_ref().is_some_and(|old_path| {
                    old_path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&needle)
                })
        })
        .map(|(index, _)| index)
        .collect()
}

fn filtered_file_indices(files: &[CurrentFile], filter: &str) -> Vec<usize> {
    let needle = filter.to_ascii_lowercase();
    files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            needle.is_empty()
                || file
                    .relative
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("pbcopy stdin is unavailable"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("pbcopy exited unsuccessfully"))
    }
}

fn panel_style() -> Style {
    Style::default().fg(PANEL_FOREGROUND).bg(PANEL_BACKGROUND)
}

fn render_source_span(
    span: &ColoredSpan,
    start_column: usize,
    selection: Option<(usize, usize)>,
) -> (Vec<Span<'static>>, usize) {
    let mut rendered = Vec::new();
    let mut segment = String::new();
    let mut segment_selected = None;
    let mut column = start_column;
    for character in span.text.chars() {
        let width = character.width().unwrap_or(0);
        let selected = selection.is_some_and(|(start, end)| {
            let occupied_end = column.saturating_add(width.max(1));
            column < end && occupied_end > start
        });
        if segment_selected != Some(selected) && !segment.is_empty() {
            rendered.push(styled_source_segment(
                std::mem::take(&mut segment),
                span.foreground,
                segment_selected == Some(true),
            ));
        }
        segment_selected = Some(selected);
        segment.push(character);
        column = column.saturating_add(width);
    }
    if !segment.is_empty() {
        rendered.push(styled_source_segment(
            segment,
            span.foreground,
            segment_selected == Some(true),
        ));
    }
    (rendered, column)
}

fn source_text_width(spans: &[ColoredSpan]) -> usize {
    spans
        .iter()
        .flat_map(|span| span.text.chars())
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

fn source_text_range(spans: &[ColoredSpan], start: usize, end: usize) -> String {
    let mut output = String::new();
    let mut column: usize = 0;
    for character in spans.iter().flat_map(|span| span.text.chars()) {
        let width = character.width().unwrap_or(0);
        let occupied_end = column.saturating_add(width.max(1));
        if column < end && occupied_end > start {
            output.push(character);
        }
        column = column.saturating_add(width);
    }
    output
}

fn source_char_width_at(spans: &[ColoredSpan], target: usize) -> Option<usize> {
    let mut column: usize = 0;
    for character in spans.iter().flat_map(|span| span.text.chars()) {
        let width = character.width().unwrap_or(0);
        let occupied_end = column.saturating_add(width.max(1));
        if target >= column && target < occupied_end {
            return Some(width.max(1));
        }
        column = column.saturating_add(width);
    }
    None
}

fn styled_source_segment(text: String, foreground: Color, selected: bool) -> Span<'static> {
    let mut style = Style::default().fg(foreground).add_modifier(Modifier::BOLD);
    if selected {
        style = style.bg(PANEL_SELECTION);
    }
    Span::styled(text, style)
}

fn bordered_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn tab_at(area: Rect, column: u16, row: u16) -> Option<Tab> {
    if !contains(area, column, row) {
        return None;
    }
    let relative = usize::from(column.saturating_sub(area.x));
    let changes_width = "1 Changes".len() + 2;
    let files_width = "2 Files".len() + 2;
    if relative < changes_width + 1 {
        Some(Tab::Changes)
    } else if relative < changes_width + 1 + files_width {
        Some(Tab::Files)
    } else {
        None
    }
}

fn decimal_width(value: usize) -> usize {
    value.max(1).to_string().len()
}

fn truncate_label(label: &str, max_width: usize) -> String {
    if label
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum::<usize>()
        <= max_width
    {
        return label.to_owned();
    }
    if max_width <= 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut width: usize = 0;
    for character in label.chars() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width) >= max_width {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output.push('…');
    output
}

fn change_symbol(kind: ChangeKind) -> (&'static str, Color) {
    match kind {
        ChangeKind::Added => ("A", Color::Green),
        ChangeKind::Modified => ("M", Color::Magenta),
        ChangeKind::Deleted => ("D", Color::Red),
        ChangeKind::Renamed => ("R", Color::Cyan),
    }
}

fn diff_stats(lines: &[DiffLine]) -> (usize, usize) {
    lines
        .iter()
        .fold((0, 0), |(additions, deletions), line| match line.kind {
            DiffLineKind::Addition => (additions + 1, deletions),
            DiffLineKind::Deletion => (additions, deletions + 1),
            _ => (additions, deletions),
        })
}

fn diff_content_dimensions(
    kind: ChangeKind,
    path: &Path,
    old_path: Option<&Path>,
    lines: Option<&[DiffLine]>,
) -> (usize, usize) {
    let (symbol, _) = change_symbol(kind);
    let (additions, deletions) = lines.map_or((0, 0), diff_stats);
    let mut width = format!("{symbol} {}    +{additions} -{deletions}", path.display()).width();
    if let Some(old_path) = old_path {
        width = width.max(format!("  from {}", old_path.display()).width());
    }

    let line_number_width = lines
        .into_iter()
        .flat_map(|lines| lines.iter().flat_map(|line| [line.old_line, line.new_line]))
        .flatten()
        .max()
        .map_or(1, decimal_width);
    let diff_lines = lines.map_or_else(
        || vec!["Generating diff…".width()],
        |lines| {
            lines
                .iter()
                .map(|line| diff_line_width(line, line_number_width))
                .collect()
        },
    );
    width = width.max(diff_lines.into_iter().max().unwrap_or(0));

    let rendered_len = 2_usize
        .saturating_add(usize::from(old_path.is_some()))
        .saturating_add(lines.map_or(1, <[DiffLine]>::len));
    (rendered_len, width)
}

fn source_content_dimensions(lines: &[Vec<ColoredSpan>]) -> (usize, usize) {
    let line_number_width = decimal_width(lines.len());
    let content_width = lines
        .iter()
        .map(|spans| line_number_width.saturating_add(3 + source_text_width(spans)))
        .max()
        .unwrap_or(0);
    (lines.len(), content_width)
}

fn diff_line_width(line: &DiffLine, line_number_width: usize) -> usize {
    if matches!(line.kind, DiffLineKind::Hunk | DiffLineKind::Header) {
        return diff_line_label(line).width();
    }
    let prefix_length = line.text.chars().next().map_or(0, char::len_utf8);
    let (prefix, body) = line.text.split_at(prefix_length);
    line_number_width
        .saturating_mul(2)
        .saturating_add(3)
        .saturating_add(prefix.width())
        .saturating_add(body.width())
}

fn scrollbar_geometry(
    axis: ScrollbarAxis,
    area: Rect,
    content_length: usize,
    viewport_length: usize,
    thumb_viewport_length: usize,
    position: usize,
) -> Option<ScrollbarGeometry> {
    if area.is_empty() || content_length <= viewport_length || thumb_viewport_length == 0 {
        return None;
    }
    let (bar, track_length, track_start) = match axis {
        ScrollbarAxis::Vertical if area.height >= 2 => (
            Rect::new(area.right().saturating_sub(1), area.y, 1, area.height),
            usize::from(area.height.saturating_sub(2)),
            area.y.saturating_add(1),
        ),
        ScrollbarAxis::Horizontal if area.width >= 2 => (
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            usize::from(area.width.saturating_sub(2)),
            area.x.saturating_add(1),
        ),
        _ => return None,
    };
    if track_length == 0 {
        return None;
    }
    let max_scroll = content_length.saturating_sub(viewport_length);
    let scrollbar_position =
        scrollbar_render_position(position.min(max_scroll), max_scroll, content_length);
    let (thumb_start, thumb_length) = scrollbar_thumb_parts(
        content_length,
        thumb_viewport_length,
        track_length,
        scrollbar_position,
    );
    Some(ScrollbarGeometry {
        bar,
        track_start,
        track_length,
        thumb_start,
        thumb_length,
        max_scroll,
    })
}

fn compact_horizontal_scrollbar_viewport(viewport_length: usize) -> usize {
    // Keep the horizontal handle visually compact in a wide diff pane.
    viewport_length.saturating_div(3).max(1)
}

fn scrollbar_render_position(position: usize, max_scroll: usize, content_length: usize) -> usize {
    if max_scroll == 0 {
        0
    } else {
        position
            .min(max_scroll)
            .saturating_mul(content_length.saturating_sub(1))
            .checked_div(max_scroll)
            .unwrap_or_default()
    }
}

fn scrollbar_thumb_parts(
    content_length: usize,
    viewport_length: usize,
    track_length: usize,
    position: usize,
) -> (usize, usize) {
    let max_position = content_length.saturating_sub(1);
    let max_viewport_position = max_position.saturating_add(viewport_length);
    if track_length == 0 || max_viewport_position == 0 {
        return (0, 0);
    }
    let thumb_length = viewport_length
        .saturating_mul(track_length)
        .saturating_add(max_viewport_position / 2)
        / max_viewport_position;
    let thumb_length = thumb_length.clamp(1, track_length);
    let thumb_start = position
        .saturating_mul(track_length)
        .saturating_add(max_viewport_position / 2)
        / max_viewport_position;
    (
        thumb_start.min(track_length.saturating_sub(thumb_length)),
        thumb_length,
    )
}

fn render_diff_rows(
    lines: Option<&[DiffLine]>,
    line_number_width: usize,
    row_width: usize,
) -> Vec<Line<'static>> {
    lines.map_or_else(
        || {
            vec![Line::styled(
                "Generating diff…",
                Style::default().fg(Color::DarkGray).bg(DIFF_BACKGROUND),
            )]
        },
        |lines| {
            lines
                .iter()
                .map(|line| render_diff_line(line, line_number_width, row_width))
                .collect()
        },
    )
}

fn render_diff_line(line: &DiffLine, line_number_width: usize, row_width: usize) -> Line<'static> {
    if matches!(line.kind, DiffLineKind::Hunk | DiffLineKind::Header) {
        let label = diff_line_label(line);
        let style = if line.kind == DiffLineKind::Hunk {
            Style::default()
                .fg(DIFF_HUNK)
                .bg(DIFF_HEADER_BACKGROUND)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan).bg(DIFF_HEADER_BACKGROUND)
        };
        return Line::from(vec![
            Span::styled(label.clone(), style),
            Span::styled(" ".repeat(row_width.saturating_sub(label.width())), style),
        ]);
    }
    let old_number = line.old_line.map_or_else(
        || " ".repeat(line_number_width),
        |number| format!("{number:>line_number_width$}"),
    );
    let new_number = line.new_line.map_or_else(
        || " ".repeat(line_number_width),
        |number| format!("{number:>line_number_width$}"),
    );
    let prefix = line.text.chars().next().unwrap_or(' ');
    let body = line.text.get(prefix.len_utf8()..).unwrap_or_default();
    let row_style = diff_row_style(line.kind);
    let gutter_style = Style::default()
        .fg(DIFF_LINE_NUMBER)
        .bg(DIFF_GUTTER_BACKGROUND);
    let marker_style = row_style
        .fg(diff_line_color(line.kind))
        .add_modifier(Modifier::BOLD);
    let body_style = row_style.fg(diff_line_color(line.kind));
    let gutter = format!("{old_number} {new_number} ");
    let marker = format!("{prefix} ");
    let occupied_width = gutter
        .width()
        .saturating_add(marker.width())
        .saturating_add(body.width());
    Line::from(vec![
        Span::styled(gutter, gutter_style),
        Span::styled(marker, marker_style),
        Span::styled(body.to_owned(), body_style),
        Span::styled(
            " ".repeat(row_width.saturating_sub(occupied_width)),
            row_style,
        ),
    ])
}

fn diff_line_label(line: &DiffLine) -> String {
    if line.kind == DiffLineKind::Hunk {
        "··· unchanged lines ···".to_owned()
    } else {
        line.text.clone()
    }
}

fn diff_row_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Addition => Style::default().bg(DIFF_ADDITION_BACKGROUND),
        DiffLineKind::Deletion => Style::default().bg(DIFF_DELETION_BACKGROUND),
        DiffLineKind::Context => Style::default().bg(DIFF_BACKGROUND),
        DiffLineKind::Notice | DiffLineKind::Header | DiffLineKind::Hunk => {
            Style::default().bg(DIFF_HEADER_BACKGROUND)
        }
    }
}

fn diff_line_color(kind: DiffLineKind) -> Color {
    match kind {
        DiffLineKind::Addition => DIFF_ADDITION,
        DiffLineKind::Deletion => DIFF_DELETION,
        DiffLineKind::Hunk | DiffLineKind::Header => DIFF_HUNK,
        DiffLineKind::Notice => Color::Yellow,
        DiffLineKind::Context => DIFF_TEXT,
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn terminal_color(red: u8, green: u8, blue: u8) -> Color {
    let red = brighten_code_component(red);
    let green = brighten_code_component(green);
    let blue = brighten_code_component(blue);
    let true_color = std::env::var("COLORTERM")
        .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"));
    if true_color {
        Color::Rgb(red, green, blue)
    } else {
        let component =
            |value: u8| -> u8 { u8::try_from((u16::from(value) * 5 + 127) / 255).unwrap_or(5) };
        Color::Indexed(16 + 36 * component(red) + 6 * component(green) + component(blue))
    }
}

fn brighten_code_component(value: u8) -> u8 {
    let value = u16::from(value).saturating_mul(125) / 100;
    u8::try_from(value).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use tempfile::TempDir;

    use super::{
        App, Cache, ChangesMode, Focus, Tab, Task, WorkResult, brighten_code_component,
        highlight_file, saturating_u16,
    };
    use crate::diff::{DiffLine, DiffLineKind};
    use crate::model::{Change, ChangeKind, CurrentFile, TextEligibility};
    use crate::snapshot::scan;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn file(path: &str) -> CurrentFile {
        CurrentFile {
            relative: path.into(),
            absolute: path.into(),
            size: 0,
            modified_unix_ns: None,
            hash: None,
            text: TextEligibility::Text,
        }
    }

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change {
            kind,
            path: path.into(),
            old_path: None,
            baseline: None,
            current: None,
        }
    }

    #[test]
    fn missing_baseline_keeps_files_tab_available() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.apply_result(WorkResult::Scan(Box::new(super::ScanResult {
            generation: 1,
            baseline: None,
            capturing: false,
            files: Vec::new(),
            changes: Vec::new(),
            notices: Vec::new(),
            error: None,
        })));
        app.focus = Focus::Content;
        let changes = render(&mut app, 80, 18);
        assert!(changes.contains("Restart this agent"));
        app.tab = Tab::Files;
        let files = render(&mut app, 80, 18);
        assert!(files.contains("No readable files."));
    }

    #[test]
    fn capture_in_progress_has_a_distinct_nonfatal_view() {
        let mut app = App::new(None, true, "w1:p1".into());
        app.loading = false;
        app.focus = Focus::Content;
        let output = render(&mut app, 80, 14);
        assert!(output.contains("Capturing baseline"));
    }

    #[test]
    fn g_switches_changes_between_agent_and_git_modes() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &newest,
            &tasks,
        );

        assert_eq!(app.changes_mode, ChangesMode::Git);
        assert_eq!(app.git_state, super::GitState::Loading);
        assert!(matches!(queued.try_recv(), Ok(Task::GitScan { .. })));
    }

    #[test]
    fn scan_invalidates_in_flight_render_results() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.render_generation = 7;
        app.apply_result(WorkResult::Scan(Box::new(super::ScanResult {
            generation: 1,
            baseline: None,
            capturing: false,
            files: Vec::new(),
            changes: Vec::new(),
            notices: Vec::new(),
            error: None,
        })));

        assert_eq!(app.render_generation, 8);
        app.apply_result(WorkResult::Diff {
            generation: 7,
            index: 0,
            lines: Vec::new(),
        });
        assert!(app.diff_cache.get(&0).is_none());
    }

    #[test]
    fn refresh_invalidates_render_results_before_the_scan_finishes() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.render_generation = 7;
        let (tasks, _queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.refresh(&newest, &tasks);

        assert_eq!(app.render_generation, 8);
        assert!(app.loading);
        app.apply_result(WorkResult::Diff {
            generation: 7,
            index: 0,
            lines: Vec::new(),
        });
        assert!(app.diff_cache.get(&0).is_none());
    }

    #[test]
    fn refresh_does_not_queue_a_second_scan_while_one_is_in_flight() {
        let mut app = App::new(None, true, "w1:p1".into());
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.refresh(&newest, &tasks);

        assert!(queued.try_recv().is_err());
        assert_eq!(app.generation, 1);
        assert!(app.loading);
    }

    #[test]
    fn narrow_layout_switches_between_navigation_and_content() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        let navigation = render(&mut app, 42, 12);
        assert!(navigation.contains("Changes"));
        app.focus = Focus::Content;
        let content = render(&mut app, 42, 12);
        assert!(content.contains("No baseline exists."));
    }

    #[test]
    fn changes_navigation_groups_files_and_keeps_file_clicks_mapped() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.changes = vec![
            change("src/core/types.ts", ChangeKind::Modified),
            change("src/core/config.ts", ChangeKind::Added),
            change("src/ui/App.tsx", ChangeKind::Modified),
        ];
        app.diff_cache.insert(
            0,
            vec![DiffLine {
                kind: DiffLineKind::Addition,
                text: "+added".into(),
                old_line: None,
                new_line: Some(1),
            }],
            6,
        );
        app.viewport = Rect::new(0, 0, 100, 20);

        let rows = app.navigation_rows();
        assert!(
            matches!(rows[0], super::NavigationRow::Group { ref label, .. } if label == "src/core/")
        );
        assert!(matches!(
            rows[1],
            super::NavigationRow::File { index: 0, .. }
        ));
        assert!(matches!(
            rows[2],
            super::NavigationRow::File { index: 1, .. }
        ));
        assert!(
            matches!(rows[3], super::NavigationRow::Group { ref label, .. } if label == "src/ui/")
        );
        assert!(matches!(
            rows[4],
            super::NavigationRow::File { index: 2, .. }
        ));

        let layout = app.ui_layout(app.viewport);
        let navigation = layout.navigation.expect("wide layout has navigation");
        assert!(matches!(
            app.navigation_row_at(navigation, navigation.x + 2, navigation.y + 3),
            Some(super::NavigationRow::File { index: 1, .. })
        ));
        let output = render(&mut app, 100, 20);
        assert!(output.contains("src/core/"));
        assert!(output.contains("+1 -0"));
    }

    #[test]
    fn files_navigation_uses_the_same_grouped_tree_style() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("src/app.rs"), file("src/lib.rs"), file("README.md")];
        app.diff_cache.insert(0, Vec::new(), 0);

        let rows = app.navigation_rows();
        assert!(
            matches!(rows[0], super::NavigationRow::Group { ref label, .. } if label == "src/")
        );
        assert!(matches!(
            rows[1],
            super::NavigationRow::File { index: 0, ref label, depth: 1 } if label == "app.rs"
        ));
        assert!(matches!(
            rows[2],
            super::NavigationRow::File { index: 1, ref label, depth: 1 } if label == "lib.rs"
        ));
        assert!(matches!(
            rows[3],
            super::NavigationRow::File { index: 2, ref label, depth: 0 } if label == "README.md"
        ));
        assert_eq!(app.navigation_stats(0), None);

        let output = render(&mut app, 100, 20);
        assert!(output.contains("▾ src/"));
        assert!(output.contains("app.rs"));
        assert!(!output.contains("src/app.rs"));
    }

    #[test]
    fn clicking_a_folder_collapses_and_reopens_its_files() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("README.md"), file("src/app.rs"), file("src/lib.rs")];
        app.viewport = Rect::new(0, 0, 100, 20);
        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);
        let navigation = app.ui_layout(app.viewport).navigation.expect("navigation");

        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                navigation.x + 2,
                navigation.y + 2,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.navigation_rows().len(), 2);
        assert!(matches!(
            app.navigation_rows()[1],
            super::NavigationRow::Group { ref label, .. } if label == "src/"
        ));

        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                navigation.x + 2,
                navigation.y + 2,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.navigation_rows().len(), 4);
    }

    #[test]
    fn each_tab_preserves_filter_selection_and_scroll() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.changes_state.filter = "src".into();
        app.changes_state.selected = 3;
        app.changes_state.scroll_y = 9;
        app.tab = Tab::Files;
        app.files_state.filter = "test".into();
        app.files_state.selected = 1;
        app.files_state.scroll_y = 4;
        app.tab = Tab::Changes;
        assert_eq!(app.active_state().filter, "src");
        assert_eq!(app.active_state().selected, 3);
        assert_eq!(app.active_state().scroll_y, 9);
        app.tab = Tab::Files;
        assert_eq!(app.active_state().filter, "test");
        assert_eq!(app.active_state().selected, 1);
        assert_eq!(app.active_state().scroll_y, 4);
    }

    #[test]
    fn mouse_click_switches_tabs_and_selects_list_items() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.viewport = Rect::new(0, 0, 80, 18);
        app.files = (0..30)
            .map(|index| file(&format!("file{index}.rs")))
            .collect();
        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 16, 0),
            &newest,
            &tasks,
        );
        assert_eq!(app.tab, Tab::Files);
        assert_eq!(app.focus, Focus::Navigation);

        app.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 4, 3),
            &newest,
            &tasks,
        );
        assert_eq!(app.files_state.selected, 1);

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 4, 3), &newest, &tasks);
        assert_eq!(app.files_state.selected, 1);
        assert_eq!(app.files_state.list_scroll_y, 1);
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 4, 3), &newest, &tasks);
        assert_eq!(app.files_state.selected, 1);
        assert_eq!(app.files_state.list_scroll_y, 0);
    }

    #[test]
    fn file_list_scrolls_without_changing_the_selected_file() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.files = (0..30)
            .map(|index| file(&format!("file{index}.rs")))
            .collect();
        app.tab = Tab::Files;
        app.files_state.selected = 0;
        app.files_state.list_scroll_y = 1;

        let output = render(&mut app, 80, 18);

        assert!(output.contains("file1.rs"));
        assert!(!output.contains("file0.rs"));
        assert_eq!(app.files_state.selected, 0);
    }

    #[test]
    fn read_only_source_shows_a_vertical_scrollbar_when_needed() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("main.rs")];
        app.source_cache.insert(
            0,
            (0..30)
                .map(|index| {
                    vec![super::ColoredSpan {
                        text: format!("line {index}"),
                        foreground: Color::White,
                    }]
                })
                .collect(),
            30,
        );

        let output = render(&mut app, 80, 18);

        assert!(output.contains('▲'));
        assert!(output.contains('▼'));
    }

    #[test]
    fn selected_read_only_source_text_excludes_line_numbers() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("main.rs")];
        app.source_cache.insert(
            0,
            vec![
                vec![super::ColoredSpan {
                    text: "fn main() {".into(),
                    foreground: Color::White,
                }],
                vec![super::ColoredSpan {
                    text: "    println!(\"hi\");".into(),
                    foreground: Color::White,
                }],
                vec![super::ColoredSpan {
                    text: "}".into(),
                    foreground: Color::White,
                }],
            ],
            32,
        );
        app.selection = Some(super::EditorSelection {
            tab: Tab::Files,
            anchor: super::EditorCursor {
                tab: Tab::Files,
                line: 0,
                column: 7,
            },
            active: super::EditorCursor {
                tab: Tab::Files,
                line: 2,
                column: 5,
            },
        });

        assert_eq!(
            app.selected_source_text().as_deref(),
            Some("main() {\n    println!(\"hi\");\n}")
        );
    }

    #[test]
    fn dragging_past_source_text_clamps_to_the_line_end() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("main.rs")];
        app.source_cache.insert(
            0,
            vec![vec![super::ColoredSpan {
                text: "abc".into(),
                foreground: Color::White,
            }]],
            3,
        );
        app.viewport = Rect::new(0, 0, 80, 18);
        let content = app.ui_layout(app.viewport).content.expect("content pane");
        let inner = super::bordered_inner(content);
        let [source_area, _] = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .areas(inner);

        assert_eq!(
            app.cursor_at_text(content, source_area.x.saturating_add(20), source_area.y,),
            Some(super::EditorCursor {
                tab: Tab::Files,
                line: 0,
                column: 7,
            })
        );
    }

    #[test]
    fn mouse_click_places_editor_cursor_and_wheel_scrolls_content() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.focus = Focus::Content;
        app.viewport = Rect::new(0, 0, 80, 18);
        app.files = vec![file("main.rs"), file("lib.rs"), file("tests.rs")];
        app.source_cache.insert(
            0,
            vec![
                vec![super::ColoredSpan {
                    text: "fn main() {}".into(),
                    foreground: Color::White,
                }],
                vec![super::ColoredSpan {
                    text: "fn main() {}".into(),
                    foreground: Color::White,
                }],
            ],
            24,
        );
        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 35, 3),
            &newest,
            &tasks,
        );
        assert_eq!(app.files_state.selected, 0);
        assert_eq!(
            app.cursor,
            Some(super::EditorCursor {
                tab: Tab::Files,
                line: 1,
                column: 5,
            })
        );
        app.handle_mouse(mouse(MouseEventKind::Moved, 40, 3), &newest, &tasks);
        assert_eq!(
            app.cursor,
            Some(super::EditorCursor {
                tab: Tab::Files,
                line: 1,
                column: 5,
            })
        );

        app.handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 40, 3),
            &newest,
            &tasks,
        );
        app.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 40, 3),
            &newest,
            &tasks,
        );
        assert_eq!(
            app.selection,
            Some(super::EditorSelection {
                tab: Tab::Files,
                anchor: super::EditorCursor {
                    tab: Tab::Files,
                    line: 1,
                    column: 5,
                },
                active: super::EditorCursor {
                    tab: Tab::Files,
                    line: 1,
                    column: 10,
                },
            })
        );
        assert_eq!(
            app.selection_bounds()
                .map(|(start, end)| (start.column, end.column)),
            Some((5, 10))
        );

        app.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 3),
            &newest,
            &tasks,
        );
        assert_eq!(app.cursor, None);
        assert_eq!(app.selection, None);

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 35, 3), &newest, &tasks);
        assert_eq!(app.files_state.scroll_y, 3);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn diff_scrollbars_drag_both_axes() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.focus = Focus::Content;
        app.viewport = Rect::new(0, 0, 100, 20);
        app.changes = vec![change("src/main.rs", ChangeKind::Modified)];
        let mut lines = (0..80)
            .map(|line| DiffLine {
                kind: DiffLineKind::Context,
                text: format!(" line {line}"),
                old_line: Some(line + 1),
                new_line: Some(line + 1),
            })
            .collect::<Vec<_>>();
        lines.push(DiffLine {
            kind: DiffLineKind::Addition,
            text: format!("+{}", "x".repeat(180)),
            old_line: None,
            new_line: Some(81),
        });
        app.diff_cache.insert(0, lines, 10_000);

        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);
        let content = app
            .ui_layout(app.viewport)
            .content
            .expect("wide layout has content");
        let metrics = app
            .content_scrollbar_metrics(content, 0)
            .expect("diff metrics");
        let vertical = metrics.vertical.expect("vertical scrollbar");
        let vertical_thumb = vertical
            .track_start
            .saturating_add(saturating_u16(vertical.thumb_start));
        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                vertical.bar.x,
                vertical_thumb,
            ),
            &newest,
            &tasks,
        );
        app.handle_mouse(
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                vertical.bar.x,
                vertical.track_start.saturating_add(saturating_u16(
                    vertical.track_length.saturating_sub(vertical.thumb_length),
                )),
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.changes_state.scroll_y, vertical.max_scroll);
        app.handle_mouse(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                vertical.bar.x,
                vertical_thumb,
            ),
            &newest,
            &tasks,
        );

        let horizontal = metrics.horizontal.expect("horizontal scrollbar");
        assert!(horizontal.thumb_length.saturating_mul(2) < horizontal.track_length);
        let horizontal_thumb = horizontal
            .track_start
            .saturating_add(saturating_u16(horizontal.thumb_start));
        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                horizontal_thumb,
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        app.handle_mouse(
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                horizontal.track_start.saturating_add(saturating_u16(
                    horizontal
                        .track_length
                        .saturating_sub(horizontal.thumb_length),
                )),
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.changes_state.scroll_x, horizontal.max_scroll);
        app.handle_mouse(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                horizontal_thumb,
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.scrollbar_drag, None);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn source_scrollbars_drag_both_axes() {
        let mut app = App::new(None, false, "w1:p1".into());
        app.loading = false;
        app.focus = Focus::Content;
        app.tab = Tab::Files;
        app.viewport = Rect::new(0, 0, 100, 20);
        app.files = vec![file("src/main.rs")];
        app.source_cache.insert(
            0,
            (0..80)
                .map(|line| {
                    vec![super::ColoredSpan {
                        text: format!("fn line_{line}() {{ {} }}", "x".repeat(170)),
                        foreground: Color::White,
                    }]
                })
                .collect(),
            20_000,
        );
        let output = render(&mut app, 100, 20);
        assert!(output.contains('◄'));

        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);
        let content = app
            .ui_layout(app.viewport)
            .content
            .expect("wide layout has content");
        let metrics = app
            .content_scrollbar_metrics(content, 0)
            .expect("source metrics");
        let vertical = metrics.vertical.expect("vertical scrollbar");
        let vertical_thumb = vertical
            .track_start
            .saturating_add(saturating_u16(vertical.thumb_start));
        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                vertical.bar.x,
                vertical_thumb,
            ),
            &newest,
            &tasks,
        );
        app.handle_mouse(
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                vertical.bar.x,
                vertical.track_start.saturating_add(saturating_u16(
                    vertical.track_length.saturating_sub(vertical.thumb_length),
                )),
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.files_state.scroll_y, vertical.max_scroll);
        app.handle_mouse(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                vertical.bar.x,
                vertical_thumb,
            ),
            &newest,
            &tasks,
        );

        let horizontal = metrics.horizontal.expect("horizontal scrollbar");
        assert!(horizontal.thumb_length.saturating_mul(2) < horizontal.track_length);
        let horizontal_thumb = horizontal
            .track_start
            .saturating_add(saturating_u16(horizontal.thumb_start));
        app.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                horizontal_thumb,
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        app.handle_mouse(
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                horizontal.track_start.saturating_add(saturating_u16(
                    horizontal
                        .track_length
                        .saturating_sub(horizontal.thumb_length),
                )),
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.files_state.scroll_x, horizontal.max_scroll);
        app.handle_mouse(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                horizontal_thumb,
                horizontal.bar.y,
            ),
            &newest,
            &tasks,
        );
        assert_eq!(app.scrollbar_drag, None);
    }

    #[test]
    fn retained_byte_cache_evicts_old_entries() {
        let mut cache = Cache::new();
        cache.insert(1, "old", super::CACHE_LIMIT);
        cache.insert(2, "new", 1);
        assert!(cache.get(&1).is_none());
        assert_eq!(cache.get(&2), Some(&"new"));
    }

    #[test]
    fn rust_source_is_syntax_highlighted_with_line_content() {
        let project = TempDir::new().expect("project");
        fs::write(
            project.path().join("main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .expect("source");
        let (files, _) = scan(project.path()).expect("scan");
        let file = files.get(std::path::Path::new("main.rs")).expect("file");
        let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let themes = syntect::highlighting::ThemeSet::load_defaults();
        let (lines, notice) = highlight_file(
            project.path(),
            file,
            &syntaxes,
            &themes.themes["base16-ocean.dark"],
        );
        assert!(notice.is_none());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].iter().any(|span| span.text.contains("fn")));
        assert!(lines[0].iter().any(|span| span.text.contains("main")));
        assert!(
            lines[0]
                .iter()
                .any(|span| span.foreground == ratatui::style::Color::White)
        );
    }

    #[test]
    fn syntax_colors_are_brightened_without_changing_background() {
        assert_eq!(brighten_code_component(100), 125);
        assert_eq!(brighten_code_component(255), 255);
    }
}
