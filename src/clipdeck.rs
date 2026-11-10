//! clipdeck: the SUPER+V clipboard picker, for every machine on the home network.
//!
//!   clipdeck show        open the overlay on the focused monitor and fill it in
//!   clipdeck pick KEY    copy that entry (on the page shown) to this machine's clipboard, then close
//!   clipdeck page DIR    flip every column a page: next/prev (or down/up, eww's scroll), first, last
//!   clipdeck hide        close it
//!   clipdeck stream      JSON for the eww overlay (eww runs this itself)
//!   clipdeck forget      delete the cached copies of the other machines' history
//!
//! One column per machine: this one's cliphist history first, then the other Linux machines' history
//! and Windows clipboard history (its current clipboard as a fallback). What's on screen is kept in
//! $XDG_RUNTIME_DIR/clipdeck.json (RAM, this user only) and deleted on close; clipboard contents
//! never go on a command line. Each other machine's last answer is cached in ~/.cache/clipdeck/
//! (this user only) and shown at once while the machine is asked again, or when it's off.
use crate::backend;
use crate::clipboards::{self, PNG_SIGNATURE, TEXT_CAP};
use crate::config::{self, Node};
use crate::util::{self, Res, args, secs};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

const HELP: &str = "clipdeck: the SUPER+V clipboard picker

  clipdeck show        open the overlay on the focused monitor and fill it in
  clipdeck pick KEY    copy that entry (on the page shown) to this machine's clipboard, then close
  clipdeck page DIR    flip every column a page: next, prev, first, last (or down/up)
  clipdeck hide        close it
  clipdeck stream      JSON for the eww overlay (eww runs this itself)
  clipdeck forget      delete the cached copies of the other machines' history

CLIPDECK_HISTORY (default 100) entries per column, CLIPDECK_CACHE=0 turns the offline cache off,
CLIPDECK_EWW picks the eww config folder.";

const KEY_ROWS: [&str; 3] = ["1234567890", "QWERTYUIOP", "ASDFGHJKL"];
const PER_COLUMN: usize = 10;

fn history_size() -> usize {
    std::env::var("CLIPDECK_HISTORY").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).clamp(PER_COLUMN, 1000)
}
fn caching() -> bool {
    std::env::var("CLIPDECK_CACHE").map_or(true, |v| v != "0")
}
fn state_path() -> PathBuf {
    util::runtime_dir().join("clipdeck.json")
}
fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| util::home().join(".cache")).join("clipdeck")
}
fn closed() -> Value {
    json!({"open": false, "page": 0, "cols": []})
}

/// Your own eww config when one defines a `clipdeck` window (so it wears your palette), else the
/// one installed next to archfleet.
fn eww_config() -> PathBuf {
    if let Some(folder) = std::env::var_os("CLIPDECK_EWW") {
        return util::expand(&folder.to_string_lossy());
    }
    let conf = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| util::home().join(".config"));
    let mut candidates = vec![conf.join("eww")];
    if let Ok(dir) = std::fs::read_dir(&conf) {
        let mut more: Vec<PathBuf> = dir.flatten().map(|e| e.path().join("eww")).collect();
        more.sort();
        candidates.extend(more);
    }
    for folder in candidates {
        if std::fs::read_to_string(folder.join("eww.yuck")).is_ok_and(|t| t.contains("defwindow clipdeck")) {
            return folder;
        }
    }
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    exe.and_then(|p| p.parent().map(|d| d.join("clipdeck").join("eww")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| util::home().join(".local/share/archfleet/clipdeck/eww"))
}

// ---------- what the overlay shows ----------

/// eww reads "\n" in a label as a line break, so backslashes become a look-alike.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").replace('\\', "⧵")
}

