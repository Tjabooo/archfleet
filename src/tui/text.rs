//! Styled text for the dashboard: a tiny markup ([b], [#rrggbb], [/]) and the formatted sections
//! (Lilin's sharing report, Screen Time, the VPN detail, the NPU line).
use crate::util::{self, ja, jb, jf, js, jtext};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;

pub const BG: Color = hex(0x0b101a);
pub const FG: Color = hex(0xd9e2f2);
pub const PANEL: Color = hex(0x121b2b);
pub const SIDEBAR: Color = hex(0x101827);
pub const CARD: Color = hex(0x111c2c);
pub const ACCENT: Color = hex(0x79e2c2);
pub const MUTED: Color = hex(0x91a3bf);
pub const DIM: Color = hex(0x596a85);
pub const SOFT: Color = hex(0xa6b7d0);
pub const LAVENDER: Color = hex(0xc7b4f8);
pub const BAD: Color = hex(0xed8796);
pub const WARN: Color = hex(0xf5c26b);
pub const BORDER: Color = hex(0x2a3c55);
pub const BORDER_DIM: Color = hex(0x25334b);
pub const HEAD: Color = hex(0x1a2940);
pub const CURSOR: Color = hex(0x255349);
pub const CURSOR_IDLE: Color = hex(0x1b3532);
pub const CURSOR_FG: Color = hex(0xdcfff1);
pub const BUTTON: Color = hex(0x17253a);
pub const PRIMARY: Color = hex(0x246157);
pub const PRIMARY_FG: Color = hex(0xe2fff6);
pub const SELECTED: Color = hex(0x1d3b3c);
pub const SELECTED_FG: Color = hex(0xb6f4e4);
pub const INPUT: Color = hex(0x121e30);
pub const ZEBRA: Color = hex(0x0e1622);

pub const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn parse_hex(text: &str) -> Option<Color> {
    let digits = text.strip_prefix('#')?;
    (digits.len() == 6).then(|| u32::from_str_radix(digits, 16).ok().map(hex)).flatten()
}

/// Escape text for `markup`: a literal '[' becomes "\[".
pub fn escape(text: &str) -> String {
    text.replace('[', "\\[")
}

/// "[b]bold[/b] [#79e2c2]green[/] plain" -> styled lines. Unknown [...] is kept as text.
pub fn markup(source: &str) -> Text<'static> {
    let mut lines = Vec::new();
    for raw in source.split('\n') {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut stack: Vec<Style> = vec![Style::default()];
        let mut text = String::new();
        let mut chars = raw.char_indices().peekable();
        let flush = |text: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
            if !text.is_empty() {
                spans.push(Span::styled(std::mem::take(text), style));
            }
        };
        while let Some((i, c)) = chars.next() {
            if c == '\\' && raw[i + 1..].starts_with('[') {
                chars.next();
                text.push('[');
                continue;
            }
            if c == '['
                && let Some(end) = raw[i..].find(']')
            {
                let tag = &raw[i + 1..i + end];
                let style = *stack.last().unwrap();
                let new = if tag == "/" || tag == "/b" {
                    Some(None)
                } else {
                    let mut s = style;
                    let mut ok = !tag.is_empty();
                    for word in tag.split_whitespace() {
                        if word == "b" {
                            s = s.add_modifier(Modifier::BOLD);
                        } else if let Some(color) = parse_hex(word) {
                            s = s.fg(color);
                        } else {
                            ok = false;
                        }
                    }
                    ok.then_some(Some(s))
                };
                if let Some(change) = new {
                    flush(&mut text, &mut spans, style);
                    match change {
                        Some(s) => stack.push(s),
                        None if stack.len() > 1 => {
                            stack.pop();
                        }
                        None => {}
                    }
                    for _ in 0..end {
                        chars.next();
                    }
                    continue;
                }
            }
            text.push(c);
        }
        flush(&mut text, &mut spans, *stack.last().unwrap());
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

pub fn c(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#d9e2f2".into(),
    }
}

// ---------- Lilin: what streams and encodes cost the server ----------

