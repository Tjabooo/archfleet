//! Small widgets drawn straight into a buffer: buttons, tables, a line editor and logs, plus the
//! map of what's clickable where.
use super::text::*;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TableId {
    Fleet,
    Procs,
    Vpn,
    Workspaces,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    ProcFilter,
    LocalPath,
    RemotePath,
    RunCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Tab,
    Table(TableId),
    Activity,
    Report,
    VpnLog,
    RunLog,
    Clip(usize),
}

/// Something that happens when a key or a click asks for it.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Machine(usize),
    Tab(usize),
    Row(TableId, usize),
    Header(TableId, usize),
    Focus(Field),
    Link(String),
    Wake,
    Shell,
    Sftp,
    Tmux,
    SpeedTest,
    ProcMine,
    ProcEnd,
    Peek,
    Lock,
    Notify,
    OpenUrl,
    HyprRefresh,
    HyprReload,
    GoWorkspace,
    FocusWindow,
    VpnConnect,
    VpnDisconnect,
    VpnReconnect,
    VpnRelay,
    VpnRestart,
    VpnTest,
    Upload,
    Download,
    ClipRefreshAll,
    ClipRefresh(usize),
    ClipCopy(usize),
    ClipSend(usize),
    ClipView(usize),
    Run,
    RunTarget(usize),
    Report(&'static str),
    News,
    Upgrade,
    Packages,
    ModalYes,
    ModalNo,
}

#[derive(Default)]
pub struct Hits {
    pub clicks: Vec<(Rect, Act)>,
    pub scrolls: Vec<(Rect, Scroll)>,
}

impl Hits {
    pub fn click(&mut self, area: Rect, act: Act) {
        self.clicks.push((area, act));
    }
    pub fn scroll(&mut self, area: Rect, target: Scroll) {
        self.scrolls.push((area, target));
    }
    /// The last one drawn wins: modals and overlays are drawn after what's under them.
    pub fn click_at(&self, x: u16, y: u16) -> Option<&Act> {
        self.clicks.iter().rev().find(|(r, _)| contains(*r, x, y)).map(|(_, a)| a)
    }
    pub fn scroll_at(&self, x: u16, y: u16) -> Option<Scroll> {
        self.scrolls.iter().rev().find(|(r, _)| contains(*r, x, y)).map(|(_, s)| *s)
    }
    /// Move what was recorded while drawing into an off-screen buffer onto the screen.
    pub fn shifted(other: Hits, dy: i32, clip: Rect) -> Hits {
        let move_rect = |r: Rect| -> Option<Rect> {
            let top = r.y as i32 + dy;
            let bottom = top + r.height as i32;
            let (top, bottom) = (top.max(clip.y as i32), bottom.min(clip.bottom() as i32));
            (bottom > top).then(|| Rect::new(r.x, top as u16, r.width, (bottom - top) as u16))
        };
        Hits {
            clicks: other.clicks.into_iter().filter_map(|(r, a)| move_rect(r).map(|r| (r, a))).collect(),
            scrolls: other.scrolls.into_iter().filter_map(|(r, s)| move_rect(r).map(|r| (r, s))).collect(),
        }
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

pub fn width(text: &str) -> u16 {
    Line::raw(text).width() as u16
}

pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    buf.set_style(area.intersection(buf.area), style);
}

/// Text on one row, cut at the area's right edge. Returns the columns used.
pub fn put(buf: &mut Buffer, x: u16, y: u16, max: u16, line: &Line) -> u16 {
    if y < buf.area.y || y >= buf.area.bottom() || max == 0 {
        return 0;
    }
    let (end_x, _) = buf.set_line(x, y, line, max);
    end_x - x
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Normal,
    Primary,
    Off,
}

/// One-row button: " Label  k ". Returns its width.
pub fn button(buf: &mut Buffer, hits: &mut Hits, x: u16, y: u16, label: &str, key: &str, kind: Kind, act: Act) -> u16 {
    let (bg, fg) = match kind {
        Kind::Primary => (PRIMARY, PRIMARY_FG),
        Kind::Normal => (BUTTON, FG),
        Kind::Off => (PANEL, DIM),
    };
    let key_style = match kind {
        Kind::Primary => Style::new().bg(ACCENT).fg(BG).add_modifier(Modifier::BOLD),
        Kind::Normal => Style::new().bg(hex(0x22344f)).fg(ACCENT),
        Kind::Off => Style::new().bg(PANEL).fg(hex(0x3a4a63)),
    };
    let mut spans = vec![Span::styled(format!(" {label} "), Style::new().bg(bg).fg(fg))];
    if !key.is_empty() {
        spans.push(Span::styled(format!(" {key} "), key_style));
    }
    let line = Line::from(spans);
    let w = line.width() as u16;
    if y >= buf.area.y && y < buf.area.bottom() {
        buf.set_line(x, y, &line, w);
    }
    if kind != Kind::Off {
        hits.click(Rect::new(x, y, w, 1), act);
    }
    w
}

pub struct Btn {
    pub label: String,
    pub key: &'static str,
    pub kind: Kind,
    pub act: Act,
    pub show: bool,
}

pub fn btn(label: impl Into<String>, key: &'static str, kind: Kind, act: Act) -> Btn {
    Btn { label: label.into(), key, kind, act, show: true }
}

/// A row of buttons that wraps onto more rows (a blank row between). Returns the height used.
pub fn button_row(buf: &mut Buffer, hits: &mut Hits, area: Rect, buttons: &[Btn]) -> u16 {
    let (mut x, mut y) = (area.x, area.y);
    for b in buttons.iter().filter(|b| b.show) {
        let w = width(&b.label) + 2 + if b.key.is_empty() { 0 } else { width(b.key) + 2 };
        if x > area.x && x + w > area.right() {
            x = area.x;
            y += 2;
        }
        x += button(buf, hits, x, y, &b.label, b.key, b.kind, b.act.clone()) + 2;
    }
    y + 1 - area.y
}

pub fn checkbox(buf: &mut Buffer, hits: &mut Hits, x: u16, y: u16, label: &str, on: bool, key: &str, act: Act) -> u16 {
    let mark = if on { Span::styled(" ■ ", Style::new().fg(ACCENT).bg(INPUT)) } else { Span::styled(" □ ", Style::new().fg(DIM).bg(INPUT)) };
    let mut spans = vec![mark, Span::styled(format!("{label} "), Style::new().fg(if on { FG } else { SOFT }).bg(INPUT))];
    if !key.is_empty() {
        spans.push(Span::styled(format!(" {key} "), Style::new().bg(hex(0x22344f)).fg(ACCENT)));
    }
    let line = Line::from(spans);
    let w = line.width() as u16;
    put(buf, x, y, w, &line);
    hits.click(Rect::new(x, y, w, 1), act);
    w
}

pub fn eyebrow(buf: &mut Buffer, area: Rect, text: &str) {
    put(buf, area.x, area.y, area.width, &Line::styled(text.to_string(), Style::new().fg(MUTED).add_modifier(Modifier::BOLD)));
}

pub fn card(title: &str, highlight: bool) -> Block<'static> {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if highlight { ACCENT } else { BORDER }))
        .style(Style::new().bg(CARD));
    if title.is_empty() {
        block
    } else {
        block.title(Span::styled(format!(" {title} "), Style::new().fg(if highlight { ACCENT } else { MUTED }).add_modifier(Modifier::BOLD)))
    }
}

