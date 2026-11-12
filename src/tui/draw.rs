//! Drawing the dashboard. Everything is drawn from App's state each time something changed.
use super::text::*;
use super::widgets::*;
use super::*;
use crate::util::{self, ja, jf, js, jtext};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

const SIDEBAR_WIDTH: u16 = 25;

struct Flow {
    x: u16,
    width: u16,
    y: u16,
    top: u16,
}

impl Flow {
    fn new(area: Rect) -> Flow {
        Flow { x: area.x, width: area.width, y: area.y, top: area.y }
    }
    fn take(&mut self, height: u16) -> Rect {
        let r = Rect::new(self.x, self.y, self.width, height);
        self.y += height;
        r
    }
    fn gap(&mut self, height: u16) {
        self.y += height;
    }
    fn used(&self) -> u16 {
        self.y - self.top
    }
}

fn styled(text: impl Into<String>, color: ratatui::style::Color) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color))
}

fn bold(text: impl Into<String>, color: ratatui::style::Color) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color).add_modifier(Modifier::BOLD))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let mut hits = Hits::default();
    let buf = frame.buffer_mut();
    fill(buf, area, Style::new().bg(BG).fg(FG));
    if area.width < 70 || area.height < 22 {
        let text = format!("Archfleet needs at least 70×22 (this is {}×{}).", area.width, area.height);
        put(buf, 1, area.height / 2, area.width.saturating_sub(2), &Line::styled(text, Style::new().fg(MUTED)));
        app.hits = hits;
        return;
    }
    header(buf, app, Rect::new(0, 0, area.width, 1));
    footer(buf, app, Rect::new(0, area.height - 1, area.width, 1));
    let body = Rect::new(0, 1, area.width, area.height - 2);
    sidebar(buf, &mut hits, app, Rect::new(0, body.y, SIDEBAR_WIDTH, body.height));
    let main = Rect::new(SIDEBAR_WIDTH + 2, body.y, area.width - SIDEBAR_WIDTH - 3, body.height);
    heading(buf, &mut hits, app, Rect::new(main.x, main.y + 1, main.width, 1));
    tab_bar(buf, &mut hits, app, Rect::new(main.x, main.y + 3, main.width, 2));
    let activity_height = 6;
    let content = Rect::new(main.x, main.y + 5, main.width, main.height.saturating_sub(5 + activity_height));
    app.activity.render(buf, &mut hits, Rect::new(main.x - 1, main.bottom() - activity_height, main.width + 2, activity_height), "ACTIVITY", Scroll::Activity);
    match app.tab {
        PROCESSES => processes(buf, &mut hits, app, content),
        RUN => run_tab(buf, &mut hits, app, content),
        MAINTENANCE => maintenance(buf, &mut hits, app, content),
        tab => {
            let mut scroll = app.tab_scroll[tab];
            scrolled(buf, &mut hits, content, &mut scroll, |buf, hits, area| match tab {
                OVERVIEW => overview(buf, hits, app, area),
                SESSION => session(buf, hits, app, area),
                VPN => vpn(buf, hits, app, area),
                FILES => files(buf, hits, app, area),
                _ => clipboard(buf, hits, app, area),
            });
            app.tab_scroll[tab] = scroll;
        }
    }
    toasts(buf, app, area);
    if app.modal.is_some() {
        modal(buf, &mut hits, app, area);
    }
    app.hits = hits;
}

/// Draw content taller than its area into an off-screen buffer and show the scrolled part.
fn scrolled(buf: &mut Buffer, hits: &mut Hits, area: Rect, scroll: &mut usize, f: impl FnOnce(&mut Buffer, &mut Hits, Rect) -> u16) {
    let virt = Rect::new(area.x, 0, area.width, 300);
    let mut tmp = Buffer::empty(virt);
    fill(&mut tmp, virt, Style::new().bg(BG).fg(FG));
    let mut inner = Hits::default();
    let used = f(&mut tmp, &mut inner, virt).min(virt.height);
    let max = used.saturating_sub(area.height) as usize;
    *scroll = (*scroll).min(max);
    for row in 0..area.height.min(used.saturating_sub(*scroll as u16)) {
        for x in area.x..area.right() {
            buf[(x, area.y + row)] = tmp[(x, *scroll as u16 + row)].clone();
        }
    }
    hits.scroll(area, Scroll::Tab);
    let shifted = Hits::shifted(inner, area.y as i32 - *scroll as i32, area);
    hits.clicks.extend(shifted.clicks);
    hits.scrolls.extend(shifted.scrolls);
    if max > 0 && area.height > 2 {
        let pos = *scroll * (area.height as usize - 1) / max;
        put(buf, area.right(), area.y + pos as u16, 1, &Line::styled("┃", Style::new().fg(BORDER)));
        if *scroll < max {
            put(buf, area.right() - 9, area.bottom() - 1, 9, &Line::styled(" ↓ more ", Style::new().fg(MUTED).bg(PANEL)));
        }
    }
}

fn header(buf: &mut Buffer, app: &App, area: Rect) {
    fill(buf, area, Style::new().bg(PANEL));
    let count = app.nodes.len();
    let words = ["no machines", "one machine", "two machines", "three machines", "four machines", "five machines"];
    let machines = words.get(count).map(|w| w.to_string()).unwrap_or_else(|| format!("{count} machines"));
    let line = Line::from(vec![bold(" ◆ ARCHFLEET", ACCENT), styled(format!("   home network / {machines}"), MUTED)]);
    put(buf, area.x, area.y, area.width, &line);
    let clock = format!(" {} ", app.clock);
    put(buf, area.right() - width(&clock), area.y, width(&clock), &Line::from(bold(clock, FG)));
}

fn footer(buf: &mut Buffer, app: &App, area: Rect) {
    fill(buf, area, Style::new().bg(PANEL));
    let keys: Vec<(String, &str)> = if app.modal.is_some() {
        vec![("Enter".into(), "confirm"), ("Esc".into(), "cancel"), ("←→".into(), "choose")]
    } else if app.focus.is_some() {
        let mut k = vec![("Esc".into(), "leave field"), ("Enter".into(), if app.focus == Some(Field::RunCommand) { "run" } else { "done" })];
        if app.focus == Some(Field::RunCommand) {
            k.push(("↑↓".into(), "history"));
            k.push(("Alt+1…".into(), "tick machine"));
        }
        k
    } else {
        let mut k = vec![
            (format!("1-{}", app.nodes.len().min(9)), "machine"),
            ("←→".into(), "tab"),
            ("r".into(), "refresh"),
            ("s".into(), "SSH"),
            ("f".into(), "SFTP"),
        ];
        if app.is_offline(app.node()) {
            k.push(("w".into(), "wake"));
        }
        k.extend(match app.tab {
            OVERVIEW => vec![("↑↓ ⏎".into(), "pick machine")],
            PROCESSES => vec![("/".into(), "filter"), ("o O".into(), "sort")],
            SESSION => vec![("t".into(), "switch table"), ("⏎".into(), "go / focus")],
            VPN => vec![("↑↓ ⏎".into(), "pick machine")],
            CLIPBOARD => vec![("↑↓".into(), "machine")],
            RUN => vec![("i".into(), "type a command")],
            MAINTENANCE => vec![("↑↓".into(), "scroll")],
            _ => vec![],
        });
        k.push(("q".into(), "quit"));
        k
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(bold(key, ACCENT));
        spans.push(styled(format!(" {label}   "), SOFT));
    }
    put(buf, area.x, area.y, area.width, &Line::from(spans));
}

