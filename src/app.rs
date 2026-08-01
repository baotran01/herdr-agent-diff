use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
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
use notify::{EventKind, RecursiveMode, Watcher};
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
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::diff::{DiffLine, DiffLineKind};
use crate::git::{
    GitChange, GitComparison, GitFileState, diff as render_git_diff, scan as scan_git,
    unpushed_commit_count,
};
use crate::herdr::{Herdr, pane_exists};
use crate::model::{ChangeKind, CurrentFile, TextEligibility};
use crate::snapshot::{safe_read, scan};
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
const FILE_ICON_SLOT_WIDTH: usize = 3;
const HIGHLIGHT_CHUNK_LINES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIcon {
    glyph: &'static str,
    color: Color,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Tab {
    Changes,
    Files,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangesMode {
    Git,
    Unpushed,
}

impl ChangesMode {
    fn label(self) -> &'static str {
        match self {
            Self::Git => "Git diff",
            Self::Unpushed => "Unpushed commits",
        }
    }

    fn comparison(self) -> GitComparison {
        match self {
            Self::Git => GitComparison::WorkingTree,
            Self::Unpushed => GitComparison::Unpushed,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigationGroupKind {
    Status,
    Folder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NavigationRow {
    Group {
        path: PathBuf,
        label: String,
        depth: usize,
        kind: NavigationGroupKind,
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

#[derive(Clone, Debug, Default)]
struct FileSearchIndex {
    file_names: Vec<String>,
    relative_paths: Vec<String>,
}

impl FileSearchIndex {
    fn from_files(files: &[CurrentFile]) -> Self {
        Self {
            file_names: files
                .iter()
                .map(|file| {
                    file.relative
                        .file_name()
                        .map_or_else(
                            || file.relative.to_string_lossy().into_owned(),
                            |name| name.to_string_lossy().into_owned(),
                        )
                        .to_lowercase()
                })
                .collect(),
            relative_paths: files
                .iter()
                .map(|file| file.relative.to_string_lossy().to_lowercase())
                .collect(),
        }
    }

    fn matches(&self, index: usize, needle: &str) -> bool {
        self.file_names
            .get(index)
            .is_some_and(|file_name| file_name.contains(needle))
            || self
                .relative_paths
                .get(index)
                .is_some_and(|relative_path| relative_path.contains(needle))
    }
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
    Highlight {
        generation: u64,
        index: usize,
        file: CurrentFile,
    },
    GitScan {
        generation: u64,
        comparison: GitComparison,
    },
    GitDiff {
        generation: u64,
        index: usize,
        change: GitChange,
    },
}

struct ScanResult {
    generation: u64,
    files: Vec<CurrentFile>,
    notices: Vec<String>,
    error: Option<String>,
}

enum WorkResult {
    Scan(Box<ScanResult>),
    SourcePreview {
        generation: u64,
        index: usize,
        lines: Vec<Vec<ColoredSpan>>,
    },
    HighlightChunk {
        generation: u64,
        index: usize,
        start: usize,
        lines: Vec<Vec<ColoredSpan>>,
        complete: bool,
        notice: Option<String>,
    },
    GitScan {
        generation: u64,
        comparison: GitComparison,
        changes: Vec<GitChange>,
        unpushed_commits: Option<usize>,
        error: Option<String>,
    },
    GitDiff {
        generation: u64,
        index: usize,
        lines: Vec<DiffLine>,
    },
}

#[derive(Clone, Debug)]
struct GitScanCache {
    changes: Vec<GitChange>,
    unpushed_commits: Option<usize>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct DiffRenderMetrics {
    rendered_len: usize,
    width: usize,
    line_number_width: usize,
    additions: usize,
    deletions: usize,
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

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries
            .iter_mut()
            .find(|entry| &entry.0 == key)
            .map(|entry| &mut entry.1)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

pub fn run(root: &Path, target_pane_id: String, herdr: &impl Herdr) -> Result<()> {
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
    // Keep only a few completed chunks ahead of the UI. This applies backpressure to
    // highlighting large files instead of eagerly retaining the whole styled file.
    let (result_tx, result_rx) = mpsc::sync_channel(4);
    spawn_worker(
        canonical_root.clone(),
        Arc::clone(&newest_generation),
        task_rx,
        result_tx,
    );
    task_tx
        .send(Task::Scan(1))
        .map_err(|_| Error::Message("background worker stopped".into()))?;

    let mut terminal = TerminalSession::start()?;
    let mut app = App::new(target_pane_id);
    if let Some(notice) = watcher_notice {
        app.notices.push(notice);
    }
    let mut dirty_since: Option<Instant> = None;
    let mut last_liveness = Instant::now();

    loop {
        while let Ok(result) = result_rx.try_recv() {
            app.apply_result(result);
            app.request_selected(&task_tx);
        }
        app.request_selected(&task_tx);
        while let Ok(event) = watch_rx.try_recv() {
            match event {
                Ok(event)
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    dirty_since = Some(Instant::now());
                }
                Ok(_) => {}
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
    sidebar_visible: bool,
    focus: Focus,
    changes_state: TabState,
    files_state: TabState,
    target_pane_id: String,
    git_changes: Vec<GitChange>,
    files: Vec<CurrentFile>,
    file_search_index: FileSearchIndex,
    notices: Vec<String>,
    loading: bool,
    initial_scan_pending: bool,
    mode: Mode,
    generation: u64,
    render_generation: u64,
    requested: BTreeSet<(Tab, usize, u64)>,
    git_diff_cache: Cache<usize, Vec<DiffLine>>,
    diff_metrics_cache: Cache<usize, DiffRenderMetrics>,
    git_scan_cache: [Option<GitScanCache>; 2],
    unpushed_commits: Option<usize>,
    source_cache: Cache<usize, Vec<Vec<ColoredSpan>>>,
    source_width_cache: Cache<usize, usize>,
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
    fn new(target_pane_id: String) -> Self {
        Self {
            tab: Tab::Changes,
            changes_mode: ChangesMode::Git,
            sidebar_visible: true,
            focus: Focus::Navigation,
            changes_state: TabState::default(),
            files_state: TabState::default(),
            target_pane_id,
            git_changes: Vec::new(),
            files: Vec::new(),
            file_search_index: FileSearchIndex::default(),
            notices: Vec::new(),
            loading: true,
            initial_scan_pending: true,
            mode: Mode::Normal,
            generation: 1,
            render_generation: 0,
            requested: BTreeSet::new(),
            git_diff_cache: Cache::new(),
            diff_metrics_cache: Cache::new(),
            git_scan_cache: [None, None],
            unpushed_commits: None,
            source_cache: Cache::new(),
            source_width_cache: Cache::new(),
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

    #[allow(clippy::too_many_lines)]
    fn apply_result(&mut self, result: WorkResult) {
        match result {
            WorkResult::Scan(result) if result.generation == self.generation => {
                self.apply_scan_result(*result);
            }
            WorkResult::SourcePreview {
                generation,
                index,
                lines,
            } if generation == self.render_generation => {
                self.apply_source_preview(index, lines);
            }
            WorkResult::HighlightChunk {
                generation,
                index,
                start,
                lines,
                complete,
                notice,
            } if generation == self.render_generation => {
                self.apply_highlight_chunk(index, start, lines, notice, complete, generation);
            }
            WorkResult::GitScan {
                generation,
                comparison,
                changes,
                unpushed_commits,
                error,
            } if generation == self.render_generation => {
                self.apply_git_scan(comparison, changes, unpushed_commits, error);
            }
            WorkResult::GitDiff {
                generation,
                index,
                lines,
            } if generation == self.render_generation => {
                self.apply_git_diff(index, generation, lines);
            }
            _ => {}
        }
    }

    fn apply_scan_result(&mut self, result: ScanResult) {
        let ScanResult {
            files,
            notices,
            error,
            ..
        } = result;
        let initial_scan = self.initial_scan_pending && self.render_generation <= 1;
        self.initial_scan_pending = false;
        if !initial_scan {
            self.render_generation = self.render_generation.saturating_add(1);
            self.requested.clear();
        }
        if error.is_none() {
            self.files = files;
            self.file_search_index = FileSearchIndex::from_files(&self.files);
            self.source_cache.clear();
            self.source_notices.clear();
            self.source_width_cache.clear();
            if !initial_scan {
                self.git_changes.clear();
                self.git_diff_cache.clear();
                self.diff_metrics_cache.clear();
                self.git_scan_cache = [None, None];
                self.unpushed_commits = None;
                self.git_error = None;
                self.git_state = GitState::Unloaded;
            }
            self.clamp_selections();
        }
        self.scan_error = error;
        self.notices = notices;
        self.loading = false;
    }

    fn apply_source_preview(&mut self, index: usize, lines: Vec<Vec<ColoredSpan>>) {
        self.cache_source(index, lines);
    }

    fn apply_highlight_chunk(
        &mut self,
        index: usize,
        start: usize,
        lines: Vec<Vec<ColoredSpan>>,
        notice: Option<String>,
        complete: bool,
        generation: u64,
    ) {
        if let Some(source) = self.source_cache.get_mut(&index) {
            for (offset, line) in lines.into_iter().enumerate() {
                if let Some(destination) = source.get_mut(start.saturating_add(offset)) {
                    *destination = line;
                }
            }
        }
        if let Some(notice) = notice {
            let bytes = notice.len();
            self.source_notices.insert(index, notice, bytes);
        }
        if complete {
            self.requested.remove(&(Tab::Files, index, generation));
        }
    }

    fn cache_source(&mut self, index: usize, lines: Vec<Vec<ColoredSpan>>) {
        let width = source_content_dimensions(&lines).1;
        let bytes = lines.iter().flatten().map(|span| span.text.len()).sum();
        self.source_cache.insert(index, lines, bytes);
        self.source_width_cache
            .insert(index, width, std::mem::size_of::<usize>());
    }

    fn apply_git_scan(
        &mut self,
        comparison: GitComparison,
        changes: Vec<GitChange>,
        unpushed_commits: Option<usize>,
        error: Option<String>,
    ) {
        self.git_state = GitState::Loaded;
        self.requested.clear();
        self.git_scan_cache[git_comparison_index(comparison)] = Some(GitScanCache {
            changes: changes.clone(),
            unpushed_commits,
            error: error.clone(),
        });
        if error.is_none() {
            self.git_changes = changes;
            self.git_diff_cache.clear();
            self.diff_metrics_cache.clear();
            self.clamp_selections();
        }
        self.unpushed_commits = unpushed_commits;
        self.git_error = error;
    }

    fn apply_git_diff(&mut self, index: usize, generation: u64, lines: Vec<DiffLine>) {
        if let Some(change) = self.git_changes.get(index) {
            let metrics = diff_render_metrics(
                change.kind,
                &change.path,
                change.old_path.as_deref(),
                Some(&lines),
            );
            self.diff_metrics_cache.insert(
                index,
                metrics,
                std::mem::size_of::<DiffRenderMetrics>(),
            );
        }
        let bytes = lines.iter().map(|line| line.text.len()).sum();
        self.git_diff_cache.insert(index, lines, bytes);
        self.requested.remove(&(Tab::Changes, index, generation));
    }

    fn refresh(&mut self, newest: &AtomicU64, tasks: &mpsc::SyncSender<Task>) -> bool {
        if self.loading {
            return false;
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
            self.diff_metrics_cache.clear();
            self.git_scan_cache = [None, None];
            self.unpushed_commits = None;
            self.git_error = None;
            self.git_state = GitState::Unloaded;
            true
        } else {
            false
        }
    }

    fn request_selected(&mut self, tasks: &mpsc::SyncSender<Task>) {
        if self.tab == Tab::Changes && self.git_state != GitState::Loaded {
            if self.git_state == GitState::Loading {
                return;
            }
            self.render_generation = self.render_generation.saturating_add(1);
            let generation = self.render_generation;
            self.requested.clear();
            if tasks
                .try_send(Task::GitScan {
                    generation,
                    comparison: self.changes_mode.comparison(),
                })
                .is_ok()
            {
                self.git_state = GitState::Loading;
            }
            return;
        }
        if self.loading {
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
            ChangesMode::Git => ChangesMode::Unpushed,
            ChangesMode::Unpushed => ChangesMode::Git,
        };
        self.changes_state.selected = 0;
        self.changes_state.scroll_y = 0;
        self.changes_state.scroll_x = 0;
        self.render_generation = self.render_generation.saturating_add(1);
        self.requested.clear();
        self.git_changes.clear();
        self.git_diff_cache.clear();
        self.diff_metrics_cache.clear();
        self.unpushed_commits = None;
        self.git_error = None;
        if let Some(cache) =
            self.git_scan_cache[git_comparison_index(self.changes_mode.comparison())].as_ref()
        {
            self.git_changes = cache.changes.clone();
            self.unpushed_commits = cache.unpushed_commits;
            self.git_error = cache.error.clone();
            self.git_state = GitState::Loaded;
        } else {
            self.git_state = GitState::Unloaded;
        }
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.scrollbar_drag = None;
        if !self.sidebar_visible {
            self.focus = Focus::Content;
        }
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        newest: &AtomicU64,
        tasks: &mpsc::SyncSender<Task>,
    ) -> bool {
        // Terminals using the enhanced keyboard protocol can report key releases in
        // addition to presses. Treating both as input makes navigation and toggles
        // fire twice on terminals that enable those reports (commonly on Linux).
        if key.kind == crossterm::event::KeyEventKind::Release {
            return false;
        }
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
                    && key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META,
                    ) =>
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
            KeyCode::Char('b') => self.toggle_sidebar(),
            KeyCode::Tab => {
                if self.sidebar_visible {
                    self.focus = match self.focus {
                        Focus::Navigation => Focus::Content,
                        Focus::Content => Focus::Navigation,
                    };
                } else {
                    self.focus = Focus::Content;
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('r') => {
                self.refresh(newest, tasks);
            }
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
        let change_count =
            filtered_git_indices(&self.git_changes, &self.changes_state.filter).len();
        let file_count = self.filtered_file_indices().len();
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
            Tab::Changes => filtered_git_indices(&self.git_changes, &self.changes_state.filter),
            Tab::Files => self.filtered_file_indices(),
        }
    }

    fn filtered_file_indices(&self) -> Vec<usize> {
        let needle = self.files_state.filter.to_lowercase();
        self.files
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                needle.is_empty() || self.file_search_index.matches(*index, &needle)
            })
            .map(|(index, _)| index)
            .collect()
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
        } else if !self.sidebar_visible {
            (None, Some(vertical[1]))
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
        if self.tab == Tab::Changes {
            return self.git_navigation_rows(indices);
        }

        let mut rows = Vec::with_capacity(indices.len().saturating_mul(2));
        let mut current_group = None;
        for index in indices {
            let path = self.files[index].relative.as_path();
            let group = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf);
            if group != current_group {
                if let Some(group) = &group {
                    rows.push(NavigationRow::Group {
                        path: group.clone(),
                        label: format!("{}/", group.display()),
                        depth: 0,
                        kind: NavigationGroupKind::Folder,
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

    fn git_navigation_rows(&self, indices: Vec<usize>) -> Vec<NavigationRow> {
        let mut rows = Vec::with_capacity(indices.len().saturating_mul(3));
        let mut current_state = None;
        let mut current_group = None;
        let mut state_collapsed = false;
        let mut group_collapsed = false;

        for index in indices {
            let change = &self.git_changes[index];
            if current_state != Some(change.state) {
                let path = git_state_group_path(change.state);
                rows.push(NavigationRow::Group {
                    path: path.clone(),
                    label: format!("{}/", change.state.label()),
                    depth: 0,
                    kind: NavigationGroupKind::Status,
                });
                current_state = Some(change.state);
                current_group = None;
                state_collapsed = self.group_collapsed(&path);
                group_collapsed = false;
            }
            if state_collapsed {
                continue;
            }

            let group = change
                .path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf);
            if group != current_group {
                current_group.clone_from(&group);
                if let Some(group) = &group {
                    let path = git_folder_group_path(change.state, group);
                    rows.push(NavigationRow::Group {
                        path: path.clone(),
                        label: format!("{}/", group.display()),
                        depth: 1,
                        kind: NavigationGroupKind::Folder,
                    });
                    group_collapsed = self.group_collapsed(&path);
                } else {
                    group_collapsed = false;
                }
            }
            if group_collapsed {
                continue;
            }

            let label = change.path.file_name().map_or_else(
                || change.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            rows.push(NavigationRow::File {
                index,
                label,
                depth: usize::from(group.is_some()).saturating_add(1),
            });
        }
        rows
    }

    fn navigation_stats(&self, index: usize) -> Option<(usize, usize)> {
        if self.tab != Tab::Changes {
            return None;
        }
        self.git_diff_cache
            .get(&index)
            .map(|lines| diff_stats(lines))
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
                self.diff_metrics_cache.get(&selected).map_or_else(
                    || diff_content_dimensions(kind, path, old_path, lines),
                    |metrics| (metrics.rendered_len, metrics.width),
                )
            }
            Tab::Files => {
                if self.source_notices.get(&selected).is_some() {
                    return None;
                }
                let lines = self.source_cache.get(&selected)?;
                (
                    lines.len(),
                    self.source_width_cache
                        .get(&selected)
                        .copied()
                        .unwrap_or_else(|| source_content_dimensions(lines).1),
                )
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
            "b: sidebar  g: Git diff / Unpushed  click folders  drag scrollbars  Tab focus  / filter  ? help  q close".into()
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
        let content_width = self
            .source_width_cache
            .get(&selected)
            .copied()
            .unwrap_or_else(|| source_content_dimensions(lines).1);
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
            NavigationRow::Group {
                path,
                label,
                depth,
                kind,
            } => {
                let marker = if self.group_collapsed(path) {
                    "▸ "
                } else {
                    "▾ "
                };
                let color = match kind {
                    NavigationGroupKind::Status => Color::Yellow,
                    NavigationGroupKind::Folder => Color::Cyan,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(gutter, gutter_style),
                    Span::styled(
                        format!("{}{marker}", "  ".repeat(*depth)),
                        Style::default().fg(color),
                    ),
                    Span::styled(label.clone(), Style::default().fg(color).bold()),
                ]))
            }
            NavigationRow::File {
                index,
                label,
                depth,
            } => {
                let (symbol, color) = if self.tab == Tab::Changes {
                    let kind = self.git_changes[*index].kind;
                    change_symbol(kind)
                } else {
                    ("·", Color::DarkGray)
                };
                let stats = self.navigation_stats(*index);
                let stats_width = stats.map_or(0, |(additions, deletions)| {
                    format!("+{additions} -{deletions}").width()
                });
                let prefix = if self.tab == Tab::Changes {
                    format!("{gutter}{}{} ", "  ".repeat(*depth), symbol)
                } else {
                    format!("{gutter}{}", "  ".repeat(*depth))
                };
                let icon =
                    (self.tab == Tab::Files).then(|| file_icon(&self.files[*index].relative));
                let icon_width = icon.map_or(0, |_| FILE_ICON_SLOT_WIDTH);
                let prefix_width = prefix.width().saturating_add(icon_width);
                let name_width = row_width
                    .saturating_sub(prefix_width)
                    .saturating_sub(stats_width)
                    .saturating_sub(1);
                let label = truncate_label(label, name_width);
                let padding = row_width
                    .saturating_sub(prefix_width)
                    .saturating_sub(label.width())
                    .saturating_sub(stats_width)
                    .max(1);
                let mut spans = vec![Span::styled(prefix, Style::default().fg(color).bold())];
                if let Some(icon) = icon {
                    spans.push(Span::styled(
                        icon.glyph,
                        Style::default().fg(icon.color).bold(),
                    ));
                    spans.push(Span::raw(
                        " ".repeat(FILE_ICON_SLOT_WIDTH.saturating_sub(icon.glyph.width())),
                    ));
                }
                spans.extend([Span::raw(label), Span::raw(" ".repeat(padding))]);
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
                Tab::Files => format!(" Files · {} files ", self.filtered_indices().len()),
            }
        } else {
            format!(
                " /{} · {} files ",
                self.active_state().filter,
                self.filtered_indices().len()
            )
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
                    let path = &self.git_changes[selected].path;
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
            } else if self.tab == Tab::Changes {
                if let Some(error) = &self.git_error {
                    format!("{} unavailable.\n\n{error}", self.changes_mode.label())
                } else if self.git_state != GitState::Loaded {
                    format!("Reading {}…", self.changes_mode.label())
                } else {
                    match self.changes_mode {
                        ChangesMode::Git => self.empty_git_message(),
                        ChangesMode::Unpushed => "No unpushed commits.".into(),
                    }
                }
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

    fn empty_git_message(&self) -> String {
        match self.unpushed_commits {
            Some(count) if count > 0 => {
                let commits = if count == 1 { "commit" } else { "commits" };
                format!(
                    "Changes are in Unpushed commits.\n\n{count} unpushed {commits}.\n\nPress g to view them."
                )
            }
            _ => "No local Git changes.\n\nPress g to check Unpushed commits.".into(),
        }
    }

    fn selected_diff(&self, selected: usize) -> Option<DiffView<'_>> {
        let change = self.git_changes.get(selected)?;
        Some((
            change.kind,
            &change.path,
            change.old_path.as_deref(),
            self.git_diff_cache.get(&selected).map(Vec::as_slice),
        ))
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
        let metrics = self
            .diff_metrics_cache
            .get(&selected)
            .copied()
            .unwrap_or_else(|| diff_render_metrics(kind, path, old_path, lines));
        let diff_width = metrics.width.max(usize::from(content_area.width));
        let header_style = Style::default().bg(DIFF_HEADER_BACKGROUND);
        let mut headers = vec![Line::from(vec![
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
            headers.push(Line::from(vec![
                Span::styled("  from ", header_style.fg(Color::DarkGray)),
                Span::styled(
                    old_path.display().to_string(),
                    header_style.fg(Color::DarkGray),
                ),
            ]));
        }

        headers[0].spans.push(Span::styled(
            format!("    +{} -{}", metrics.additions, metrics.deletions),
            header_style.fg(Color::DarkGray),
        ));
        headers.push(Line::styled("", Style::default().bg(DIFF_BACKGROUND)));

        let line_number_width = metrics.line_number_width;
        let rendered_len = metrics.rendered_len;
        let max_scroll_y = rendered_len.saturating_sub(usize::from(content_area.height));
        let max_scroll_x = diff_width.saturating_sub(usize::from(content_area.width));
        let scroll_y = self.changes_state.scroll_y.min(max_scroll_y);
        let scroll_x = self.changes_state.scroll_x.min(max_scroll_x);
        let visible_end = scroll_y
            .saturating_add(usize::from(content_area.height))
            .min(rendered_len);
        let mut rendered = Vec::with_capacity(visible_end.saturating_sub(scroll_y));
        for row in scroll_y..visible_end {
            if let Some(header) = headers.get(row) {
                rendered.push(header.clone());
            } else {
                let diff_index = row.saturating_sub(headers.len());
                rendered.extend(render_diff_rows(
                    lines,
                    line_number_width,
                    diff_width,
                    diff_index,
                    diff_index.saturating_add(1),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(rendered)
                .style(Style::default().fg(DIFF_TEXT).bg(DIFF_BACKGROUND))
                .scroll((0, saturating_u16(scroll_x))),
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
            let mut scrollbar = ScrollbarState::new(diff_width)
                .position(scrollbar_render_position(
                    scroll_x,
                    max_scroll_x,
                    diff_width,
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
            frame.render_widget(Paragraph::new("").style(panel_style()), area);
            return;
        };
        let [content_area, horizontal_scrollbar_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let line_count = lines.len();
        let width = line_count.max(1).ilog10() as usize + 1;
        let content_width = self
            .source_width_cache
            .get(&selected)
            .copied()
            .unwrap_or_else(|| source_content_dimensions(lines).1);
        let max_scroll_y = line_count.saturating_sub(usize::from(content_area.height));
        let max_scroll_x = content_width.saturating_sub(usize::from(content_area.width));
        let scroll_y = self.files_state.scroll_y.min(max_scroll_y);
        let scroll_x = self.files_state.scroll_x.min(max_scroll_x);
        let visible_end = scroll_y
            .saturating_add(usize::from(content_area.height))
            .min(line_count);
        let rendered: Vec<Line<'_>> = lines[scroll_y..visible_end]
            .iter()
            .enumerate()
            .map(|(offset, spans)| {
                let index = scroll_y.saturating_add(offset);
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
        frame.render_widget(
            Paragraph::new(rendered)
                .style(panel_style())
                .scroll((0, saturating_u16(scroll_x))),
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
                "Git Changes\n\n\
                 1 / 2       Changes review / Files browser\n\
                 b           Show / hide the sidebar\n\
                 g           Switch to Git diff / Unpushed commits in Changes\n\
                 Tab         navigation / diff or source focus\n\
                 Mouse       click folders to expand/collapse, tabs/items/content, drag scrollbars, wheel scroll, drag select\n\
                 ↑↓ or jk    select files / scroll content\n\
                 ←→ or hl    horizontal scroll\n\
                 /           filter active list\n\
                 r           full refresh\n\
                 Ctrl+C / ⌘C copy selected text; mouse selections copy on release\n\
                 ?           close help\n\
                 q           close viewer\n\n\
                 Read only. Git diff shows local changes; Unpushed shows commits ahead of @{upstream}.\n\
                 Git diff groups files by staged, unstaged, mixed, or untracked status; empty groups are hidden.\n\
                 Results show changes that are not yet part of the pushed branch.",
            )
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn spawn_worker(
    root: PathBuf,
    newest_generation: Arc<AtomicU64>,
    tasks: mpsc::Receiver<Task>,
    results: mpsc::SyncSender<WorkResult>,
) {
    thread::spawn(move || {
        let syntaxes = Arc::new(OnceLock::<SyntaxSet>::new());
        let theme = Arc::new(OnceLock::<Theme>::new());
        while let Ok(task) = tasks.recv() {
            match task {
                Task::Scan(generation) => {
                    let root = root.clone();
                    let newest_generation = Arc::clone(&newest_generation);
                    let results = results.clone();
                    thread::spawn(move || {
                        if newest_generation.load(Ordering::Acquire) != generation {
                            return;
                        }
                        let result = match scan(&root) {
                            Ok((map, notices)) => WorkResult::Scan(Box::new(ScanResult {
                                generation,
                                files: map.into_values().collect(),
                                notices,
                                error: None,
                            })),
                            Err(error) => WorkResult::Scan(Box::new(ScanResult {
                                generation,
                                files: Vec::new(),
                                notices: Vec::new(),
                                error: Some(error.to_string()),
                            })),
                        };
                        if newest_generation.load(Ordering::Acquire) != generation {
                            return;
                        }
                        let _ = results.send(result);
                    });
                }
                Task::Highlight {
                    generation,
                    index,
                    file,
                } => {
                    spawn_highlight_worker(
                        root.clone(),
                        results.clone(),
                        Arc::clone(&syntaxes),
                        Arc::clone(&theme),
                        generation,
                        index,
                        file,
                    );
                }
                Task::GitScan {
                    generation,
                    comparison,
                } => {
                    let root = root.clone();
                    let results = results.clone();
                    thread::spawn(move || {
                        let _ = results.send(git_scan_result(&root, generation, comparison));
                    });
                }
                Task::GitDiff {
                    generation,
                    index,
                    change,
                } => {
                    let root = root.clone();
                    let results = results.clone();
                    thread::spawn(move || {
                        let _ = results.send(git_diff_result(&root, generation, index, &change));
                    });
                }
            }
        }
    });
}

fn spawn_highlight_worker(
    root: PathBuf,
    results: mpsc::SyncSender<WorkResult>,
    syntaxes: Arc<OnceLock<SyntaxSet>>,
    theme: Arc<OnceLock<Theme>>,
    generation: u64,
    index: usize,
    file: CurrentFile,
) {
    thread::spawn(move || {
        let syntaxes = syntaxes.get_or_init(SyntaxSet::load_defaults_newlines);
        let theme =
            theme.get_or_init(|| ThemeSet::load_defaults().themes["base16-ocean.dark"].clone());
        match read_source(&root, &file) {
            Ok(text) => {
                let _ = results.send(WorkResult::SourcePreview {
                    generation,
                    index,
                    lines: plain_source(&text),
                });
                highlight_source_chunks(
                    &root,
                    &file.relative,
                    &text,
                    syntaxes,
                    theme,
                    |start, lines, complete| {
                        results
                            .send(WorkResult::HighlightChunk {
                                generation,
                                index,
                                start,
                                lines,
                                complete,
                                notice: None,
                            })
                            .is_ok()
                    },
                );
            }
            Err(error) => {
                let _ = results.send(WorkResult::HighlightChunk {
                    generation,
                    index,
                    start: 0,
                    lines: Vec::new(),
                    complete: true,
                    notice: Some(error),
                });
            }
        }
    });
}

fn git_scan_result(root: &Path, generation: u64, comparison: GitComparison) -> WorkResult {
    match scan_git(root, comparison) {
        Ok(changes) => WorkResult::GitScan {
            generation,
            comparison,
            changes,
            unpushed_commits: (comparison == GitComparison::WorkingTree)
                .then(|| unpushed_commit_count(root))
                .flatten(),
            error: None,
        },
        Err(error) => WorkResult::GitScan {
            generation,
            comparison,
            changes: Vec::new(),
            unpushed_commits: None,
            error: Some(error),
        },
    }
}

fn git_comparison_index(comparison: GitComparison) -> usize {
    match comparison {
        GitComparison::WorkingTree => 0,
        GitComparison::Unpushed => 1,
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

#[cfg(test)]
fn highlight_file(
    root: &Path,
    file: &CurrentFile,
    syntaxes: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
) -> (Vec<Vec<ColoredSpan>>, Option<String>) {
    let text = match read_source(root, file) {
        Ok(text) => text,
        Err(error) => return (Vec::new(), Some(error)),
    };
    (
        highlight_source(root, &file.relative, &text, syntaxes, theme),
        None,
    )
}

fn read_source(root: &Path, file: &CurrentFile) -> std::result::Result<String, String> {
    if file.text != TextEligibility::Text {
        return Err(format!(
            "{}\n\nMetadata-only file: {:?}, {} bytes",
            file.relative.display(),
            file.text,
            file.size
        ));
    }
    let bytes = match safe_read(root, &file.relative, crate::model::INLINE_TEXT_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) => return Err(error.to_string()),
    };
    String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".into())
}

fn plain_source(text: &str) -> Vec<Vec<ColoredSpan>> {
    LinesWithEndings::from(text)
        .map(|line| {
            vec![ColoredSpan {
                text: line.trim_end_matches(['\r', '\n']).to_owned(),
                foreground: Color::White,
            }]
        })
        .collect()
}

#[cfg(test)]
fn highlight_source(
    root: &Path,
    relative: &Path,
    text: &str,
    syntaxes: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
) -> Vec<Vec<ColoredSpan>> {
    let mut highlighted = Vec::new();
    highlight_source_chunks(root, relative, text, syntaxes, theme, |_, mut lines, _| {
        highlighted.append(&mut lines);
        true
    });
    highlighted
}

fn highlight_source_chunks(
    root: &Path,
    relative: &Path,
    text: &str,
    syntaxes: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
    mut emit: impl FnMut(usize, Vec<Vec<ColoredSpan>>, bool) -> bool,
) {
    let syntax = syntax_for_file(root, relative, syntaxes);
    let default_foreground = theme.settings.foreground;
    let true_color = std::env::var("COLORTERM")
        .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"));
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut chunk = Vec::with_capacity(HIGHLIGHT_CHUNK_LINES);
    let mut start = 0;
    let mut lines = LinesWithEndings::from(text).peekable();
    while let Some(line) = lines.next() {
        chunk.push(
            highlighter
                .highlight_line(line, syntaxes)
                .unwrap_or_default()
                .into_iter()
                .map(|(style, text)| ColoredSpan {
                    text: text.trim_end_matches(['\r', '\n']).to_owned(),
                    foreground: if default_foreground == Some(style.foreground) {
                        Color::White
                    } else {
                        terminal_color(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                            true_color,
                        )
                    },
                })
                .collect(),
        );
        if chunk.len() == HIGHLIGHT_CHUNK_LINES || lines.peek().is_none() {
            let complete = lines.peek().is_none();
            let count = chunk.len();
            if !emit(start, std::mem::take(&mut chunk), complete) {
                return;
            }
            start = start.saturating_add(count);
        }
    }
    if start == 0 {
        let _ = emit(0, Vec::new(), true);
    }
}

fn syntax_for_file<'a>(
    root: &Path,
    relative: &Path,
    syntaxes: &'a SyntaxSet,
) -> &'a SyntaxReference {
    let syntax = match relative
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension)
            if extension.eq_ignore_ascii_case("jsx")
                || extension.eq_ignore_ascii_case("tsx")
                || extension.eq_ignore_ascii_case("ts") =>
        {
            syntaxes.find_syntax_by_token("JavaScript")
        }
        _ => syntaxes
            .find_syntax_for_file(root.join(relative))
            .ok()
            .flatten(),
    };
    syntax.unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

fn filtered_git_indices(changes: &[GitChange], filter: &str) -> Vec<usize> {
    let needle = filter.to_ascii_lowercase();
    let mut indices: Vec<usize> = changes
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
        .collect();
    indices.sort_by_key(|index| git_state_rank(changes[*index].state));
    indices
}

fn git_state_rank(state: GitFileState) -> u8 {
    match state {
        GitFileState::Staged => 0,
        GitFileState::Unstaged => 1,
        GitFileState::StagedAndUnstaged => 2,
        GitFileState::Untracked => 3,
        GitFileState::Committed => 4,
    }
}

fn git_state_group_path(state: GitFileState) -> PathBuf {
    PathBuf::from(".herdr-git-status").join(state.label())
}

fn git_folder_group_path(state: GitFileState, folder: &Path) -> PathBuf {
    git_state_group_path(state).join("folders").join(folder)
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn copy_with_command(program: &str, arguments: &[&str], text: &str) -> io::Result<Option<()>> {
    let mut child = match Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("clipboard stdin is unavailable"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    let status = child.wait()?;
    if status.success() {
        Ok(Some(()))
    } else {
        Err(io::Error::other(format!("{program} exited unsuccessfully")))
    }
}

#[cfg(target_os = "macos")]
fn copy_to_clipboard(text: &str) -> io::Result<()> {
    copy_with_command("pbcopy", &[], text)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "pbcopy is unavailable on this macOS system",
        )
    })
}

#[cfg(target_os = "linux")]
fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut candidates = if wayland {
        vec![
            ("wl-copy", Vec::new()),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ]
    } else {
        vec![
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
            ("wl-copy", Vec::new()),
        ]
    };
    let mut last_error = None;
    for (program, arguments) in candidates.drain(..) {
        match copy_with_command(program, &arguments, text) {
            Ok(Some(())) => return Ok(()),
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
    }

    if let Ok(()) = copy_via_osc52(text) {
        return Ok(());
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no Linux clipboard provider found (install wl-clipboard, xclip, or xsel)",
        )
    }))
}

#[cfg(target_os = "linux")]
fn copy_via_osc52(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

#[cfg(target_os = "linux")]
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(first >> 2)] as char);
        encoded.push(TABLE[usize::from(((first & 0b11) << 4) | (second >> 4))] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from(((second & 0b1111) << 2) | (third >> 6))] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(third & 0b11_1111)] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn copy_to_clipboard(_text: &str) -> io::Result<()> {
    Err(io::Error::other(
        "clipboard copying is unsupported on this platform",
    ))
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

fn file_icon(path: &Path) -> FileIcon {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(name, "Dockerfile" | "Containerfile") {
        return file_icon_badge("DO", Color::Rgb(36, 150, 237));
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    extension
        .as_deref()
        .and_then(file_icon_for_extension)
        .unwrap_or_else(|| file_icon_badge("··", Color::DarkGray))
}

fn file_icon_badge(glyph: &'static str, color: Color) -> FileIcon {
    FileIcon { glyph, color }
}

fn file_icon_for_extension(extension: &str) -> Option<FileIcon> {
    let icon = match extension {
        "rs" => file_icon_badge("RS", Color::Rgb(222, 165, 132)),
        "ts" => file_icon_badge("TS", Color::Rgb(49, 120, 198)),
        "tsx" => file_icon_badge("⚛", Color::Rgb(97, 218, 251)),
        "js" | "jsx" | "mjs" | "cjs" => file_icon_badge("JS", Color::Rgb(247, 223, 30)),
        "py" | "pyw" => file_icon_badge("PY", Color::Rgb(55, 118, 171)),
        "go" => file_icon_badge("GO", Color::Rgb(0, 173, 216)),
        "rb" => file_icon_badge("RB", Color::Rgb(204, 52, 45)),
        "php" => file_icon_badge("PH", Color::Rgb(119, 123, 180)),
        "java" => file_icon_badge("JV", Color::Rgb(248, 152, 32)),
        "kt" | "kts" => file_icon_badge("KT", Color::Rgb(127, 82, 255)),
        "swift" => file_icon_badge("SW", Color::Rgb(240, 81, 56)),
        "c" => file_icon_badge("C", Color::Rgb(85, 132, 181)),
        "h" | "hpp" | "hh" => file_icon_badge("C#", Color::Rgb(101, 155, 211)),
        "cc" | "cpp" | "cxx" => file_icon_badge("C+", Color::Rgb(0, 89, 156)),
        "cs" => file_icon_badge("C#", Color::Rgb(104, 33, 122)),
        "html" | "htm" => file_icon_badge("<>", Color::Rgb(227, 76, 38)),
        "css" | "scss" | "sass" | "less" => file_icon_badge("#.", Color::Rgb(38, 77, 228)),
        "json" | "jsonc" | "json5" => file_icon_badge("{}", Color::Rgb(240, 180, 41)),
        "toml" => file_icon_badge("TM", Color::Rgb(156, 39, 176)),
        "yaml" | "yml" => file_icon_badge("YM", Color::Rgb(203, 56, 55)),
        "md" | "mdx" => file_icon_badge("MD", Color::Rgb(83, 174, 85)),
        "sh" | "bash" | "zsh" | "fish" | "ps1" => file_icon_badge("SH", Color::Rgb(137, 180, 72)),
        "sql" => file_icon_badge("DB", Color::Rgb(0, 150, 136)),
        _ => return None,
    };
    Some(icon)
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
    let metrics = diff_render_metrics(kind, path, old_path, lines);
    (metrics.rendered_len, metrics.width)
}

fn diff_render_metrics(
    kind: ChangeKind,
    path: &Path,
    old_path: Option<&Path>,
    lines: Option<&[DiffLine]>,
) -> DiffRenderMetrics {
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
    let diff_width = lines.map_or_else(
        || "Generating diff…".width(),
        |lines| {
            lines
                .iter()
                .map(|line| diff_line_width(line, line_number_width))
                .max()
                .unwrap_or(0)
        },
    );
    width = width.max(diff_width);

    let rendered_len = 2_usize
        .saturating_add(usize::from(old_path.is_some()))
        .saturating_add(lines.map_or(1, <[DiffLine]>::len));
    DiffRenderMetrics {
        rendered_len,
        width,
        line_number_width,
        additions,
        deletions,
    }
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
    start: usize,
    end: usize,
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
                .get(start..end.min(lines.len()))
                .unwrap_or_default()
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

fn terminal_color(red: u8, green: u8, blue: u8, true_color: bool) -> Color {
    let red = brighten_code_component(red);
    let green = brighten_code_component(green);
    let blue = brighten_code_component(blue);
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
    use std::path::Path;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use tempfile::TempDir;
    use unicode_width::UnicodeWidthStr;

    #[cfg(target_os = "linux")]
    use super::base64_encode;
    use super::{
        App, Cache, ChangesMode, FileSearchIndex, Focus, Tab, Task, WorkResult,
        brighten_code_component, file_icon, highlight_file, highlight_source_chunks,
        saturating_u16, syntax_for_file,
    };
    use crate::diff::{DiffLine, DiffLineKind};
    use crate::git::{GitChange, GitComparison, GitFileState};
    use crate::model::{ChangeKind, CurrentFile, TextEligibility};
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
            text: TextEligibility::Text,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn osc52_clipboard_encoding_matches_base64() {
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    fn git_change(path: &str, kind: ChangeKind) -> GitChange {
        GitChange {
            kind,
            path: path.into(),
            old_path: None,
            untracked: false,
            comparison: GitComparison::WorkingTree,
            state: GitFileState::Unstaged,
        }
    }

    #[test]
    fn changes_start_in_git_mode_and_g_toggles_to_unpushed() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        assert_eq!(app.changes_mode, ChangesMode::Git);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &newest,
            &tasks,
        );

        assert_eq!(app.changes_mode, ChangesMode::Unpushed);
        assert_eq!(app.git_state, super::GitState::Loading);
        assert!(matches!(
            queued.try_recv(),
            Ok(Task::GitScan {
                comparison: GitComparison::Unpushed,
                ..
            })
        ));

        app.handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &newest,
            &tasks,
        );

        assert_eq!(app.changes_mode, ChangesMode::Git);
        assert_eq!(app.git_state, super::GitState::Loading);
        assert!(matches!(
            queued.try_recv(),
            Ok(Task::GitScan {
                comparison: GitComparison::WorkingTree,
                ..
            })
        ));
    }

    #[test]
    fn enhanced_keyboard_release_events_do_not_repeat_actions() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);
        let mut release = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        release.kind = crossterm::event::KeyEventKind::Release;

        assert!(!app.handle_key(release, &newest, &tasks));
        assert_eq!(app.changes_mode, ChangesMode::Git);
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn g_reuses_a_loaded_comparison_without_queueing_another_scan() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.git_state = super::GitState::Loaded;
        app.git_scan_cache[1] = Some(super::GitScanCache {
            changes: Vec::new(),
            unpushed_commits: None,
            error: None,
        });
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &newest,
            &tasks,
        );

        assert_eq!(app.changes_mode, ChangesMode::Unpushed);
        assert_eq!(app.git_state, super::GitState::Loaded);
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn empty_git_view_points_to_unpushed_commits() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.git_state = super::GitState::Loaded;
        app.unpushed_commits = Some(1);

        let output = render(&mut app, 100, 20);
        assert!(output.contains("Changes are in Unpushed commits."));
        assert!(output.contains("1 unpushed commit."));
        assert!(output.contains("Press g to view them."));
        assert!(!output.contains("No local Git changes."));
    }

    #[test]
    fn b_toggles_sidebar_for_changes_and_files_tabs() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.viewport = Rect::new(0, 0, 100, 20);
        let (tasks, _) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        for tab in [Tab::Changes, Tab::Files] {
            app.tab = tab;
            app.sidebar_visible = true;
            app.focus = Focus::Navigation;
            assert!(app.ui_layout(app.viewport).navigation.is_some());

            app.handle_key(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                &newest,
                &tasks,
            );

            let hidden_layout = app.ui_layout(app.viewport);
            assert!(!app.sidebar_visible);
            assert_eq!(app.focus, Focus::Content);
            assert!(hidden_layout.navigation.is_none());
            assert_eq!(hidden_layout.content.map(|area| area.width), Some(100));

            app.handle_key(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                &newest,
                &tasks,
            );

            assert!(app.sidebar_visible);
            assert!(app.ui_layout(app.viewport).navigation.is_some());
        }
    }

    #[test]
    fn scan_invalidates_in_flight_render_results() {
        let mut app = App::new("w1:p1".into());
        app.render_generation = 7;
        app.apply_result(WorkResult::Scan(Box::new(super::ScanResult {
            generation: 1,
            files: Vec::new(),
            notices: Vec::new(),
            error: None,
        })));

        assert_eq!(app.render_generation, 8);
    }

    #[test]
    fn refresh_invalidates_render_results_before_the_scan_finishes() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.render_generation = 7;
        let (tasks, _queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.refresh(&newest, &tasks);

        assert_eq!(app.render_generation, 8);
        assert!(app.loading);
        assert!(app.git_diff_cache.get(&0).is_none());
    }

    #[test]
    fn refresh_does_not_queue_a_second_scan_while_one_is_in_flight() {
        let mut app = App::new("w1:p1".into());
        let (tasks, queued) = std::sync::mpsc::sync_channel(8);
        let newest = std::sync::atomic::AtomicU64::new(1);

        app.refresh(&newest, &tasks);

        assert!(queued.try_recv().is_err());
        assert_eq!(app.generation, 1);
        assert!(app.loading);
    }

    #[test]
    fn narrow_layout_switches_between_navigation_and_content() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        let navigation = render(&mut app, 42, 12);
        assert!(navigation.contains("Changes"));
        app.focus = Focus::Content;
        let content = render(&mut app, 42, 12);
        assert!(content.contains("Reading Git diff"));
    }

    #[test]
    fn source_preview_renders_before_highlighting_finishes() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("main.rs")];
        app.render_generation = 3;
        app.apply_result(WorkResult::SourcePreview {
            generation: 3,
            index: 0,
            lines: vec![vec![super::ColoredSpan {
                text: "fn main() {}".into(),
                foreground: Color::White,
            }]],
        });

        let output = render(&mut app, 80, 18);

        assert!(output.contains("fn main() {}"));
        assert!(!output.contains("Highlighting source"));
    }

    #[test]
    fn highlight_chunks_replace_only_their_preview_lines() {
        let mut app = App::new("w1:p1".into());
        app.render_generation = 3;
        app.requested.insert((Tab::Files, 0, 3));
        app.apply_result(WorkResult::SourcePreview {
            generation: 3,
            index: 0,
            lines: vec![
                vec![super::ColoredSpan {
                    text: "first".into(),
                    foreground: Color::White,
                }],
                vec![super::ColoredSpan {
                    text: "second".into(),
                    foreground: Color::White,
                }],
            ],
        });

        app.apply_result(WorkResult::HighlightChunk {
            generation: 3,
            index: 0,
            start: 1,
            lines: vec![vec![super::ColoredSpan {
                text: "second".into(),
                foreground: Color::Cyan,
            }]],
            complete: false,
            notice: None,
        });

        let lines = app.source_cache.get(&0).expect("source preview");
        assert_eq!(lines[0][0].foreground, Color::White);
        assert_eq!(lines[1][0].foreground, Color::Cyan);
        assert!(app.requested.contains(&(Tab::Files, 0, 3)));

        app.apply_result(WorkResult::HighlightChunk {
            generation: 3,
            index: 0,
            start: 0,
            lines: vec![vec![super::ColoredSpan {
                text: "first".into(),
                foreground: Color::Green,
            }]],
            complete: true,
            notice: None,
        });

        assert!(!app.requested.contains(&(Tab::Files, 0, 3)));
        assert_eq!(
            app.source_cache.get(&0).expect("source preview")[0][0].foreground,
            Color::Green
        );
    }

    #[test]
    fn pending_source_does_not_show_a_placeholder() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("main.rs")];

        let output = render(&mut app, 80, 18);

        assert!(!output.contains("Highlighting source"));
    }

    #[test]
    fn changes_navigation_groups_files_and_keeps_file_clicks_mapped() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.git_changes = vec![
            git_change("src/core/types.ts", ChangeKind::Modified),
            git_change("src/core/config.ts", ChangeKind::Added),
            git_change("src/ui/App.tsx", ChangeKind::Modified),
        ];
        app.git_changes[0].state = GitFileState::Staged;
        app.git_changes[1].state = GitFileState::Untracked;
        app.git_changes[2].state = GitFileState::StagedAndUnstaged;
        app.git_diff_cache.insert(
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
            matches!(rows[0], super::NavigationRow::Group { ref label, .. } if label == "staged/")
        );
        assert!(matches!(
            rows[1],
            super::NavigationRow::Group { ref label, .. } if label == "src/core/"
        ));
        assert!(matches!(
            rows[2],
            super::NavigationRow::File { index: 0, .. }
        ));
        assert!(
            matches!(rows[3], super::NavigationRow::Group { ref label, .. } if label == "mixed/")
        );
        assert!(matches!(
            rows[4],
            super::NavigationRow::Group { ref label, .. } if label == "src/ui/"
        ));
        assert!(matches!(
            rows[5],
            super::NavigationRow::File { index: 2, .. }
        ));
        assert!(matches!(
            rows[6],
            super::NavigationRow::Group { ref label, .. } if label == "untracked/"
        ));
        assert!(matches!(
            rows[7],
            super::NavigationRow::Group { ref label, .. } if label == "src/core/"
        ));
        assert!(matches!(
            rows[8],
            super::NavigationRow::File { index: 1, .. }
        ));

        let layout = app.ui_layout(app.viewport);
        let navigation = layout.navigation.expect("wide layout has navigation");
        assert!(matches!(
            app.navigation_row_at(navigation, navigation.x + 2, navigation.y + 3),
            Some(super::NavigationRow::File { index: 0, .. })
        ));
        let output = render(&mut app, 100, 20);
        assert!(output.contains("src/core/"));
        assert!(output.contains("+1 -0"));
        assert!(output.contains("staged/"));
        assert!(output.contains("mixed/"));
        assert!(output.contains("untracked/"));
    }

    #[test]
    fn files_navigation_uses_the_same_grouped_tree_style() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("src/app.rs"), file("src/lib.rs"), file("README.md")];

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
    fn files_navigation_renders_language_badges_before_names() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![
            file("app.tsx"),
            file("lib.rs"),
            file("config.json"),
            file("README.md"),
        ];

        let output = render(&mut app, 100, 20);
        assert!(output.contains("⚛  app.tsx"));
        assert!(output.contains("RS lib.rs"));
        assert!(output.contains("{} config.json"));
        assert!(output.contains("MD README.md"));
    }

    #[test]
    fn file_icons_fit_the_fixed_badge_slot() {
        use std::path::Path;

        for path in ["main.rs", "app.ts", "view.tsx", "data.json", "notes.md"] {
            assert!(file_icon(Path::new(path)).glyph.width() <= 2);
        }
    }

    #[test]
    fn clicking_a_folder_collapses_and_reopens_its_files() {
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
    fn files_filter_matches_preindexed_filenames_case_insensitively() {
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.tab = Tab::Files;
        app.files = vec![file("src/Überblick.rs"), file("docs/README.md")];
        app.file_search_index = FileSearchIndex::from_files(&app.files);

        app.files_state.filter = "ÜBER".into();
        assert_eq!(app.filtered_file_indices(), vec![0]);

        app.files_state.filter = "readme".into();
        assert_eq!(app.filtered_file_indices(), vec![1]);
    }

    #[test]
    fn mouse_click_switches_tabs_and_selects_list_items() {
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
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
        let mut app = App::new("w1:p1".into());
        app.loading = false;
        app.focus = Focus::Content;
        app.viewport = Rect::new(0, 0, 100, 20);
        app.git_changes = vec![git_change("src/main.rs", ChangeKind::Modified)];
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
        app.git_diff_cache.insert(0, lines, 10_000);

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
        let mut app = App::new("w1:p1".into());
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
    fn large_sources_are_highlighted_in_bounded_chunks() {
        let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let themes = syntect::highlighting::ThemeSet::load_defaults();
        let text = "let value = 1;\n".repeat(super::HIGHLIGHT_CHUNK_LINES + 1);
        let mut emissions = Vec::new();

        highlight_source_chunks(
            Path::new("/tmp/project"),
            Path::new("large.js"),
            &text,
            &syntaxes,
            &themes.themes["base16-ocean.dark"],
            |start, lines, complete| {
                emissions.push((start, lines.len(), complete));
                true
            },
        );

        assert_eq!(
            emissions,
            vec![
                (0, super::HIGHLIGHT_CHUNK_LINES, false),
                (super::HIGHLIGHT_CHUNK_LINES, 1, true),
            ]
        );
    }

    #[test]
    fn javascript_family_extensions_use_javascript_syntax() {
        let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();

        for extension in ["jsx", "tsx", "ts"] {
            let syntax = syntax_for_file(
                Path::new("/tmp/project"),
                Path::new(&format!("component.{extension}")),
                &syntaxes,
            );
            assert_eq!(syntax.name, "JavaScript", "*.{extension}");
        }
    }

    #[test]
    fn syntax_colors_are_brightened_without_changing_background() {
        assert_eq!(brighten_code_component(100), 125);
        assert_eq!(brighten_code_component(255), 255);
    }
}