/// cliphist's one-line preview -> (label, is_image).
fn describe(preview: &str) -> (String, bool) {
    if let Some(inner) = preview.strip_prefix("[[ binary data ").and_then(|p| p.strip_suffix(" ]]")) {
        // "<size> <fmt> <w>x<h>", where size itself has a space ("12 KiB")
        let words: Vec<&str> = inner.split(' ').collect();
        if words.len() >= 3
            && let Some((w, h)) = words[words.len() - 1].split_once('x')
            && w.chars().all(|c| c.is_ascii_digit())
            && h.chars().all(|c| c.is_ascii_digit())
        {
            let format = words[words.len() - 2].to_uppercase();
            let size = words[..words.len() - 2].join(" ");
            return (format!("▣ {format} {w}×{h} · {size}"), true);
        }
    }
    if preview.starts_with("[[ binary data") {
        return (format!("▣ {}", preview.trim_matches(|c| c == '[' || c == ']' || c == ' ')), true);
    }
    let label = one_line(preview);
    (if label.is_empty() { "(blank space)".into() } else { label }, false)
}

fn column(node: &Node, index: usize, local: bool) -> Value {
    let kind = if local {
        "THIS MACHINE"
    } else if !node.linux() {
        "WINDOWS · HISTORY"
    } else {
        "ARCH"
    };
    json!({"alias": node.alias, "name": node.hostname.to_uppercase(), "kind": kind, "base_kind": kind, "state": "loading",
           "note": format!("reaching {}…", node.hostname), "items": [], "keys": KEY_ROWS[index]})
}

/// All fetched entries go in `items`; the key chips are handed out per page (see page_of).
fn fill_history(col: &mut Value, entries: &[Value]) {
    let items: Vec<Value> = entries
        .iter()
        .take(history_size())
        .map(|entry| {
            let (label, image) =
                if entry["kind"] == "image" { ("▣ Windows image".to_string(), true) } else { describe(entry["preview"].as_str().unwrap_or("")) };
            let mut item = json!({"id": entry["id"], "label": label, "img": image});
            if let Some(text) = entry["text"].as_str() {
                item["text"] = json!(text); // full text: copies without asking the machine again
            }
            item
        })
        .collect();
    let empty = items.is_empty();
    col["items"] = json!(items);
    col["state"] = json!(if empty { "empty" } else { "ok" });
    col["note"] = json!(if empty { "no clipboard history" } else { "" });
}

fn fill_current(col: &mut Value, value: &Value) {
    col["items"] = json!([]);
    let text = value["text"].as_str().unwrap_or("");
    let (label, image) = match value["kind"].as_str() {
        Some("text") if !text.is_empty() => {
            let label = util::cut(&one_line(&util::cut(text, 600)), 300);
            (if label.is_empty() { "(blank space)".into() } else { label }, false)
        }
        Some("image") => {
            (format!("▣ PNG {}×{} · {} KiB", util::jtext(&value["width"]), util::jtext(&value["height"]), value["bytes"].as_u64().unwrap_or(0) / 1024), true)
        }
        kind => {
            col["state"] = json!("empty");
            col["note"] = json!(if matches!(kind, Some("empty") | Some("text")) { "clipboard is empty" } else { "only formats it can’t copy" });
            return;
        }
    };
    let mut item = json!({"label": label, "img": image});
    if !image && text.len() <= TEXT_CAP {
        item["text"] = json!(text);
    }
    col["items"] = json!([item]);
    col["state"] = json!("ok");
    col["note"] = json!("");
}

fn fetch_column(col: &mut Value, node: &Node) -> Res<()> {
    let limit = history_size();
    if node.linux() {
        let entries = backend::call(node, "clipboard-history", json!({"limit": limit, "text": true}))?;
        fill_history(col, entries.as_array().map(Vec::as_slice).unwrap_or(&[]));
        return Ok(());
    }
    let history = backend::call(node, "clipboard-history", json!(limit)).unwrap_or(json!({"status": "unavailable", "entries": []}));
    let entries = util::ja(&history, "entries");
    if !entries.is_empty() {
        fill_history(col, entries);
        return Ok(());
    }
    fill_current(col, &backend::clipboard(node)?);
    col["kind"] = json!("WINDOWS · CURRENT");
    let reason = match history["status"].as_str() {
        Some("ok") => "history is empty",
        Some("disabled") => "history is disabled",
        _ => "history is unavailable",
    };
    let has_items = !util::ja(col, "items").is_empty();
    let note = col["note"].as_str().unwrap_or("").to_string();
    col["note"] = json!(if has_items { format!("{reason} · showing current clipboard") } else { format!("{reason} · {note}") });
    Ok(())
}