// ---------- line editor ----------

#[derive(Default, Clone)]
pub struct Input {
    pub text: String,
    cursor: usize,
    history: Vec<String>,
    position: usize,
}

impl Input {
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
    }
    fn byte(&self, char_index: usize) -> usize {
        self.text.char_indices().nth(char_index).map_or(self.text.len(), |(i, _)| i)
    }
    pub fn insert(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let at = self.byte(self.cursor);
        self.text.insert_str(at, &clean);
        self.cursor += clean.chars().count();
    }
    /// Remember a submitted line for Up/Down.
    pub fn remember(&mut self) {
        if !self.text.is_empty() && self.history.last() != Some(&self.text) {
            self.history.push(self.text.clone());
        }
        self.position = self.history.len();
    }
    pub fn older(&mut self) {
        if self.position > 0 {
            self.position -= 1;
            let text = self.history[self.position].clone();
            self.set(&text);
        }
    }
    pub fn newer(&mut self) {
        if self.position < self.history.len() {
            self.position += 1;
            let text = self.history.get(self.position).cloned().unwrap_or_default();
            self.set(&text);
        }
    }
    /// Editing keys. Returns whether the key was used.
    pub fn key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = len,
                'u' => {
                    let at = self.byte(self.cursor);
                    self.text.replace_range(..at, "");
                    self.cursor = 0;
                }
                'k' => {
                    let at = self.byte(self.cursor);
                    self.text.truncate(at);
                }
                'w' => {
                    let chars: Vec<char> = self.text.chars().collect();
                    let mut start = self.cursor;
                    while start > 0 && chars[start - 1] == ' ' {
                        start -= 1;
                    }
                    while start > 0 && chars[start - 1] != ' ' {
                        start -= 1;
                    }
                    let (a, b) = (self.byte(start), self.byte(self.cursor));
                    self.text.replace_range(a..b, "");
                    self.cursor = start;
                }
                _ => return false,
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    pub fn render(&self, buf: &mut Buffer, hits: &mut Hits, area: Rect, placeholder: &str, focused: bool, field: Field) {
        let style = Style::new().bg(INPUT).fg(FG);
        fill(buf, area, style);
        let edge = Span::styled("▎", Style::new().bg(INPUT).fg(if focused { ACCENT } else { BORDER }));
        put(buf, area.x, area.y, 1, &Line::from(edge));
        hits.click(area, Act::Focus(field));
        let inner = area.width.saturating_sub(2);
        if self.text.is_empty() {
            let mut spans = Vec::new();
            if focused {
                spans.push(Span::styled(" ", Style::new().bg(ACCENT)));
            }
            spans.push(Span::styled(placeholder.to_string(), Style::new().bg(INPUT).fg(DIM)));
            put(buf, area.x + 1, area.y, inner, &Line::from(spans));
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let start = (self.cursor + 1).saturating_sub(inner as usize);
        let mut spans = Vec::new();
        let before: String = chars[start..self.cursor].iter().collect();
        spans.push(Span::styled(before, style));
        if focused {
            let at = chars.get(self.cursor).map_or(" ".into(), |c| c.to_string());
            spans.push(Span::styled(at, Style::new().bg(ACCENT).fg(BG)));
            spans.push(Span::styled(chars.iter().skip(self.cursor + 1).collect::<String>(), style));
        } else {
            spans.push(Span::styled(chars[self.cursor..].iter().collect::<String>(), style));
        }
        put(buf, area.x + 1, area.y, inner, &Line::from(spans));
    }
}