fn load_bar(value: Option<f64>, maximum: Option<f64>, width: usize) -> String {
    let Some(maximum) = maximum.filter(|m| *m > 0.0) else { return " ".repeat(width) };
    let value = value.unwrap_or(0.0);
    let percent = 100.0 * value / maximum;
    let least = if value > 0.0 { 1 } else { 0 };
    let filled = ((percent.min(100.0) / 100.0 * width as f64).round() as usize).max(least).min(width);
    format!("[{}]{}[/][#2a3c55]{}[/]", if percent >= 85.0 { c(BAD) } else { c(ACCENT) }, "━".repeat(filled), "─".repeat(width - filled))
}

fn stream_is_slow(stream: &Value, now_ms: f64) -> bool {
    if let Some(behind) = stream.get("behind") {
        return util::truthy(behind);
    }
    let needs = jf(stream, "needs").unwrap_or(0.0);
    needs > 0.0 && !jb(stream, "waiting") && jf(stream, "avg").unwrap_or(0.0) < needs * 0.9 && now_ms - jf(stream, "since").unwrap_or(now_ms) > 60000.0
}

fn where_label(value: &Value) -> String {
    match value.as_str() {
        Some("internet") => "internet".into(),
        Some("lan") => "home network".into(),
        Some("tv") => "TV".into(),
        _ => jtext(value),
    }
}