fn offline(col: &mut Value, error: &str) {
    let lower = error.to_lowercase();
    let asleep = lower.contains("timed out") || lower.contains("no route") || lower.contains("did not answer");
    col["state"] = json!("off");
    col["note"] = json!(if asleep {
        "offline · asleep?".to_string()
    } else if error.is_empty() {
        "unreachable".into()
    } else {
        util::cut(error, 80)
    });
    if col["cached"] == true && !util::ja(col, "items").is_empty() {
        col["note"] =
            json!(format!("{} · showing its history from {}", if asleep { "offline" } else { "unreachable" }, ago(col["saved"].as_f64().unwrap_or(0.0))));
    }
}

fn ago(saved: f64) -> String {
    let seconds = (util::now_secs() - saved).max(0.0);
    if seconds < 90.0 {
        "just now".into()
    } else if seconds < 5400.0 {
        format!("{:.0} min ago", seconds / 60.0)
    } else if seconds < 36.0 * 3600.0 {
        format!("{:.0} h ago", seconds / 3600.0)
    } else {
        format!("{:.0} d ago", seconds / 86400.0)
    }
}

// ---------- the other machines' last answer, for when they're off ----------

fn cache_file(alias: &str) -> PathBuf {
    let safe: String = alias.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    cache_dir().join(format!("{safe}.json"))
}

fn store_cache(col: &Value) {
    if !caching() || !matches!(col["state"].as_str(), Some("ok") | Some("empty")) {
        return;
    }
    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let data = json!({"saved": util::now_secs(), "col": {"kind": col["kind"], "state": col["state"], "note": col["note"], "items": col["items"]}});
    let _ = util::write_private(&cache_file(col["alias"].as_str().unwrap_or("")), data.to_string().as_bytes());
}

/// Show the machine's cached history while it's asked again.
fn prefill(col: &mut Value) {
    if !caching() {
        return;
    }
    let Some(cached) = std::fs::read(cache_file(col["alias"].as_str().unwrap_or(""))).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
        return;
    };
    let (Some(saved), data) = (cached["saved"].as_f64(), &cached["col"]) else { return };
    if util::ja(data, "items").is_empty() {
        return;
    }
    if data["kind"].is_string() {
        col["kind"] = data["kind"].clone();
    }
    col["items"] = data["items"].clone();
    col["state"] = json!("cached");
    col["cached"] = json!(true);
    col["saved"] = json!(saved);
    col["note"] = json!(format!("updating · showing its history from {}", ago(saved)));
}