fn sidebar(buf: &mut Buffer, hits: &mut Hits, app: &App, area: Rect) {
    fill(buf, area, Style::new().bg(SIDEBAR));
    for y in area.y..area.bottom() {
        put(buf, area.right(), y, 1, &Line::styled("│", Style::new().fg(BORDER_DIM).bg(SIDEBAR)));
    }
    let inner = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
    let mut flow = Flow::new(inner);
    flow.gap(1);
    eyebrow(buf, flow.take(1), "YOUR MACHINES");
    flow.gap(1);
    for (i, node) in app.nodes.iter().enumerate() {
        if flow.y + 4 > area.bottom() {
            break;
        }
        let rect = flow.take(4);
        let selected = i == app.sel;
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if selected { ACCENT } else { hex(0x283852) }))
            .style(Style::new().bg(if selected { SELECTED } else { hex(0x151f30) }));
        let text_area = block.inner(rect);
        block.render(rect, buf);
        let status = app.status(node);
        let (dot, color) = match status {
            Some(s) if s["online"] == true => ("●", ACCENT),
            Some(_) if app.waking.get(&node.alias).is_some_and(|w| !w.1) => ("◌", LAVENDER),
            Some(_) => ("○", BAD),
            None => ("◌", MUTED),
        };
        let fg = if selected { SELECTED_FG } else { SOFT };
        let key = if i < 9 { format!("{}", i + 1) } else { String::new() };
        let label = util::cut(&node.label, text_area.width.saturating_sub(5) as usize);
        put(buf, text_area.x + 1, text_area.y, text_area.width, &Line::from(vec![styled(dot, color), Span::raw(" "), bold(label, fg)]));
        put(buf, text_area.right() - 2, text_area.y, 1, &Line::styled(key, Style::new().fg(DIM)));
        put(
            buf,
            text_area.x + 3,
            text_area.y + 1,
            text_area.width - 3,
            &Line::styled(node.hostname.clone(), Style::new().fg(if selected { fg } else { MUTED })),
        );
        hits.click(rect, Act::Machine(i));
        flow.gap(1);
    }
    flow.gap(1);
    if flow.y + 6 < area.bottom() {
        put(buf, inner.x + 1, flow.take(1).y, inner.width, &Line::styled("PRIVATE LAN", Style::new().fg(ACCENT)));
        put(buf, inner.x + 1, flow.take(1).y, inner.width, &Line::styled(config::lan_label(&app.nodes), Style::new().fg(ACCENT)));
        flow.gap(1);
        put(buf, inner.x + 1, flow.take(1).y, inner.width, &Line::styled("SSH keys stay on", Style::new().fg(MUTED)));
        put(buf, inner.x + 1, flow.take(1).y, inner.width, &Line::styled("their own machines.", Style::new().fg(MUTED)));
    }
}

fn heading(buf: &mut Buffer, hits: &mut Hits, app: &App, area: Rect) {
    let node = app.node();
    let line = Line::from(vec![
        bold(format!("{}  /  {}", node.label, node.hostname), LAVENDER),
        styled(if node.user.is_empty() && node.address.is_empty() { String::new() } else { format!("     {}@{}", node.user, node.address) }, MUTED),
    ]);
    put(buf, area.x, area.y, area.width, &line);
    // wake state on the right
    let waking = app.waking.get(&node.alias);
    let (state, color, button) = if let Some((started, false)) = waking {
        let s = started.elapsed().as_secs();
        (format!("◌ Waking · {}:{:02} / 2:00", s / 60, s % 60), LAVENDER, None)
    } else if app.is_offline(node) && node.mac.is_some() {
        let timed_out = waking.is_some_and(|w| w.1);
        (if timed_out { "No answer after 2 minutes".into() } else { "○ Offline".into() }, BAD, Some(if timed_out { "Retry wake" } else { "Wake" }))
    } else if app.is_offline(node) {
        ("○ Offline · Wi-Fi only, wake it by hand".into(), BAD, None)
    } else {
        (String::new(), FG, None)
    };
    let mut right = area.right();
    if let Some(label) = button {
        let w = width(label) + 2 + 3;
        right -= w;
        button_row(buf, hits, Rect::new(right, area.y, w, 1), &[btn(label, "w", Kind::Primary, Act::Wake)]);
        right -= 2;
    }
    if !state.is_empty() {
        let w = width(&state);
        put(buf, right.saturating_sub(w), area.y, w, &Line::styled(state, Style::new().fg(color)));
    }
}

fn tab_bar(buf: &mut Buffer, hits: &mut Hits, app: &App, area: Rect) {
    let mut x = area.x;
    put(buf, area.x, area.y + 1, area.width, &Line::styled("─".repeat(area.width as usize), Style::new().fg(BORDER_DIM)));
    for (i, title) in TABS.iter().enumerate() {
        let w = width(title) + 2;
        if x + w > area.right() {
            break;
        }
        let active = i == app.tab;
        let style = if active { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(MUTED) };
        put(buf, x, area.y, w, &Line::styled(format!(" {title} "), style));
        if active {
            put(buf, x, area.y + 1, w, &Line::styled("━".repeat(w as usize), Style::new().fg(ACCENT)));
        }
        hits.click(Rect::new(x, area.y, w, 2), Act::Tab(i));
        x += w + 1;
    }
}

fn toasts(buf: &mut Buffer, app: &App, area: Rect) {
    let mut y = area.bottom().saturating_sub(2);
    for toast in app.toasts.iter().rev() {
        let w = (width(&toast.text) + 4).min(64).min(area.width - 2);
        let lines = Paragraph::new(toast.text.clone()).wrap(Wrap { trim: true }).line_count(w - 4) as u16;
        let h = lines.min(4) + 2;
        if y < h + 2 {
            break;
        }
        y -= h;
        let rect = Rect::new(area.right() - w - 1, y, w, h);
        Clear.render(rect, buf);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if toast.error { BAD } else { ACCENT }))
            .style(Style::new().bg(hex(0x152237)).fg(FG));
        let inner = block.inner(rect).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
        block.render(rect, buf);
        Paragraph::new(toast.text.clone()).wrap(Wrap { trim: true }).render(inner, buf);
    }
}