pub fn sharing_text(load: &Value) -> String {
    let now_ms = util::now_secs() * 1000.0;
    let (muted, bad, good) = (c(MUTED), c(BAD), c(ACCENT));
    let limits = load.get("limits").cloned().unwrap_or(Value::Null);
    let mut lines = vec![format!("[{muted}]Lilin on the server  ·  checked {}[/]", util::clock(jf(load, "at").unwrap_or(0.0)))];
    for w in ja(load, "warnings") {
        lines.push(format!("[{bad}]⚠ {}[/]", escape(&jtext(w))));
    }
    let row = |lines: &mut Vec<String>, name: &str, bar: String, reading: String| lines.push(format!("{name:<13}{bar}  {reading}"));
    let internet = load.get("internet").cloned().unwrap_or(Value::Null);
    let up = jf(&internet, "streams").unwrap_or(0.0) + jf(&internet, "torrents").unwrap_or(0.0);
    let cap = jf(&limits, "uploadMbps").filter(|c| *c > 0.0);
    let mut parts = vec![format!("streams {}", util::mbits(jf(&internet, "streams")))];
    if jf(&internet, "torrents").is_some() {
        parts.push(format!("torrents {}", util::mbits(jf(&internet, "torrents"))));
    }
    parts.push(format!("home network {}", util::mbits(load.get("lan").and_then(|l| jf(l, "streams")))));
    let cap_text = cap.map(|c| format!(" / {}", jtext(&serde_json::json!(c)))).unwrap_or_default();
    row(
        &mut lines,
        "Internet up",
        load_bar(Some(up), cap.map(|c| c * 1e6 / 8.0), 14),
        format!("{}{cap_text} Mbit/s  [{muted}]{}[/]", util::mbits(Some(up)), parts.join(" · ")),
    );
    if let Some(net) = load.get("net").filter(|n| util::truthy(n)) {
        let link = jf(&limits, "linkMbps").filter(|l| *l > 0.0);
        row(
            &mut lines,
            "Network card",
            load_bar(jf(net, "up"), link.map(|l| l * 1e6 / 8.0), 14),
            format!("↑ {}  ↓ {}{} Mbit/s", util::mbits(jf(net, "up")), util::mbits(jf(net, "down")), link.map(|l| format!(" / {l:.0}")).unwrap_or_default()),
        );
    }
    if let Some(cpu) = jf(load, "cpu") {
        let own = load.get("lilinCpu").filter(|v| !v.is_null()).map(|v| format!("  [{muted}]Lilin {}%[/]", jtext(v))).unwrap_or_default();
        row(&mut lines, "CPU", load_bar(Some(cpu), Some(100.0), 14), format!("{}%{own}", jtext(&load["cpu"])));
    }
    if let Some(gpu) = load.get("gpu").filter(|g| util::truthy(g)) {
        row(
            &mut lines,
            "GPU encoder",
            load_bar(jf(gpu, "enc"), Some(100.0), 14),
            format!(
                "{}%  [{muted}]{} of {} NVENC encodes · GPU {}% · {}[/]",
                jf(gpu, "enc").map(|v| jtext(&serde_json::json!(v))).unwrap_or("0".into()),
                jf(gpu, "sessions").map(|v| jtext(&serde_json::json!(v))).unwrap_or("0".into()),
                limits.get("nvencSessions").map(jtext).unwrap_or("8".into()),
                jf(gpu, "busy").map(|v| jtext(&serde_json::json!(v))).unwrap_or("0".into()),
                util::celsius(jf(gpu, "temp"))
            ),
        );
    }
    let mut disks: Vec<&Value> = ja(load, "disks").iter().filter(|d| js(d, "label").is_some()).collect();
    disks.sort_by_key(|d| js(d, "label") != Some("Media"));
    for disk in disks {
        row(
            &mut lines,
            &format!("{} {}:", jtext(&disk["label"]), jtext(&disk["drive"])),
            load_bar(jf(disk, "busy"), Some(100.0), 14),
            format!("{}% busy  [{muted}]reads {} · writes {}[/]", jtext(&disk["busy"]), util::speed(jf(disk, "read")), util::speed(jf(disk, "write"))),
        );
    }
    if !jb(load, "sampled") {
        lines.push(format!("[{muted}]Machine readings are starting up; streams and jobs are counted already.[/]"));
    }

    let streams = ja(load, "streams");
    lines.push(String::new());
    lines.push(if streams.is_empty() {
        format!("[b]STREAMING NOW[/b]  [{muted}]nothing is being watched[/]")
    } else {
        format!("[b]STREAMING NOW[/b]  {}", streams.len())
    });
    for s in streams {
        let slow = stream_is_slow(s, now_ms);
        let place = if s["where"] == "internet" && jb(s, "fromHome") { "internet (from home, via Cloudflare)".into() } else { where_label(&s["where"]) };
        let who: Vec<String> = [js(s, "who"), js(s, "device")].into_iter().flatten().map(str::to_string).collect();
        let mut info = vec![
            escape(&who.join(" · ")),
            escape(&place),
            if jb(s, "waiting") { "buffered ahead".into() } else { format!("{} now", util::mbits(jf(s, "rate"))) },
        ];
        if jb(s, "speed") {
            info.push(format!("arrives at {}", util::mbits(jf(s, "speed"))));
        }
        info.push(format!("avg {}", util::mbits(jf(s, "avg"))));
        if let Some(needs) = jf(s, "needs").filter(|n| *n > 0.0) {
            info.push(if slow { format!("[{bad}]file needs {}[/]", util::mbits(Some(needs))) } else { format!("needs {}", util::mbits(Some(needs))) });
        }
        info.push(format!("{} · {}", util::size_text(jf(s, "sent")), util::lasted(now_ms - jf(s, "since").unwrap_or(now_ms))));
        lines.push(format!("[{}]●[/] {}  [{muted}]{}[/]", if slow { &bad } else { &good }, escape(js(s, "label").unwrap_or("")), info.join(" · ")));
    }

    let jobs = ja(load, "jobs");
    lines.push(if jobs.is_empty() {
        format!("[b]ENCODING NOW[/b]  [{muted}]no clip copies, conversions or previews[/]")
    } else {
        format!("[b]ENCODING NOW[/b]  {}", jobs.len())
    });
    for j in jobs {
        let mut info: Vec<String> = Vec::new();
        if let Some(e) = js(j, "encoder") {
            info.push(e.into());
        }
        info.push(j.get("cpu").filter(|v| !v.is_null()).map(|v| format!("CPU {}%", jtext(v))).unwrap_or("CPU …".into()));
        if jb(j, "mem") {
            info.push(format!("RAM {}", util::size(jf(j, "mem"))));
        }
        if let Some(p) = j.get("percent").filter(|v| !v.is_null()) {
            info.push(format!("{}%{}", jtext(p), js(j, "speed").map(|s| format!(" · {s}")).unwrap_or_default()));
        }
        info.push(format!("running {}", util::lasted(now_ms - jf(j, "since").unwrap_or(now_ms))));
        lines.push(format!(
            "[{good}]●[/] {} · {}  [{muted}]{}[/]",
            escape(js(j, "kind").unwrap_or("")),
            escape(js(j, "label").unwrap_or("")),
            escape(&info.join(" · "))
        ));
    }

    let top = ja(load, "top");
    if !top.is_empty() {
        let busiest: Vec<String> = top.iter().map(|p| format!("{} {}%", jtext(&p["name"]), jtext(&p["cpu"]))).collect();
        lines.push(format!("[b]BUSIEST[/b]  [{muted}]{}[/]", escape(&busiest.join(" · "))));
    }
    let recent = ja(load, "recent");
    if !recent.is_empty() {
        lines.push("[b]RECENTLY[/b]".into());
        for r in recent.iter().take(4) {
            let when = format!("{}–{}", util::clock(jf(r, "start").unwrap_or(0.0)), util::clock(jf(r, "end").unwrap_or(0.0)));
            if r["type"] == "job" {
                let mut extra: Vec<String> = Vec::new();
                if let Some(e) = js(r, "encoder") {
                    extra.push(e.into());
                }
                if let Some(cpu) = r.get("cpu").filter(|v| !v.is_null()) {
                    extra.push(format!("CPU avg {}%", jtext(cpu)));
                }
                if r.get("ok") == Some(&Value::Bool(false)) {
                    extra.push("FAILED".into());
                }
                lines.push(format!(
                    "  [{muted}]{when}[/]  {} · {}  [{muted}]{}[/]",
                    escape(js(r, "kind").unwrap_or("")),
                    escape(js(r, "label").unwrap_or("")),
                    escape(&extra.join(" · "))
                ));
            } else {
                let extra = format!(
                    "{} · {} · avg {}, peak {} Mbit/s",
                    jtext(&r["who"]),
                    where_label(&r["where"]),
                    util::mbits(jf(r, "avg")),
                    util::mbits(jf(r, "peak"))
                );
                lines.push(format!("  [{muted}]{when}[/]  {}  [{muted}]{}[/]", escape(js(r, "label").unwrap_or("")), escape(&extra)));
            }
        }
    }
    lines.join("\n")
}