fn forget() {
    if let Ok(dir) = std::fs::read_dir(cache_dir()) {
        for entry in dir.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// ---------- state shared with `pick`, `page` and the eww stream ----------

fn save(state: &Value) {
    let _ = util::write_private(&state_path(), state.to_string().as_bytes());
}

fn load() -> Value {
    std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(closed)
}

/// Read-modify-write of the state file, one process at a time.
fn changing(f: impl FnOnce(&mut Value)) {
    use std::os::unix::io::AsRawFd;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(util::runtime_dir().join("clipdeck.lock"));
    let Ok(lock) = lock else { return };
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let mut state = load();
    f(&mut state);
    drop(lock); // closing drops the lock
}

fn pages(state: &Value) -> usize {
    util::ja(state, "cols").iter().map(|c| util::ja(c, "items").len().div_ceil(PER_COLUMN)).max().unwrap_or(0).max(1)
}

fn current_page(state: &Value) -> usize {
    (state["page"].as_u64().unwrap_or(0) as usize).min(pages(state) - 1)
}

/// The column's entries on that page, each with its key chip.
fn page_of(col: &Value, page: usize) -> Vec<Value> {
    let keys: Vec<char> = col["keys"].as_str().unwrap_or("").chars().collect();
    util::ja(col, "items")
        .iter()
        .skip(page * PER_COLUMN)
        .take(PER_COLUMN)
        .zip(keys)
        .map(|(item, key)| {
            let mut item = item.clone();
            item["key"] = json!(key.to_string());
            item
        })
        .collect()
}

/// Only what the overlay draws (no ids, no full text).
fn for_eww(state: &Value) -> Value {
    let total = pages(state);
    let page = current_page(state);
    let cols: Vec<Value> = util::ja(state, "cols")
        .iter()
        .map(|c| {
            let shown = page_of(c, page);
            let count = util::ja(c, "items").len();
            let mut note = c["note"].as_str().unwrap_or("").to_string();
            if note.is_empty() && shown.is_empty() {
                note = format!("end of history · {count} entries");
            }
            let span = if !shown.is_empty() && total > 1 {
                format!("{}–{} / {count}", page * PER_COLUMN + 1, page * PER_COLUMN + shown.len())
            } else {
                String::new()
            };
            let items: Vec<Value> = shown.iter().map(|it| json!({"key": it["key"], "label": it["label"], "img": it["img"]})).collect();
            json!({"name": c["name"], "kind": c["kind"], "state": c["state"], "note": note, "span": span, "items": items})
        })
        .collect();
    json!({"open": state["open"] == true, "page": page + 1, "pages": total, "cols": cols})
}

/// Print the overlay's JSON whenever the state file changes: inotify, so it sleeps in between.
fn stream() -> i32 {
    use std::io::Write;
    let dir = util::runtime_dir();
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd >= 0 {
        let path = std::ffi::CString::new(dir.to_string_lossy().as_bytes()).unwrap();
        unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE) };
    }
    let mut last = String::new();
    let mut buf = [0u8; 4096];
    loop {
        let state = if state_path().exists() { load() } else { closed() };
        let line = for_eww(&state).to_string();
        if line != last {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return 0; // eww went away
            }
            last = line;
        }
        if fd < 0 {
            std::thread::sleep(secs(0.2));
            continue;
        }
        // wait until something in the runtime folder changes that concerns clipdeck.json
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                std::thread::sleep(secs(0.2));
                break;
            }
            let mut offset = 0usize;
            let mut relevant = false;
            while offset + 16 <= n as usize {
                let len = u32::from_ne_bytes(buf[offset + 12..offset + 16].try_into().unwrap()) as usize;
                let name = &buf[offset + 16..(offset + 16 + len).min(n as usize)];
                let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
                relevant |= name == b"clipdeck.json";
                offset += 16 + len;
            }
            if relevant {
                break;
            }
        }
    }
}

// ---------- opening and closing ----------

fn sh(argv: &[&str], timeout: f64) -> (i32, String) {
    match util::run(&args(argv), None, secs(timeout), None) {
        Ok(out) => (out.code.unwrap_or(1), out.both()),
        Err(error) => (1, error),
    }
}

fn eww(extra: &[&str]) -> (i32, String) {
    let config = eww_config().to_string_lossy().to_string();
    let mut argv = vec!["eww", "-c", &config];
    argv.extend_from_slice(extra);
    sh(&argv, 10.0)
}

/// Hyprland's Lua config takes a Lua expression; a classic hyprland.conf a plain dispatcher.
fn submap(name: &str) {
    let (rc, out) = sh(&["hyprctl", "dispatch", &format!("hl.dsp.submap(\"{name}\")")], 10.0);
    let out = out.trim().to_lowercase();
    if rc != 0 || !(out.is_empty() || out == "ok") {
        sh(&["hyprctl", "dispatch", "submap", name], 10.0);
    }
}

fn notify(title: &str, body: &str) {
    sh(&["notify-send", "-a", "clipdeck", "-t", "2500", title, body], 10.0);
}