fn modal(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) {
    // dim what's behind it
    for cell in buf.content.iter_mut() {
        cell.set_fg(DIM);
    }
    let Some(modal) = app.modal.as_mut() else { return };
    let w = 72.min(area.width - 4);
    let (body, buttons): (Vec<Line<'static>>, [(String, bool, Act); 2]) = match modal {
        Modal::Confirm { message, label, yes, .. } => (
            message.split('\n').map(|l| Line::styled(l.to_string(), Style::new().fg(FG))).collect(),
            [("Cancel".into(), !*yes, Act::ModalNo), (label.clone(), *yes, Act::ModalYes)],
        ),
        Modal::Prompt { title, label, .. } => (
            vec![Line::styled(title.clone(), Style::new().fg(LAVENDER).add_modifier(Modifier::BOLD))],
            [("Cancel".into(), false, Act::ModalNo), (label.clone(), true, Act::ModalYes)],
        ),
    };
    let text_height = Paragraph::new(body.clone()).wrap(Wrap { trim: false }).line_count(w - 6) as u16;
    let prompt = matches!(modal, Modal::Prompt { .. });
    let h = text_height + 4 + if prompt { 2 } else { 0 };
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + area.height.saturating_sub(h) / 2, w, h);
    Clear.render(rect, buf);
    let block =
        Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).style(Style::new().bg(hex(0x152237)).fg(FG));
    let inner = block.inner(rect).inner(ratatui::layout::Margin { horizontal: 2, vertical: 0 });
    block.render(rect, buf);
    Paragraph::new(body).wrap(Wrap { trim: false }).render(Rect::new(inner.x, inner.y + 1, inner.width, text_height), buf);
    if let Modal::Prompt { input, placeholder, .. } = modal {
        input.render(buf, hits, Rect::new(inner.x, inner.y + 2 + text_height, inner.width, 1), placeholder, true, Field::RunCommand);
    }
    // buttons, right-aligned
    let mut x = inner.right();
    let y = rect.bottom() - 2;
    for (label, focused, act) in buttons.iter().rev() {
        let kind = if *focused { Kind::Primary } else { Kind::Normal };
        let key = match act {
            Act::ModalYes if !prompt => "y",
            Act::ModalYes => "⏎",
            _ => "esc",
        };
        let w = width(label) + 2 + width(key) + 2;
        x -= w;
        button(buf, hits, x, y, label, key, kind, act.clone());
        x -= 2;
    }
}

// ---------- Overview ----------

fn sparkline(buf: &mut Buffer, area: Rect, data: &[f64], top: f64, empty: &str, color: ratatui::style::Color) {
    if data.is_empty() || (data.len() < 2 && top == 0.0) {
        put(buf, area.x, area.y, area.width, &Line::styled(empty.to_string(), Style::new().fg(MUTED)));
        return;
    }
    let levels = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let top = if top > 0.0 { top } else { data.iter().cloned().fold(0.0, f64::max).max(1e-9) };
    let w = area.width as usize;
    let values: Vec<i64> = (0..w).map(|x| ((data[(x * data.len() / w).min(data.len() - 1)] * 24.0 / top).round() as i64).clamp(0, 24)).collect();
    for row in 0..3i64 {
        let text: String = values.iter().map(|v| levels[(v - (2 - row) * 8).clamp(0, 8) as usize]).collect();
        put(buf, area.x, area.y + row as u16, area.width, &Line::styled(text, Style::new().fg(color)));
    }
}

fn metric(buf: &mut Buffer, area: Rect, title: &str, body: &str) {
    let block = card(title, false);
    let inner = block.inner(area).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
    block.render(area, buf);
    markup_block(buf, inner, body, Style::new().fg(FG).bg(CARD));
}