// ---------- logs ----------

pub struct Log {
    pub lines: Vec<(Line<'static>, Option<String>)>,
    /// Rows scrolled up from the end (follow) or down from the top (read from the top).
    pub offset: usize,
    pub from_top: bool,
    limit: usize,
}

impl Log {
    pub fn new(limit: usize, from_top: bool) -> Log {
        Log { lines: Vec::new(), offset: 0, from_top, limit }
    }
    pub fn clear(&mut self) {
        self.lines.clear();
        self.offset = 0;
    }
    pub fn push(&mut self, line: Line<'static>) {
        self.lines.push((line, None));
        self.trim();
    }
    pub fn link(&mut self, line: Line<'static>, url: String) {
        self.lines.push((line, Some(url)));
    }
    pub fn text(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            self.lines.push((Line::styled(line.replace('\t', "    "), style), None));
        }
        self.trim();
    }
    fn trim(&mut self) {
        if self.lines.len() > self.limit {
            let extra = self.lines.len() - self.limit;
            self.lines.drain(..extra);
        }
    }
    pub fn scroll(&mut self, rows: i32) {
        // positive: towards older (up)
        let delta = if self.from_top { -rows } else { rows };
        self.offset = (self.offset as i64 + delta as i64).max(0) as usize;
    }

    /// Draw inside a rounded border with `title`, word-wrapped; links are clickable.
    pub fn render(&mut self, buf: &mut Buffer, hits: &mut Hits, area: Rect, title: &str, target: Scroll) {
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(BORDER_DIM))
            .style(Style::new().bg(hex(0x0d1521)).fg(SOFT))
            .title(Span::styled(format!(" {title} "), Style::new().fg(DIM)));
        let inner = block.inner(area).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
        block.render(area, buf);
        hits.scroll(area, target);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let heights: Vec<usize> = self
            .lines
            .iter()
            .map(|(l, _)| if l.width() <= inner.width as usize { 1 } else { Paragraph::new(l.clone()).wrap(Wrap { trim: false }).line_count(inner.width) })
            .collect();
        let total: usize = heights.iter().sum();
        let room = inner.height as usize;
        let max_offset = total.saturating_sub(room);
        self.offset = self.offset.min(max_offset);
        let first_row = if self.from_top { self.offset } else { max_offset - self.offset };
        let mut row = 0usize;
        for ((line, link), h) in self.lines.iter().zip(&heights) {
            let (top, bottom) = (row, row + h);
            row = bottom;
            if bottom <= first_row {
                continue;
            }
            if top >= first_row + room {
                break;
            }
            let skip = first_row.saturating_sub(top);
            let y = inner.y + (top + skip - first_row) as u16;
            let visible = (*h - skip).min(room - (y - inner.y) as usize) as u16;
            let rect = Rect::new(inner.x, y, inner.width, visible);
            Paragraph::new(line.clone()).wrap(Wrap { trim: false }).scroll((skip as u16, 0)).render(rect, buf);
            if let Some(url) = link {
                hits.click(rect, Act::Link(url.clone()));
            }
        }
        if total > room {
            let pos = (first_row * (room - 1)).checked_div(max_offset).unwrap_or(0);
            let bar = Span::styled("┃", Style::new().fg(BORDER));
            put(buf, area.right() - 1, inner.y + pos as u16, 1, &Line::from(bar));
        }
    }
}