fn focused_monitor() -> Vec<String> {
    let (_, out) = sh(&["hyprctl", "monitors", "-j"], 10.0);
    let monitors: Vec<Value> = serde_json::from_str(&out).unwrap_or_default();
    monitors
        .iter()
        .find(|m| m["focused"] == true)
        .map(|m| [&m["model"], &m["name"]].iter().filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)).collect())
        .unwrap_or_default()
}

fn show() -> i32 {
    let nodes = config::load_nodes().unwrap_or_default();
    let Some(local) = nodes.iter().find(|n| n.is_local()) else {
        notify("clipdeck", "This machine isn’t in ~/.config/archfleet/hosts.json (match \"hostname\" to its hostname).");
        return 1;
    };
    let mut order: Vec<&Node> = vec![local];
    order.extend(nodes.iter().filter(|n| !n.is_local()));
    order.truncate(KEY_ROWS.len());
    let mut cols: Vec<Value> = order.iter().enumerate().map(|(i, n)| column(n, i, i == 0)).collect();
    for col in cols.iter_mut().skip(1) {
        prefill(col);
    }
    match clipboards::cliphist_history(history_size(), false) {
        Ok(entries) => fill_history(&mut cols[0], &entries),
        Err(error) => offline(&mut cols[0], &error),
    }
    save(&json!({"open": true, "page": 0, "cols": cols}));

    if eww(&["ping"]).1 != "pong" {
        let config = eww_config();
        let _ = std::process::Command::new("eww")
            .arg("-c")
            .arg(&config)
            .arg("daemon")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        for _ in 0..40 {
            std::thread::sleep(secs(0.1));
            if eww(&["ping"]).1 == "pong" {
                break;
            }
        }
    }
    let mut screens: Vec<Option<String>> = focused_monitor().into_iter().map(Some).collect();
    screens.push(None);
    let opened = screens.iter().any(|screen| {
        let ok = match screen {
            Some(m) => eww(&["open", "clipdeck", "--screen", m]).0 == 0,
            None => eww(&["open", "clipdeck"]).0 == 0,
        };
        if ok {
            submap("clip"); // only now: every other bind is off inside it
        }
        ok
    });
    if !opened {
        let _ = std::fs::remove_file(state_path());
        notify("Clipboard overlay didn’t open", &format!("eww couldn’t open it; try: eww -c {} logs", eww_config().display()));
        return 1;
    }

    let lock = Mutex::new(());
    std::thread::scope(|scope| {
        for (index, (col, node)) in cols.iter().zip(order.iter()).enumerate().skip(1) {
            let lock = &lock;
            scope.spawn(move || {
                let mut fetched = col.clone();
                let mut fresh = col.clone();
                fresh["kind"] = col["base_kind"].clone();
                fresh["cached"] = json!(false);
                match fetch_column(&mut fresh, node) {
                    Ok(()) => {
                        fetched = fresh;
                        store_cache(&fetched);
                    }
                    Err(error) => offline(&mut fetched, &error), // keeps the cached copy, if any
                }
                let _guard = lock.lock();
                changing(|current| {
                    // merge into what's on disk now: the page may have been flipped meanwhile
                    if current["open"] == true && util::ja(current, "cols").len() > index {
                        current["cols"][index] = fetched;
                        save(current);
                    }
                });
            });
        }
    });
    0
}

fn hide() {
    let _ = std::fs::remove_file(state_path());
    eww(&["close", "clipdeck"]);
    submap("reset");
}

/// The entry's content and, when known, its type.
fn fetch_bytes(alias: &str, item: &Value) -> Res<(Vec<u8>, Option<String>)> {
    let nodes = config::load_nodes()?;
    let node = config::find(&nodes, alias).ok_or("That machine is no longer in hosts.json.")?;
    if !node.linux() {
        let value =
            if item.get("id").is_some_and(|i| !i.is_null()) { backend::call(node, "clipboard-entry", item["id"].clone())? } else { backend::clipboard(node)? };
        let (normalized, raw) = clipboards::payload_bytes(&value)?;
        return Ok((raw, normalized["mime"].as_str().filter(|m| !m.is_empty()).map(str::to_string)));
    }
    let id = item["id"].as_u64().ok_or("That entry is gone.")?;
    let raw = if node.is_local() {
        clipboards::cliphist_decode(id)?
    } else {
        util::unb64(backend::call(node, "clipboard-entry", json!(id))?["data"].as_str().unwrap_or(""))?
    };
    let mime = raw.starts_with(PNG_SIGNATURE).then(|| "image/png".to_string()); // else wl-copy works it out
    Ok((raw, mime))
}