fn overview(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) -> u16 {
    let node = app.node().clone();
    let data = app.status(&node).cloned().unwrap_or(serde_json::Value::Null);
    let online = data["online"] == true;
    let accent = c(ACCENT);
    let mut flow = Flow::new(area);
    flow.gap(1);
    let row = flow.take(5);
    let card_width = (row.width + 1) / 4;
    let cards: [(&str, String); 4] = if online {
        let cpu = jf(&data, "cpu").unwrap_or(0.0);
        let temp = jf(&data, "cpu_temp").map(|t| format!("  ·  {}", util::celsius(Some(t)))).unwrap_or_default();
        let (mut busy, mut gpu_temp) = (jf(&data, "gpu_busy"), jf(&data, "gpu_temp"));
        if busy.is_none()
            && gpu_temp.is_none()
            && node.lilin
            && let Some(gpu) = app.sharing.as_ref().map(|s| &s["gpu"]).filter(|g| util::truthy(g))
        {
            (busy, gpu_temp) = (jf(gpu, "busy"), jf(gpu, "temp")); // read by Lilin (nvidia-smi)
        }
        let npu = npu_line(&data["npu"]);
        let gpu = if busy.is_none() && gpu_temp.is_none() {
            format!("[#596a85]No readings[/]{}", if npu.is_empty() { String::new() } else { format!("\n{npu}") })
        } else {
            let parts: Vec<String> = [busy.map(|b| format!("{b:.0}% busy")), gpu_temp.map(|t| util::celsius(Some(t)))].into_iter().flatten().collect();
            format!(
                "{}\n[{accent}]{}[/]{}",
                parts.join("  ·  "),
                util::meter(busy.unwrap_or(0.0), 18),
                if npu.is_empty() { String::new() } else { format!("\n{npu}") }
            )
        };
        let used = 100.0 * jf(&data, "memory_used").unwrap_or(0.0) / jf(&data, "memory_total").unwrap_or(1.0).max(1.0);
        let mut disks: Vec<serde_json::Value> = ja(&data, "disks").to_vec();
        if disks.is_empty() {
            disks.push(serde_json::json!({"name": if node.linux() { "/" } else { "C:" }, "used": data["disk_used"], "total": data["disk_total"]}));
        }
        let whole = |v: f64| if v >= 100.0 * 1024f64.powi(3) { format!("{:.0}", v / 1024f64.powi(3)) } else { format!("{:.1}", v / 1024f64.powi(3)) };
        let storage = if disks.len() == 1 {
            let d = &disks[0];
            let (u, t) = (jf(d, "used").unwrap_or(0.0), jf(d, "total").unwrap_or(0.0));
            format!("{} / {} GiB\n[{accent}]{}[/]", whole(u), whole(t), util::meter(100.0 * u / t.max(1.0), 18))
        } else {
            disks
                .iter()
                .take(3)
                .map(|d| {
                    let (u, t) = (jf(d, "used").unwrap_or(0.0), jf(d, "total").unwrap_or(0.0));
                    format!("{} {}/{}G [#91a3bf]{:.0}%[/]", jtext(&d["name"]), whole(u), whole(t), 100.0 * u / t.max(1.0))
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        [
            ("CPU", format!("{cpu:.1}%{temp}\n[{accent}]{}[/]", util::meter(cpu, 18))),
            ("GPU", gpu),
            (
                "MEMORY",
                format!(
                    "{} / {}\n[{accent}]{}[/]",
                    util::gib(jf(&data, "memory_used").unwrap_or(0.0)),
                    util::gib(jf(&data, "memory_total").unwrap_or(0.0)),
                    util::meter(used, 18)
                ),
            ),
            ("STORAGE", storage),
        ]
    } else {
        let wait = if app.status(&node).is_some() { "—" } else { "Waiting…" };
        [("CPU", wait.into()), ("GPU", wait.into()), ("MEMORY", wait.into()), ("STORAGE", wait.into())]
    };
    for (i, (title, body)) in cards.iter().enumerate() {
        let x = row.x + i as u16 * card_width;
        let w = if i == 3 { row.right() - x } else { card_width - 1 };
        metric(buf, Rect::new(x, row.y, w, row.height), title, body);
    }
    flow.gap(1);
    eyebrow(buf, flow.take(1), "CPU HISTORY  ·  0–100%  ·  15 s/sample");
    let history: Vec<f64> = app.histories.get(&node.alias).map(|h| h.iter().copied().collect()).unwrap_or_default();
    sparkline(buf, flow.take(3), &history, 100.0, "Waiting for CPU samples…", ACCENT);
    flow.gap(1);

    let details = if online {
        let mut first = Vec::new();
        if data.get("net_rx").is_some_and(|v| !v.is_null()) {
            first.push(format!("↓ {}  ↑ {}", util::speed(jf(&data, "net_rx")), util::speed(jf(&data, "net_tx"))));
        }
        first.push(format!("Uptime {}", util::uptime(jf(&data, "uptime").unwrap_or(0.0))));
        first.push(jtext(&data["kernel"]));
        let mut second = Vec::new();
        if node.linux() {
            second.push(format!("Last full upgrade {}", util::ago(js(&data, "last_upgrade"))));
        }
        let batteries: Vec<serde_json::Value> = match &data["battery"] {
            serde_json::Value::Array(a) => a.clone(),
            v if v.is_object() => vec![v.clone()],
            _ => vec![],
        };
        if !batteries.is_empty() {
            second.push(format!(
                "Battery {}",
                batteries.iter().map(|b| format!("{}% {}", jtext(&b["percent"]), jtext(&b["state"]))).collect::<Vec<_>>().join(", ")
            ));
        }
        second.push(format!("status read in {} ms", jtext(&data["elapsed_ms"])));
        format!("{}\n{}", first.join("   ·   "), second.join("   ·   "))
    } else {
        data["error"].as_str().map(str::to_string).unwrap_or_else(|| "Checking connection…".into())
    };
    let rect = flow.take(2);
    Paragraph::new(details).style(Style::new().fg(SOFT)).wrap(Wrap { trim: true }).render(rect, buf);
    flow.gap(1);
    let can_test = !node.is_local() && !app.is_offline(&node) && app.speed_running.is_none();
    let h = button_row(
        buf,
        hits,
        Rect::new(flow.x, flow.y, flow.width, 3),
        &[
            btn("SSH shell", "s", Kind::Primary, Act::Shell),
            btn("SFTP browser", "f", Kind::Normal, Act::Sftp),
            btn("tmux session", "t", if node.linux() { Kind::Normal } else { Kind::Off }, Act::Tmux),
            btn(if app.speed_running.is_some() { "Testing…" } else { "Test link" }, "l", if can_test { Kind::Normal } else { Kind::Off }, Act::SpeedTest),
        ],
    );
    flow.gap(h + 1);
    let (speed_text, speed_color) = if node.is_local() {
        ("Test link measures SSH speed to another machine; this is the one you are on.".to_string(), DIM)
    } else {
        app.speed.get(&node.alias).cloned().unwrap_or(("Test link: latency and SSH speed both ways, 64 MB each (takes a few seconds).".into(), DIM))
    };
    put(buf, flow.x, flow.take(1).y, flow.width, &Line::styled(speed_text, Style::new().fg(speed_color)));
    flow.gap(1);

    if node.lilin {
        eyebrow(buf, flow.take(1), "SHARING  ·  what streams and encodes cost the server");
        flow.gap(1);
        let text = match (&app.sharing, &app.sharing_error) {
            (Some(load), error) => {
                let mut t = sharing_text(load);
                if let Some(error) = error {
                    t = format!("[{}]Last read failed: {} (showing {})[/]\n{t}", c(BAD), escape(error), util::clock(jf(load, "at").unwrap_or(0.0)));
                }
                t
            }
            (None, Some(error)) => format!("[{}]{}[/]", c(BAD), escape(error)),
            (None, None) => "Reading Lilin…".into(),
        };
        let h = markup_block(buf, Rect::new(flow.x, flow.y, flow.width, 60), &text, Style::new().fg(SOFT));
        flow.gap(h + 1);
        let history: Vec<f64> = app
            .sharing
            .as_ref()
            .map(|l| ja(l, "history").iter().map(|p| (jf(p, "inet").unwrap_or(0.0) + jf(p, "tor").unwrap_or(0.0)) * 8.0 / 1e6).collect())
            .unwrap_or_default();
        let peak = history.iter().cloned().fold(0.0, f64::max);
        let limit = app
            .sharing
            .as_ref()
            .and_then(|l| l["limits"].get("uploadMbps"))
            .filter(|v| util::truthy(v))
            .map(|v| format!("  ·  limit {}", jtext(v)))
            .unwrap_or_default();
        eyebrow(buf, flow.take(1), &format!("INTERNET UPLOAD  ·  last hour  ·  peak {peak:.0} Mbit/s{limit}"));
        sparkline(buf, flow.take(3), &history, 0.0, "History builds up a minute at a time…", ACCENT);
        flow.gap(1);
    }

    if node.linux() {
        let entry = app.screentime.get(&node.alias);
        let data = entry.and_then(|e| e.data.as_ref());
        let read = entry.filter(|e| data.is_some() && !e.read.is_empty()).map(|e| format!("  ·  read {}", e.read)).unwrap_or_default();
        eyebrow(buf, flow.take(1), &format!("SCREEN TIME  ·  today on {}{read}", node.hostname));
        flow.gap(1);
        let error = entry.and_then(|e| e.error.as_ref());
        let text = match (data, error) {
            (Some(d), Some(e)) => format!("[{}]Last read failed: {}[/]\n{}", c(BAD), escape(e), screentime_text(d)),
            (Some(d), None) => screentime_text(d),
            (None, Some(e)) => format!("[{}]{}[/]", c(BAD), escape(e)),
            (None, None) => "Reading Screen Time…".into(),
        };
        let h = markup_block(buf, Rect::new(flow.x, flow.y, flow.width, 30), &text, Style::new().fg(FG));
        flow.gap(h + 1);
    }

    let cols = [col("Machine", 16, false), col("Link", 9, false), col("CPU", 5, true), col("Memory", 10, true), col("Uptime", 9, true), col("VPN", 0, false)];
    let rows: Vec<Vec<Span<'static>>> = app
        .nodes
        .iter()
        .map(|n| {
            let s = app.status(n);
            let on = s.is_some_and(|s| s["online"] == true);
            let s = s.cloned().unwrap_or(serde_json::Value::Null);
            let vpn = if on { s["vpn"].clone() } else { serde_json::Value::Null };
            vec![
                Span::raw(n.hostname.clone()),
                styled(
                    if on {
                        "online"
                    } else if s.is_null() {
                        "checking"
                    } else {
                        "offline"
                    },
                    if on { ACCENT } else { BAD },
                ),
                Span::raw(if on { format!("{:.0}%", jf(&s, "cpu").unwrap_or(0.0)) } else { "—".into() }),
                Span::raw(if on { util::gib(jf(&s, "memory_used").unwrap_or(0.0)) } else { "—".into() }),
                Span::raw(if on { util::uptime(jf(&s, "uptime").unwrap_or(0.0)) } else { "—".into() }),
                styled(vpn_short(&vpn), vpn_color(&vpn)),
            ]
        })
        .collect();
    let rect = flow.take(rows.len() as u16 + 1);
    let view = TableView { id: TableId::Fleet, cursor: Some(app.fleet_cursor), focused: true };
    table(buf, hits, rect, &cols, &rows, &view, &mut app.fleet_offset);
    flow.used()
}

// ---------- Processes ----------

fn processes(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) {
    let mut flow = Flow::new(area);
    flow.gap(1);
    let row = flow.take(4);
    let windows = app.process_data.as_ref().is_some_and(|d| d["platform"] == "windows");
    let short = |v: f64| if v < 10.0 * 1024f64.powi(3) { format!("{:.1}G", v / 1024f64.powi(3)) } else { format!("{:.0}G", v / 1024f64.powi(3)) };
    let accent = c(ACCENT);
    let boxes: Vec<(&str, String)> = match &app.process_data {
        Some(data) => {
            let t = &data["totals"];
            let cpu = jf(t, "cpu").unwrap_or(0.0);
            let used = 100.0 * jf(t, "memory_used").unwrap_or(0.0) / jf(t, "memory_total").unwrap_or(1.0).max(1.0);
            let gpu = match jf(t, "gpu") {
                None => "Not available".to_string(),
                Some(g) => format!(
                    "{g:.0}% busy\n{}",
                    match (jf(t, "vram_used"), jf(t, "vram_total")) {
                        (Some(u), Some(total)) if total > 0.0 => format!("VRAM {}/{}", short(u), short(total)),
                        _ => String::new(),
                    }
                ),
            };
            vec![
                ("CPU", format!("{cpu:.0}%\n[{accent}]{}[/]", util::meter(cpu, 8))),
                ("GPU", gpu),
                (
                    "MEMORY",
                    format!(
                        "{}/{}\n[{accent}]{}[/]",
                        short(jf(t, "memory_used").unwrap_or(0.0)),
                        short(jf(t, "memory_total").unwrap_or(0.0)),
                        util::meter(used, 8)
                    ),
                ),
                ("DISK", format!("R {}\nW {}", util::speed(jf(t, "disk_read")), util::speed(jf(t, "disk_write")))),
                ("NETWORK", format!("↓ {}\n↑ {}", util::speed(jf(t, "net_rx")), util::speed(jf(t, "net_tx")))),
            ]
        }
        None => ["CPU", "GPU", "MEMORY", "DISK", "NETWORK"].iter().map(|t| (*t, "—".to_string())).collect(),
    };
    let w = (row.width + 1) / 5;
    for (i, (title, body)) in boxes.iter().enumerate() {
        let x = row.x + i as u16 * w;
        metric(buf, Rect::new(x, row.y, if i == 4 { row.right() - x } else { w - 1 }, 4), title, body);
    }
    flow.gap(1);
    let controls = flow.take(1);
    let can_end = app.selected_process().is_some_and(|p| p["own"] == true) && !windows;
    let end_w = width("End process…") + 2 + 3;
    let mine_w = width("Only mine") + 4 + 3;
    let filter_w = controls.width.saturating_sub(end_w + mine_w + 4);
    app.proc_filter.render(
        buf,
        hits,
        Rect::new(controls.x, controls.y, filter_w, 1),
        "Filter name, command, user, PID   ( / )",
        app.focus == Some(Field::ProcFilter),
        Field::ProcFilter,
    );
    checkbox(buf, hits, controls.x + filter_w + 2, controls.y, "Only mine", app.proc_mine, "m", Act::ProcMine);
    button(buf, hits, controls.right() - end_w, controls.y, "End process…", "e", if can_end { Kind::Normal } else { Kind::Off }, Act::ProcEnd);
    flow.gap(1);
    let status_rect = flow.take(1);
    let footer_height = 5;
    let table_rect = Rect::new(area.x, flow.y, area.width, area.bottom().saturating_sub(flow.y + footer_height));
    let detail_rect = Rect::new(area.x, table_rect.bottom() + 1, area.width, 3);
    let note_rect = Rect::new(area.x, detail_rect.bottom(), area.width, 1);

    let (sort, descending) = app.proc_sort;
    let cols: Vec<Col> = PROCESS_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, (key, title, w, numeric))| {
            let title = match (*key, windows) {
                ("disk", true) => "I/O",
                ("net", true) => "Net",
                _ => title,
            };
            let arrow = if i == sort { if descending { " ▼" } else { " ▲" } } else { "" };
            let mut c = col(format!("{title}{arrow}"), *w, *numeric || *key == "pid");
            if i == sort {
                c.style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
            }
            c
        })
        .chain(std::iter::once(col("Command", 0, false)))
        .collect();
    let rows_all = app.process_rows();
    let total = app.process_data.as_ref().map(|d| ja(d, "processes").len()).unwrap_or(0);
    let matching = rows_all.len();
    let rows: Vec<Vec<Span<'static>>> = rows_all
        .iter()
        .take(PROCESS_LIMIT)
        .map(|p| {
            let mut cells = vec![
                styled(jtext(&p["pid"]), DIM),
                Span::raw(js(p, "name").unwrap_or("?").to_string()),
                styled(js(p, "user").unwrap_or("—").to_string(), if js(p, "user").is_some() { FG } else { DIM }),
            ];
            for key in ["cpu", "gpu", "memory", "disk", "net"] {
                let value = process_value(p, key);
                let text = match key {
                    "cpu" | "gpu" => util::percent_text(value),
                    "memory" => util::size(value),
                    _ => util::speed(value),
                };
                cells.push(if value.unwrap_or(0.0) > 0.0 { Span::raw(text) } else { styled(text, DIM) });
            }
            cells.push(styled(js(p, "command").unwrap_or("").to_string(), DIM));
            cells
        })
        .collect();
    let pids: Vec<u64> = rows_all.iter().take(PROCESS_LIMIT).filter_map(|p| p["pid"].as_u64()).collect();
    let mut cursor = app.proc_cursor.and_then(|pid| pids.iter().position(|p| *p == pid));
    if cursor.is_none() && !pids.is_empty() {
        cursor = Some(0);
        app.proc_cursor = Some(pids[0]);
    }
    let view = TableView { id: TableId::Procs, cursor, focused: true };
    table(buf, hits, table_rect, &cols, &rows, &view, &mut app.proc_offset);

    let status = if let Some(message) = &app.process_message {
        message.clone()
    } else if app.process_data.is_none() {
        "Open this tab to read the selected machine’s processes.".into()
    } else if rows.is_empty() {
        if app.proc_filter.text.trim().is_empty() && !app.proc_mine { "No processes reported.".into() } else { "No processes match this filter.".into() }
    } else {
        let names = ["PID", "name", "user", "CPU", "GPU", "memory", "disk", "network"];
        let order = if PROCESS_COLUMNS[sort].3 {
            if descending { "highest first" } else { "lowest first" }
        } else if descending {
            "Z–A"
        } else {
            "A–Z"
        };
        let shown = if matching != total { format!(" matching (of {total})") } else { String::new() };
        format!(
            "Showing {} of {matching}{shown} · sorted by {}, {order} · updated {} · sort: click a title or press o / O",
            rows.len(),
            names[sort],
            app.process_updated
        )
    };
    put(buf, status_rect.x, status_rect.y, status_rect.width, &Line::styled(status, Style::new().fg(MUTED)));

    if let Some(p) = app.selected_process() {
        let threads = p["threads"].as_u64().unwrap_or(0);
        let mut lines = vec![Line::from(vec![
            bold(js(&p, "name").unwrap_or("?").to_string(), FG),
            styled(
                format!(" · PID {} · {} · {threads} thread{}", jtext(&p["pid"]), js(&p, "user").unwrap_or("user unknown"), if threads == 1 { "" } else { "s" }),
                SOFT,
            ),
        ])];
        if let Some(command) = js(&p, "command") {
            lines.push(Line::styled(util::cut(command, 240), Style::new().fg(MUTED)));
        }
        let io = if windows { "I/O" } else { "Disk" };
        let tcp = if windows { String::new() } else { format!("   ·   TCP ↓ {} ↑ {}", util::speed(jf(&p, "net_rx")), util::speed(jf(&p, "net_tx"))) };
        lines.push(Line::styled(
            format!(
                "{io} read {} · write {}{tcp}   ·   VRAM {}",
                util::speed(jf(&p, "disk_read")),
                util::speed(jf(&p, "disk_write")),
                util::size(jf(&p, "vram"))
            ),
            Style::new().fg(SOFT),
        ));
        for (i, line) in lines.iter().enumerate() {
            put(buf, detail_rect.x, detail_rect.y + i as u16, detail_rect.width, line);
        }
    }
    let note = if windows {
        "I/O = all reads and writes, network included (Windows has no per-process network counter) · GPU = busiest engine · CPU = share of the whole machine"
    } else {
        "Net TCP = TCP only, no UDP/QUIC (NETWORK above counts all traffic) · CPU = share of the whole machine · — = needs root"
    };
    put(buf, note_rect.x, note_rect.y, note_rect.width, &Line::styled(note, Style::new().fg(DIM)));
}

// ---------- Session ----------

fn session(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) -> u16 {
    let linux = app.node().linux();
    let kind = |k: Kind| if linux { k } else { Kind::Off };
    let mut flow = Flow::new(area);
    flow.gap(1);
    let h = button_row(
        buf,
        hits,
        Rect::new(flow.x, flow.y, flow.width, 3),
        &[
            btn("Peek screen", "k", kind(Kind::Primary), Act::Peek),
            btn("Lock screen…", "l", kind(Kind::Normal), Act::Lock),
            btn("Notify…", "n", kind(Kind::Normal), Act::Notify),
            btn("Open URL…", "u", kind(Kind::Normal), Act::OpenUrl),
        ],
    );
    flow.gap(h + 1);
    let summary = match &app.hypr {
        Some(data) => {
            let monitors = ja(data, "monitors");
            let line1 =
                format!("Hyprland {}   ·   {} monitor(s)   ·   {} windows", jtext(&data["version"]["version"]), monitors.len(), ja(data, "clients").len());
            let line2: Vec<String> = monitors
                .iter()
                .map(|m| format!("{} {}×{} @ {:.0} Hz", jtext(&m["name"]), jtext(&m["width"]), jtext(&m["height"]), jf(m, "refreshRate").unwrap_or(0.0)))
                .collect();
            format!("{line1}\n{}", line2.join("   "))
        }
        None if !linux => "Hyprland controls are available on the Linux machines.".into(),
        None if app.hypr_message.is_empty() => "Reading the Hyprland session…".into(),
        None => app.hypr_message.clone(),
    };
    let rect = flow.take(2);
    Paragraph::new(summary).style(Style::new().fg(SOFT)).wrap(Wrap { trim: true }).render(rect, buf);
    flow.gap(1);
    let ws_focus = app.session_table == TableId::Workspaces;
    let h = button_row(
        buf,
        hits,
        Rect::new(flow.x, flow.y, flow.width, 3),
        &[
            btn("Refresh session", "r", kind(Kind::Primary), Act::HyprRefresh),
            btn("Reload config…", "R", kind(Kind::Normal), Act::HyprReload),
            btn("Go to workspace…", "g", kind(Kind::Normal), Act::GoWorkspace),
            btn(
                if ws_focus { "Go to selected workspace" } else { "Focus selected window" },
                "⏎",
                kind(Kind::Normal),
                if ws_focus { Act::GoWorkspace } else { Act::FocusWindow },
            ),
        ],
    );
    flow.gap(h + 1);
    let workspaces = app.workspaces();
    let windows = app.windows();
    app.ws_cursor = app.ws_cursor.min(workspaces.len().saturating_sub(1));
    app.win_cursor = app.win_cursor.min(windows.len().saturating_sub(1));
    eyebrow(buf, flow.take(1), &format!("WORKSPACES{}", if ws_focus { "  ·  ↑↓ ⏎ go  ·  t windows" } else { "" }));
    let rows: Vec<Vec<Span<'static>>> =
        workspaces.iter().map(|w| vec![Span::raw(jtext(&w["name"])), Span::raw(jtext(&w["monitor"])), Span::raw(jtext(&w["windows"]))]).collect();
    let rect = flow.take((rows.len() as u16 + 1).clamp(2, 9));
    let view = TableView { id: TableId::Workspaces, cursor: (!rows.is_empty()).then_some(app.ws_cursor), focused: ws_focus };
    table(buf, hits, rect, &[col("Workspace", 18, false), col("Monitor", 14, false), col("Windows", 8, true)], &rows, &view, &mut app.ws_offset);
    flow.gap(1);
    eyebrow(buf, flow.take(1), &format!("WINDOWS{}", if ws_focus { "" } else { "  ·  ↑↓ ⏎ focus  ·  t workspaces" }));
    let rows: Vec<Vec<Span<'static>>> =
        windows.iter().map(|w| vec![Span::raw(jtext(&w["class"])), Span::raw(jtext(&w["title"])), Span::raw(jtext(&w["workspace"]["name"]))]).collect();
    let rect = flow.take((rows.len() as u16 + 1).clamp(2, 16));
    let view = TableView { id: TableId::Windows, cursor: (!rows.is_empty()).then_some(app.win_cursor), focused: !ws_focus };
    table(buf, hits, rect, &[col("App", 20, false), col("Window title", 0, false), col("Workspace", 16, false)], &rows, &view, &mut app.win_offset);
    flow.used()
}

// ---------- VPN ----------

fn vpn(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) -> u16 {
    let mut flow = Flow::new(area);
    flow.gap(1);
    eyebrow(buf, flow.take(1), "VPN  ·  every machine, read with the status every 15 s");
    flow.gap(1);
    let rows: Vec<Vec<Span<'static>>> = app
        .nodes
        .iter()
        .map(|n| {
            let s = app.status(n).cloned().unwrap_or(serde_json::Value::Null);
            let on = s["online"] == true;
            let vpn = if on { s["vpn"].clone() } else { serde_json::Value::Null };
            let kind = if !util::truthy(&vpn) {
                "—"
            } else if vpn["kind"] == "tunnel" {
                "torrent tunnel"
            } else {
                "Mullvad"
            };
            let state = if on {
                vpn_short(&vpn)
            } else if s.is_null() {
                "checking".into()
            } else {
                "offline".into()
            };
            vec![
                Span::raw(n.hostname.clone()),
                Span::raw(kind),
                styled(state, vpn_color(&vpn)),
                Span::raw(js(&vpn, "relay").unwrap_or("—").to_string()),
                Span::raw(js(&vpn, "exit_ip").unwrap_or("—").to_string()),
                Span::raw(if util::truthy(&vpn) { util::since(jf(&vpn, "last_drop")) } else { "—".into() }),
            ]
        })
        .collect();
    let rect = flow.take(rows.len() as u16 + 1);
    let view = TableView { id: TableId::Vpn, cursor: Some(app.vpn_cursor), focused: true };
    let cols = [
        col("Machine", 14, false),
        col("VPN", 15, false),
        col("State", 18, false),
        col("Relay", 22, false),
        col("Exit IP", 16, false),
        col("Last drop", 0, false),
    ];
    table(buf, hits, rect, &cols, &rows, &view, &mut app.vpn_offset);
    flow.gap(1);
    let node = app.node().clone();
    let data = app.status(&node).cloned();
    let detail = match &data {
        Some(d) if d["online"] == true => vpn_lines(&node.hostname, &d["vpn"]),
        Some(_) => format!("{} is offline.", node.hostname),
        None => "Checking…".into(),
    };
    let lines: Vec<Line<'static>> =
        detail.split('\n').map(|l| Line::styled(l.to_string(), Style::new().fg(if l.starts_with('!') { BAD } else { SOFT }))).collect();
    let h = lines.len() as u16;
    Paragraph::new(lines).render(flow.take(h), buf);
    flow.gap(1);
    let buttons = if node.linux() {
        let k = |kind| if app.vpn_linux_ok() { kind } else { Kind::Off };
        vec![
            btn("Connect", "o", k(Kind::Primary), Act::VpnConnect),
            btn("Disconnect…", "x", k(Kind::Normal), Act::VpnDisconnect),
            btn("Reconnect", "e", k(Kind::Normal), Act::VpnReconnect),
            btn("Switch relay…", "l", k(Kind::Normal), Act::VpnRelay),
        ]
    } else {
        vec![
            btn("Restart tunnel…", "R", if node.vpn_str("restart").is_some() { Kind::Primary } else { Kind::Off }, Act::VpnRestart),
            btn("Test tunnel", "t", if node.vpn_str("test").is_some() { Kind::Normal } else { Kind::Off }, Act::VpnTest),
        ]
    };
    let h = button_row(buf, hits, Rect::new(flow.x, flow.y, flow.width, 3), &buttons);
    flow.gap(h + 1);
    let rect = flow.take(12);
    app.vpn_log.render(buf, hits, Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), rect.height), "VPN RESULTS", Scroll::VpnLog);
    flow.used()
}