// ---------- Screen Time ----------

fn category_color(category: &str) -> String {
    let color = match category {
        "Social" => 0xe2668a,
        "Games" => 0xee8a4a,
        "Entertainment" => 0x9b7fe8,
        "Web" => 0x4f92e3,
        "Development" => 0x2fb3a4,
        "Productivity" => 0xe0ae37,
        "Creativity" => 0x5dbb6e,
        "Education" => 0xc8955a,
        "Utilities" => 0x8e98ab,
        _ => 0x91a3bf,
    };
    c(hex(color))
}

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn screentime_text(data: &Value) -> String {
    let (muted, bad) = (c(MUTED), c(BAD));
    let mut lines = Vec::new();
    let daemon = data.get("daemon").cloned().unwrap_or(Value::Null);
    if util::truthy(&daemon) && !jb(&daemon, "running") {
        lines.push(format!("[{bad}]The screentime daemon isn’t running (systemctl --user start screentime): numbers stop at its last write.[/]"));
    }
    let mut head = vec![format!("[b]{}[/b] today", util::st_duration(jf(data, "total")))];
    if let Some(pickups) = data.get("pickups").and_then(Value::as_u64) {
        let first = js(data, "first_pickup").map(|f| format!(", first at {f}")).unwrap_or_default();
        head.push(format!("{pickups} pickup{}{first}", if pickups == 1 { "" } else { "s" }));
    }
    if let Some(longest) = data.get("longest_session").filter(|l| util::truthy(l)) {
        head.push(format!("longest {} ({}–{})", util::st_duration(jf(longest, "seconds")), jtext(&longest["start"]), jtext(&longest["end"])));
    }
    let apps = ja(data, "apps");
    if jb(&daemon, "running")
        && let Some(state) = js(&daemon, "state")
    {
        let app = apps.iter().find(|a| a["app"] == daemon["app"]).and_then(|a| js(a, "name")).or_else(|| js(&daemon, "app"));
        let tail = match app {
            Some(app) if state == "active" => format!(" in {}", escape(app)),
            _ => String::new(),
        };
        head.push(format!("now {}{tail}", escape(state)));
    }
    lines.push(head.join("  ·  "));
    let hourly = ja(data, "hourly");
    let top = hourly.iter().filter_map(|h| jf(h, "seconds")).fold(0.0, f64::max);
    if top > 0.0 {
        let mut cols = String::new();
        for hour in hourly {
            let seconds = jf(hour, "seconds").unwrap_or(0.0);
            if seconds <= 0.0 {
                cols.push_str("  ");
                continue;
            }
            let lead = hour
                .get("categories")
                .and_then(Value::as_object)
                .and_then(|cats| cats.iter().max_by(|a, b| a.1.as_f64().unwrap_or(0.0).total_cmp(&b.1.as_f64().unwrap_or(0.0))).map(|(k, _)| k.clone()))
                .unwrap_or_else(|| "Other".into());
            let block = BLOCKS[((8.0 * seconds / top).round() as usize).clamp(1, 8)];
            cols.push_str(&format!("[{}]{block}{block}[/]", category_color(&lead)));
        }
        lines.push(cols);
        lines.push(format!("[{muted}]{}[/]", [0, 6, 12, 18].iter().map(|h| format!("{h:02}{}", " ".repeat(10))).collect::<String>()));
    }
    if apps.is_empty() {
        lines.push(format!("[{muted}]Nothing used yet today.[/]"));
    } else {
        let best = jf(&apps[0], "seconds").filter(|b| *b > 0.0).unwrap_or(1.0);
        for app in apps.iter().take(5) {
            let seconds = jf(app, "seconds").unwrap_or(0.0);
            let filled = ((16.0 * seconds / best).round() as usize).clamp(1, 16);
            let category = js(app, "category").unwrap_or("Other");
            let name = util::cut(js(app, "name").unwrap_or(""), 18);
            lines.push(format!(
                "{:<18} {:>7}  [{}]{}[/][#2a3c55]{}[/]  [{muted}]{}[/]",
                escape(&name),
                util::st_duration(Some(seconds)),
                category_color(category),
                "━".repeat(filled),
                "─".repeat(16 - filled),
                escape(category)
            ));
        }
    }
    let categories = ja(data, "categories");
    if !categories.is_empty() {
        let parts: Vec<String> = categories
            .iter()
            .take(4)
            .map(|cat| {
                let name = js(cat, "name").unwrap_or("Other");
                format!("[{}]●[/] {} {}", category_color(name), escape(name), util::st_duration(jf(cat, "seconds")))
            })
            .collect();
        lines.push(parts.join("   "));
    }
    lines.join("\n")
}