fn page(direction: &str) {
    changing(|state| {
        if state["open"] != true {
            return;
        }
        let last = pages(state) - 1;
        let page = current_page(state);
        let page = match direction {
            "next" | "down" => page + 1,
            "prev" | "up" => page.saturating_sub(1),
            "first" => 0,
            "last" => last,
            _ => page,
        };
        state["page"] = json!(page.min(last));
        save(state);
    });
}

fn pick(key: &str) {
    let state = load();
    let page = current_page(&state);
    let key = key.to_uppercase();
    let cols = util::ja(&state, "cols");
    let found = cols.iter().enumerate().find_map(|(i, c)| page_of(c, page).into_iter().find(|it| it["key"] == key.as_str()).map(|it| (i, c, it)));
    let Some((index, col, item)) = found else { return }; // nothing (yet) under that key: stay open
    hide();
    let name = col["name"].as_str().unwrap_or("").to_lowercase();
    let alias = col["alias"].as_str().unwrap_or("");
    let result = (|| -> Res<()> {
        let (data, mime) = if let Some(text) = item["text"].as_str() {
            (text.as_bytes().to_vec(), None) // already here: works with that machine off
        } else if col["cached"] == true {
            fetch_bytes(alias, &item).map_err(|_| format!("{name} is off: only text entries copy from its saved history."))?
        } else {
            fetch_bytes(alias, &item)?
        };
        clipboards::wl_copy(&data, mime.as_deref(), false, None)
    })();
    match result {
        Ok(()) if index > 0 => notify(&format!("Copied from {name}"), "Ready to paste here."),
        Ok(()) => {}
        Err(error) => notify(&format!("Couldn’t copy from {name}"), &util::cut(&error, 200)),
    }
}

pub fn main(argv: &[String]) -> i32 {
    match (argv.first().map(String::as_str), argv.get(1)) {
        (None | Some("-h" | "--help" | "help"), _) => {
            println!("{HELP}");
            0
        }
        (Some("show"), None) => show(),
        (Some("hide"), None) => {
            hide();
            0
        }
        (Some("pick"), Some(key)) => {
            pick(key);
            0
        }
        (Some("page"), Some(direction)) => {
            page(&direction.to_lowercase());
            0
        }
        (Some("stream"), None) => stream(),
        (Some("forget"), None) => {
            forget();
            0
        }
        _ => {
            eprintln!("clipdeck: bad command (try `clipdeck help`)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_previews() {
        assert_eq!(describe("[[ binary data 12 KiB png 10x20 ]]"), ("▣ PNG 10×20 · 12 KiB".into(), true));
        assert_eq!(describe("  a\\b  \n c"), ("a⧵b c".into(), false));
        assert_eq!(describe(""), ("(blank space)".into(), false));
    }

    #[test]
    fn paging() {
        let items: Vec<Value> = (0..25).map(|i| json!({"id": i, "label": i.to_string(), "img": false})).collect();
        let state = json!({"open": true, "page": 2, "cols": [{"name": "A", "kind": "x", "state": "ok", "note": "", "keys": KEY_ROWS[0], "items": items}]});
        assert_eq!(pages(&state), 3);
        let shown = for_eww(&state);
        assert_eq!(shown["page"], 3);
        assert_eq!(shown["cols"][0]["span"], "21–25 / 25");
        assert_eq!(shown["cols"][0]["items"][0]["key"], "1");
        assert!(shown["cols"][0]["items"][0].get("id").is_none());
    }
}