// ---------- Files ----------

fn files(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) -> u16 {
    let mut flow = Flow::new(area);
    flow.gap(1);
    eyebrow(buf, flow.take(1), "TRANSFER A FILE");
    flow.gap(1);
    for line in ["Paths on the selected machine are relative to its home folder unless absolute.", "The local path is on the machine running Archfleet."] {
        put(buf, flow.x, flow.take(1).y, flow.width, &Line::styled(line, Style::new().fg(SOFT)));
    }
    flow.gap(1);
    let field_width = flow.width.min(90);
    put(buf, flow.x, flow.take(1).y, flow.width, &Line::from(vec![styled("Local file", SOFT), styled("   i to edit, Tab switches", DIM)]));
    let r = flow.take(1);
    app.local_path.render(buf, hits, Rect::new(r.x, r.y, field_width, 1), "~/Downloads/example.zip", app.focus == Some(Field::LocalPath), Field::LocalPath);
    flow.gap(1);
    put(buf, flow.x, flow.take(1).y, flow.width, &Line::styled(format!("Path on {}", app.node().hostname), Style::new().fg(SOFT)));
    let r = flow.take(1);
    app.remote_path.render(
        buf,
        hits,
        Rect::new(r.x, r.y, field_width, 1),
        "Downloads/example.zip (Windows also accepts C:/...)",
        app.focus == Some(Field::RemotePath),
        Field::RemotePath,
    );
    flow.gap(1);
    let k = if app.transfer_running { Kind::Off } else { Kind::Normal };
    let h = button_row(
        buf,
        hits,
        Rect::new(flow.x, flow.y, flow.width, 3),
        &[
            btn("Upload →", "u", if app.transfer_running { Kind::Off } else { Kind::Primary }, Act::Upload),
            btn("← Download", "d", k, Act::Download),
            btn("Open SFTP", "f", Kind::Normal, Act::Sftp),
        ],
    );
    flow.gap(h + 1);
    put(buf, flow.x, flow.take(1).y, flow.width, &Line::styled(app.transfer_status.clone(), Style::new().fg(ACCENT)));
    flow.used()
}