// ---------- tables ----------

pub struct Col {
    pub title: String,
    pub width: u16, // 0: share what's left
    pub right: bool,
    pub style: Style,
}

pub fn col(title: impl Into<String>, width: u16, right: bool) -> Col {
    Col { title: title.into(), width, right, style: Style::new().fg(SOFT).add_modifier(Modifier::BOLD) }
}

pub struct TableView {
    pub id: TableId,
    pub cursor: Option<usize>,
    pub focused: bool,
}

/// Header + rows, zebra striped, the cursor row highlighted. `offset` keeps the cursor in view.
pub fn table(buf: &mut Buffer, hits: &mut Hits, area: Rect, cols: &[Col], rows: &[Vec<Span<'static>>], view: &TableView, offset: &mut usize) {
    if area.height == 0 {
        return;
    }
    fill(buf, area, Style::new().bg(SIDEBAR));
    let fixed: u16 = cols.iter().map(|c| c.width + 1).sum();
    let flexible = cols.iter().filter(|c| c.width == 0).count() as u16;
    let share = area.width.saturating_sub(fixed + flexible).checked_div(flexible).unwrap_or(0);
    let widths: Vec<u16> = cols.iter().map(|c| if c.width == 0 { share.max(4) } else { c.width }).collect();
    // header
    fill(buf, Rect::new(area.x, area.y, area.width, 1), Style::new().bg(HEAD));
    let mut x = area.x + 1;
    for (i, (c, w)) in cols.iter().zip(&widths).enumerate() {
        let text = if c.right { format!("{:>w$}", c.title, w = *w as usize) } else { c.title.clone() };
        put(buf, x, area.y, (*w).min(area.right().saturating_sub(x)), &Line::styled(text, c.style.bg(HEAD)));
        hits.click(Rect::new(x, area.y, *w, 1), Act::Header(view.id, i));
        x += w + 1;
        if x >= area.right() {
            break;
        }
    }
    let room = area.height.saturating_sub(1) as usize;
    if let Some(cursor) = view.cursor {
        if cursor < *offset {
            *offset = cursor;
        } else if room > 0 && cursor >= *offset + room {
            *offset = cursor + 1 - room;
        }
    }
    *offset = (*offset).min(rows.len().saturating_sub(room));
    for (i, row) in rows.iter().enumerate().skip(*offset).take(room) {
        let y = area.y + 1 + (i - *offset) as u16;
        let selected = view.cursor == Some(i);
        let bg = if selected {
            if view.focused { CURSOR } else { CURSOR_IDLE }
        } else if i % 2 == 1 {
            ZEBRA
        } else {
            SIDEBAR
        };
        fill(buf, Rect::new(area.x, y, area.width, 1), Style::new().bg(bg).fg(if selected { CURSOR_FG } else { FG }));
        let mut x = area.x + 1;
        for ((cell, c), w) in row.iter().zip(cols).zip(&widths) {
            let mut cell = cell.clone();
            cell.style = cell.style.bg(bg);
            if selected && cell.style.fg.is_none() {
                cell.style = cell.style.fg(CURSOR_FG);
            }
            let cw = cell.width() as u16;
            let shown = if c.right && cw < *w { x + w - cw } else { x };
            put(buf, shown, y, (*w).min(area.right().saturating_sub(shown)), &Line::from(cell));
            x += w + 1;
            if x >= area.right() {
                break;
            }
        }
        hits.click(Rect::new(area.x, y, area.width, 1), Act::Row(view.id, i));
    }
    if rows.len() > room && room > 0 {
        let pos = *offset * (room - 1) / rows.len().saturating_sub(room).max(1);
        put(buf, area.right() - 1, area.y + 1 + pos as u16, 1, &Line::styled("┃", Style::new().fg(BORDER)));
    }
    hits.scroll(area, Scroll::Table(view.id));
}

/// A paragraph of markup, cut (not wrapped) at the area's edge. Returns its height.
pub fn markup_block(buf: &mut Buffer, area: Rect, source: &str, base: Style) -> u16 {
    let text: Text = markup(source);
    let height = text.lines.len() as u16;
    Paragraph::new(text).style(base).render(Rect { height: height.min(area.height), ..area }, buf);
    height
}