// ---------- VPN and NPU ----------

/// One fleet-table cell: 'on · Stockholm', 'off', 'BLOCKED', '—' (no VPN).
pub fn vpn_short(vpn: &Value) -> String {
    if !util::truthy(vpn) {
        return "—".into();
    }
    let state = vpn["state"].as_str().unwrap_or("");
    let text = match state {
        "connected" => format!("on · {}", js(vpn, "city").or_else(|| js(vpn, "relay")).unwrap_or("?")),
        "error" => "BLOCKED".into(),
        "connecting" | "disconnecting" => format!("{state}…"),
        _ if vpn["kind"] == "tunnel" => "DOWN".into(),
        _ if jb(vpn, "lockdown") => "off · lockdown".into(),
        _ => "off".into(),
    };
    if ja(vpn, "problems").is_empty() { text } else { text + " !" }
}

pub fn vpn_color(vpn: &Value) -> Color {
    if !util::truthy(vpn) {
        MUTED
    } else if vpn.get("ok") == Some(&Value::Bool(false)) {
        BAD
    } else if vpn["state"] == "connected" {
        ACCENT
    } else {
        MUTED
    }
}

pub fn vpn_lines(hostname: &str, vpn: &Value) -> String {
    if !util::truthy(vpn) {
        return format!("{hostname}: no VPN client reported.");
    }
    let kind = if vpn["kind"] == "tunnel" { "torrent split tunnel (only the torrent client uses it)" } else { "Mullvad app" };
    let mut second = format!("state {}", jtext(&vpn["state"]));
    if let Some(relay) = js(vpn, "relay") {
        second += &format!("  ·  relay {relay}");
    }
    if let Some(ip) = js(vpn, "exit_ip") {
        second += &format!("  ·  exit {ip}");
    }
    if let Some(city) = js(vpn, "city") {
        second += &format!(" ({city})");
    }
    let mut extra = Vec::new();
    if let Some(lockdown) = vpn.get("lockdown").and_then(Value::as_bool) {
        extra.push(format!("lockdown {}", if lockdown { "on" } else { "off" }));
    }
    let drop = jf(vpn, "last_drop").filter(|d| *d > 0.0);
    extra.push(format!("last drop {}", if drop.is_some() { util::since(drop) } else { "none recorded".into() }));
    if let Some(leak) = jf(vpn, "last_leak").filter(|l| *l > 0.0) {
        extra.push(format!("last leak alarm {}", util::since(Some(leak))));
    }
    let mut lines = vec![format!("{hostname}  ·  {kind}"), second, extra.join("  ·  ")];
    lines.extend(ja(vpn, "problems").iter().map(|p| format!("! {}", jtext(p))));
    lines.join("\n")
}