// ---------- Clipboard ----------

fn clipboard(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) -> u16 {
    let mut flow = Flow::new(area);
    flow.gap(1);
    let r = flow.take(1);
    let w = button(buf, hits, r.x, r.y, "Refresh all", "a", Kind::Primary, Act::ClipRefreshAll);
    put(
        buf,
        r.x + w + 2,
        r.y,
        r.width.saturating_sub(w + 2),
        &Line::styled("Text and PNG · Copy here copies the snapshot shown · ↑↓ picks the machine the keys act on", Style::new().fg(SOFT)),
    );
    flow.gap(1);
    put(buf, flow.x, flow.take(1).y, flow.width, &Line::styled(app.clip_result.clone(), Style::new().fg(ACCENT)));
    flow.gap(1);
    for (i, node) in app.nodes.iter().enumerate() {
        let selected = i == app.sel;
        let rect = flow.take(13);
        let block = card(&format!("{} / {}", node.label, node.hostname), selected);
        let inner = block.inner(rect).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
        block.render(rect, buf);
        hits.click(rect, Act::Machine(i));
        let value = app.clip_values.get(&node.alias);
        let preview: String = match value {
            Some(v) if v["kind"] == "text" => v["text"].as_str().unwrap_or("").chars().take(20000).collect(),
            Some(v) if v["kind"] == "image" => format!(
                "PNG image · {} × {} pixels\n\nCopy here pastes this image into this machine's clipboard; View image opens it.",
                jtext(&v["width"]),
                jtext(&v["height"])
            ),
            Some(v) if v["kind"] == "empty" => String::new(),
            Some(v) => format!("This clipboard offers: {}", ja(v, "types").iter().map(jtext).collect::<Vec<_>>().join(", ")),
            None => String::new(),
        };
        let text_rect = Rect::new(inner.x, inner.y + 1, inner.width, 7);
        fill(buf, text_rect, Style::new().bg(hex(0x0d1521)));
        let scroll = app.clip_scroll.get(&node.alias).copied().unwrap_or(0);
        let para = Paragraph::new(preview.replace('\t', "    ")).style(Style::new().fg(FG).bg(hex(0x0d1521))).wrap(Wrap { trim: false });
        let lines = para.line_count(text_rect.width) as u16;
        let scroll = scroll.min(lines.saturating_sub(text_rect.height));
        para.scroll((scroll, 0)).render(text_rect, buf);
        hits.scroll(text_rect, Scroll::Clip(i));
        let state = app.clip_state.get(&node.alias).cloned().unwrap_or_else(|| "Not read yet.".into());
        put(buf, inner.x, inner.y + 9, inner.width, &Line::styled(state, Style::new().fg(SOFT)));
        let kind = value.and_then(|v| v["kind"].as_str()).unwrap_or("");
        let can_copy = (kind == "text" || kind == "image") && !app.clip_busy && !node.is_local();
        let key = |k: &'static str| if selected { k } else { "" };
        button_row(
            buf,
            hits,
            Rect::new(inner.x, inner.y + 10, inner.width, 1),
            &[
                btn("Refresh", "", Kind::Normal, Act::ClipRefresh(i)),
                btn("Copy here", key("h"), if can_copy { Kind::Primary } else { Kind::Off }, Act::ClipCopy(i)),
                btn("Send local here", key("l"), if node.is_local() || app.clip_busy { Kind::Off } else { Kind::Normal }, Act::ClipSend(i)),
                btn("View image", key("v"), if kind == "image" { Kind::Normal } else { Kind::Off }, Act::ClipView(i)),
            ],
        );
        flow.gap(1);
    }
    flow.used()
}

