#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Header,
    Hunk,
    Addition,
    Deletion,
    Context,
    Notice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

impl DiffLine {
    fn new(kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            old_line: None,
            new_line: None,
        }
    }

    fn numbered(
        kind: DiffLineKind,
        text: impl Into<String>,
        old_line: Option<usize>,
        new_line: Option<usize>,
    ) -> Self {
        Self {
            kind,
            text: text.into(),
            old_line,
            new_line,
        }
    }
}

pub(crate) fn parse_unified_diff(unified: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut cursors = None;
    let mut saw_hunk = false;
    unified.lines().for_each(|line| {
        if line.starts_with("@@") {
            saw_hunk = true;
            cursors = parse_hunk_cursors(line);
            lines.push(DiffLine::new(DiffLineKind::Hunk, line));
            return;
        }

        if !saw_hunk {
            let kind = if line.starts_with("Binary files") {
                DiffLineKind::Notice
            } else {
                DiffLineKind::Header
            };
            lines.push(DiffLine::new(kind, line));
            return;
        }

        let Some((old_cursor, new_cursor)) = cursors else {
            lines.push(DiffLine::new(DiffLineKind::Notice, line));
            return;
        };
        let (kind, old_line, new_line, next_cursors) = match line.chars().next() {
            Some('+') => (
                DiffLineKind::Addition,
                None,
                Some(new_cursor),
                (old_cursor, new_cursor.saturating_add(1)),
            ),
            Some('-') => (
                DiffLineKind::Deletion,
                Some(old_cursor),
                None,
                (old_cursor.saturating_add(1), new_cursor),
            ),
            Some(' ') => (
                DiffLineKind::Context,
                Some(old_cursor),
                Some(new_cursor),
                (old_cursor.saturating_add(1), new_cursor.saturating_add(1)),
            ),
            _ => (DiffLineKind::Notice, None, None, (old_cursor, new_cursor)),
        };
        lines.push(DiffLine::numbered(kind, line, old_line, new_line));
        cursors = Some(next_cursors);
    });
    lines
}

fn parse_hunk_cursors(line: &str) -> Option<(usize, usize)> {
    let ranges = line.strip_prefix("@@")?.split("@@").next()?;
    let mut old = None;
    let mut new = None;
    for range in ranges.split_whitespace() {
        let (kind, value) = range.split_at(1);
        let start = value.split(',').next()?.parse().ok()?;
        match kind {
            "-" => old = Some(start),
            "+" => new = Some(start),
            _ => {}
        }
    }
    old.zip(new)
}