/// One short line for the GPU card: a dot coloured by state, then what the NPU (npukit) holds.
pub fn npu_line(npu: &Value) -> String {
    if !util::truthy(npu) {
        return String::new();
    }
    let place = js(npu, "runs_on").unwrap_or("NPU");
    let state = js(npu, "state").unwrap_or("?");
    let activity = js(npu, "activity");
    let what = match activity {
        Some(a) => match a {
            "rec" => "REC".to_string(),
            "stt" => "speech".into(),
            "ocr" => "OCR".into(),
            "index" => "indexing".into(),
            other => other.into(),
        },
        None => {
            let comps: Vec<&str> = ja(npu, "components").iter().filter_map(Value::as_str).collect();
            let mut parts: Vec<String> = Vec::new();
            if comps.contains(&"chat") {
                parts.push(js(npu, "chat_model").unwrap_or("").replace(':', " "));
            }
            for comp in &comps {
                match *comp {
                    "asr" => parts.push("STT".into()),
                    "embed" => parts.push("EMB".into()),
                    _ => {}
                }
            }
            parts.join("+")
        }
    };
    if state == "off" && activity.is_none() {
        return format!("[#596a85]{place} idle[/]");
    }
    let color = if state == "error" || activity == Some("rec") {
        c(BAD)
    } else if state == "ready" && activity.is_none() {
        c(ACCENT)
    } else {
        c(WARN)
    };
    format!("{place} [{color}]●[/] {}", if what.is_empty() { escape(state) } else { escape(&what) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain(text: &Text) -> String {
        text.lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn markup_styles() {
        let t = markup("[b]A[/b] [#79e2c2]g[/] \\[x] [nope]\nz");
        assert_eq!(plain(&t), "A g [x] [nope]\nz");
        assert!(t.lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.lines[0].spans[2].style.fg, Some(ACCENT));
    }

    #[test]
    fn vpn_cells() {
        assert_eq!(vpn_short(&Value::Null), "—");
        assert_eq!(vpn_short(&json!({"state": "connected", "city": "Stockholm"})), "on · Stockholm");
        assert_eq!(vpn_short(&json!({"state": "down", "kind": "tunnel", "problems": ["x"]})), "DOWN !");
        assert!(vpn_lines("m", &json!({"state": "connected", "kind": "mullvad", "lockdown": true})).contains("lockdown on"));
    }

    #[test]
    fn npu() {
        assert_eq!(npu_line(&Value::Null), "");
        assert!(npu_line(&json!({"state": "off"})).contains("NPU idle"));
        let line = npu_line(&json!({"state": "ready", "components": ["chat", "asr"], "chat_model": "qwen:4b", "runs_on": "GPU"}));
        assert!(line.starts_with("GPU ") && line.contains("qwen 4b+STT"));
    }

    #[test]
    fn sharing_and_screentime_render() {
        let load = json!({"at": 0, "sampled": true, "limits": {"uploadMbps": 100}, "internet": {"streams": 1e6},
                          "streams": [{"label": "Film [4K]", "who": "o", "where": "lan", "avg": 1e6, "since": 0}], "jobs": []});
        let text = plain(&markup(&sharing_text(&load)));
        assert!(text.contains("Film [4K]") && text.contains("STREAMING NOW  1"));
        let st = json!({"total": 3600, "pickups": 2, "apps": [{"name": "kitty", "seconds": 3600, "category": "Development", "app": "kitty"}],
                        "hourly": [{"seconds": 10, "categories": {"Web": 10}}]});
        assert!(plain(&markup(&screentime_text(&st))).contains("1h 00m today"));
    }
}