// ---------- Run ----------

fn run_tab(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) {
    let mut flow = Flow::new(area);
    flow.gap(1);
    eyebrow(buf, flow.take(1), "RUN ON FLEET  ·  the same command on every ticked machine");
    flow.gap(1);
    let r = flow.take(1);
    let count = app.run_targets.iter().filter(|t| **t).count();
    let label = if app.run_running { "Running…".to_string() } else { format!("Run on {count}") };
    let bw = width(&label) + 2 + 3;
    app.run_command.render(
        buf,
        hits,
        Rect::new(r.x, r.y, r.width.saturating_sub(bw + 2), 1),
        "Command, e.g. uptime   (↑ ↓ earlier commands)",
        app.focus == Some(Field::RunCommand),
        Field::RunCommand,
    );
    button(buf, hits, r.right() - bw, r.y, &label, "⏎", if app.run_running || count == 0 { Kind::Off } else { Kind::Primary }, Act::Run);
    flow.gap(1);
    let r = flow.take(1);
    let mut x = r.x;
    for (i, node) in app.nodes.iter().enumerate() {
        let label = format!("{}{}", node.hostname, if node.linux() { "" } else { " · cmd" });
        let key = if i < 9 { format!("alt+{}", i + 1) } else { String::new() };
        x += checkbox(buf, hits, x, r.y, &label, app.run_targets[i], &key, Act::RunTarget(i)) + 2;
    }
    flow.gap(1);
    let r = flow.take(1);
    if app.run_status.is_empty() {
        put(
            buf,
            r.x,
            r.y,
            r.width,
            &Line::styled("Linux machines run it in your login shell, Windows machines in cmd.exe. Each stops after 60 s.", Style::new().fg(MUTED)),
        );
    } else {
        let mut spans = Vec::new();
        for (alias, label, color) in &app.run_status {
            let name = config::find(&app.nodes, alias).map(|n| n.hostname.clone()).unwrap_or_default();
            spans.push(bold(format!("{name} "), FG));
            spans.push(styled(format!("{label}    "), *color));
        }
        put(buf, r.x, r.y, r.width, &Line::from(spans));
    }
    flow.gap(1);
    let rect = Rect::new(area.x - 1, flow.y, area.width + 2, area.bottom().saturating_sub(flow.y));
    app.run_log.render(buf, hits, rect, "OUTPUT", Scroll::RunLog);
}

// ---------- Maintenance ----------

fn maintenance(buf: &mut Buffer, hits: &mut Hits, app: &mut App, area: Rect) {
    let linux = app.node().linux();
    let k = |kind: Kind| if linux { kind } else { Kind::Off };
    let mut flow = Flow::new(area);
    flow.gap(1);
    let groups: [(&str, Vec<Btn>); 3] = [
        (
            "UPDATES",
            vec![
                btn("Check updates", "u", k(Kind::Primary), Act::Report("updates")),
                btn("Arch news", "n", Kind::Normal, Act::News),
                btn("Upgrade system…", "U", k(Kind::Normal), Act::Upgrade),
            ],
        ),
        (
            "DIAGNOSE",
            vec![
                btn("Failed services", "v", k(Kind::Normal), Act::Report("services")),
                btn("Boot errors", "j", k(Kind::Normal), Act::Report("journal")),
                btn("Disk usage", "d", k(Kind::Normal), Act::Report("storage")),
                btn("Login sessions", "l", k(Kind::Normal), Act::Report("sessions")),
            ],
        ),
        ("FLEET", vec![btn("Compare packages", "k", Kind::Normal, Act::Packages)]),
    ];
    for (label, buttons) in groups.iter() {
        let r = flow.take(1);
        put(buf, r.x, r.y, 11, &Line::styled(*label, Style::new().fg(MUTED).add_modifier(Modifier::BOLD)));
        let h = button_row(buf, hits, Rect::new(r.x + 11, r.y, r.width - 11, 3), buttons);
        if *label == "FLEET" {
            let used = width("Compare packages") + 2 + 3 + 2;
            put(
                buf,
                r.x + 11 + used,
                r.y,
                r.width.saturating_sub(11 + used),
                &Line::styled("Explicitly installed packages, first two Linux machines", Style::new().fg(DIM)),
            );
        }
        flow.gap(h);
    }
    let rect = Rect::new(area.x - 1, flow.y, area.width + 2, area.bottom().saturating_sub(flow.y));
    app.report.render(buf, hits, rect, "RESULTS", Scroll::Report);
}
