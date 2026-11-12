//! The dashboard. One thread reads the terminal, background threads do the SSH work; both send
//! messages to the main loop, which only redraws when something changed.
mod draw;
mod text;
mod widgets;

use crate::backend;
use crate::config::{self, Node};
use crate::util::{self, jf};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use text::*;
use widgets::*;

pub const TABS: [&str; 8] = ["Overview", "Processes", "Session", "VPN", "Files", "Clipboard", "Run", "Maintenance"];
pub const OVERVIEW: usize = 0;
pub const PROCESSES: usize = 1;
pub const SESSION: usize = 2;
pub const VPN: usize = 3;
pub const FILES: usize = 4;
pub const CLIPBOARD: usize = 5;
pub const RUN: usize = 6;
pub const MAINTENANCE: usize = 7;

const STATUS_INTERVAL: u64 = 15;
const PROCESS_INTERVAL: u64 = 3;
const SCREENTIME_INTERVAL: u64 = 60;
const WAKE_TIMEOUT: u64 = 120;
const RUN_TIMEOUT: f64 = 60.0;
pub const PROCESS_LIMIT: usize = 200;

/// Process table: key, title, width, numeric (numeric columns sort highest first).
pub const PROCESS_COLUMNS: [(&str, &str, u16, bool); 8] = [
    ("pid", "PID", 7, false),
    ("name", "Process", 18, false),
    ("user", "User", 9, false),
    ("cpu", "CPU", 6, true),
    ("gpu", "GPU", 6, true),
    ("memory", "Memory", 9, true),
    ("disk", "Disk", 10, true),
    ("net", "Net TCP", 10, true),
];

type Apply = Box<dyn FnOnce(&mut App) + Send>;

fn apply(f: impl FnOnce(&mut App) + Send + 'static) -> Apply {
    Box::new(f)
}

pub enum Msg {
    Term(Event),
    Apply(Apply),
    Tick,
}

pub enum Modal {
    Confirm { message: String, label: String, yes: bool, on_yes: Option<Box<dyn FnOnce(&mut App)>> },
    Prompt { title: String, placeholder: String, label: String, input: Input, on_ok: Option<Box<dyn FnOnce(&mut App, String)>> },
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

#[derive(Default)]
pub struct ScreenTime {
    pub data: Option<Value>,
    pub error: Option<String>,
    pub at: Option<Instant>,
    pub read: String,
}

pub struct App {
    pub nodes: Vec<Node>,
    pub sel: usize,
    pub local: usize,
    pub tab: usize,
    pub tx: Sender<Msg>,
    pub quit: bool,
    pub hits: Hits,
    pub focus: Option<Field>,
    pub modal: Option<Modal>,
    pub toasts: Vec<Toast>,
    pub tab_scroll: [usize; 8],
    pub activity: Log,
    pub terminal_request: Option<Vec<String>>,

    pub statuses: HashMap<String, Value>,
    pub histories: HashMap<String, VecDeque<f64>>,
    status_pending: usize,
    last_status: Option<Instant>,
    pub sharing: Option<Value>,
    pub sharing_error: Option<String>,
    pub screentime: HashMap<String, ScreenTime>,
    pub fleet_cursor: usize,
    pub fleet_offset: usize,
    pub waking: HashMap<String, (Instant, bool)>,
    pub speed: HashMap<String, (String, Color)>,
    pub speed_running: Option<String>,

    pub process_data: Option<Value>,
    pub process_alias: Option<String>,
    pub process_error: Option<String>,
    pub process_message: Option<String>,
    pub process_updated: String,
    process_busy: bool,
    process_generation: u64,
    last_process: Option<Instant>,
    pub proc_sort: (usize, bool),
    pub proc_filter: Input,
    pub proc_mine: bool,
    pub proc_cursor: Option<u64>,
    pub proc_offset: usize,

    pub hypr: Option<Value>,
    pub hypr_message: String,
    pub session_table: TableId,
    pub ws_cursor: usize,
    pub ws_offset: usize,
    pub win_cursor: usize,
    pub win_offset: usize,

    pub vpn_cursor: usize,
    pub vpn_offset: usize,
    pub vpn_log: Log,

    pub local_path: Input,
    pub remote_path: Input,
    pub transfer_running: bool,
    pub transfer_status: String,

    pub clip_values: HashMap<String, Value>,
    pub clip_state: HashMap<String, String>,
    clip_generation: HashMap<String, u64>,
    pub clip_busy: bool,
    pub clip_result: String,
    pub clip_scroll: HashMap<String, u16>,
    previews: Vec<(PathBuf, Instant)>,

    pub run_command: Input,
    pub run_targets: Vec<bool>,
    pub run_running: bool,
    pub run_status: Vec<(String, String, Color)>,
    pub run_log: Log,
    run_failures: usize,
    run_left: usize,

    pub report: Log,
    prompt_values: HashMap<(String, &'static str), String>,
    pub clock: String,
}

impl App {
    fn new(nodes: Vec<Node>, tx: Sender<Msg>, tab: usize) -> App {
        let local = nodes.iter().position(Node::is_local).unwrap_or(0);
        let run_targets = nodes.iter().map(Node::linux).collect();
        App {
            sel: local,
            local,
            tab,
            tx,
            quit: false,
            hits: Hits::default(),
            focus: None,
            modal: None,
            toasts: Vec::new(),
            tab_scroll: [0; 8],
            activity: Log::new(150, false),
            terminal_request: None,
            statuses: HashMap::new(),
            histories: HashMap::new(),
            status_pending: 0,
            last_status: None,
            sharing: None,
            sharing_error: None,
            screentime: HashMap::new(),
            fleet_cursor: local,
            fleet_offset: 0,
            waking: HashMap::new(),
            speed: HashMap::new(),
            speed_running: None,
            process_data: None,
            process_alias: None,
            process_error: None,
            process_message: None,
            process_updated: String::new(),
            process_busy: false,
            process_generation: 0,
            last_process: None,
            proc_sort: (3, true),
            proc_filter: Input::default(),
            proc_mine: false,
            proc_cursor: None,
            proc_offset: 0,
            hypr: None,
            hypr_message: String::new(),
            session_table: TableId::Windows,
            ws_cursor: 0,
            ws_offset: 0,
            win_cursor: 0,
            win_offset: 0,
            vpn_cursor: local,
            vpn_offset: 0,
            vpn_log: Log::new(400, false),
            local_path: Input::default(),
            remote_path: Input::default(),
            transfer_running: false,
            transfer_status: "Ready to transfer.".into(),
            clip_values: HashMap::new(),
            clip_state: HashMap::new(),
            clip_generation: HashMap::new(),
            clip_busy: false,
            clip_result: "Open this tab or press r to read each machine’s clipboard.".into(),
            clip_scroll: HashMap::new(),
            previews: Vec::new(),
            run_command: Input::default(),
            run_targets,
            run_running: false,
            run_status: Vec::new(),
            run_log: Log::new(5000, false),
            run_failures: 0,
            run_left: 0,
            report: Log::new(5000, true),
            prompt_values: HashMap::new(),
            clock: util::now_hm(),
            nodes,
        }
    }

    pub fn node(&self) -> &Node {
        &self.nodes[self.sel]
    }

    fn spawn(&self, job: impl FnOnce() -> Apply + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Apply(job()));
        });
    }

    pub fn log_event(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.activity.push(Line::from(vec![Span::styled(format!("{}  ", util::now_hms()), Style::new().fg(DIM)), Span::raw(text)]));
    }

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        let text = util::cut(&text.into(), 250);
        self.toasts.push(Toast { text, error, until: Instant::now() + Duration::from_secs(if error { 7 } else { 4 }) });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    pub fn is_offline(&self, node: &Node) -> bool {
        self.statuses.get(&node.alias).is_some_and(|s| s["online"] != true)
    }

    pub fn status(&self, node: &Node) -> Option<&Value> {
        self.statuses.get(&node.alias)
    }

    // ---------- selection and tabs ----------

    fn select_node(&mut self, index: usize) {
        if index >= self.nodes.len() {
            return;
        }
        self.sel = index;
        self.fleet_cursor = index;
        self.vpn_cursor = index;
        self.hypr = None;
        self.hypr_message =
            if self.node().linux() { "Press r to read this Hyprland session.".into() } else { "Hyprland controls are available on the Linux machines.".into() };
        self.report.clear();
        self.screentime_tick();
        if self.tab == SESSION && self.node().linux() {
            self.refresh_hypr();
        }
        if self.process_alias.as_deref() != Some(&self.node().alias) {
            self.process_data = None;
            self.process_alias = None;
            self.process_error = None;
            self.process_message = None;
            self.proc_cursor = None;
        }
        if self.tab == PROCESSES {
            self.refresh_processes();
        }
    }

    fn set_tab(&mut self, tab: usize) {
        if tab == self.tab {
            return;
        }
        self.tab = tab;
        self.focus = if tab == RUN { Some(Field::RunCommand) } else { None };
        match tab {
            SESSION if self.node().linux() => self.refresh_hypr(),
            PROCESSES => self.refresh_processes(),
            CLIPBOARD => self.refresh_clipboards(None),
            _ => {}
        }
        if tab != CLIPBOARD {
            // don't keep other machines' clipboards in memory while nobody looks at them
            self.clip_values.clear();
            for node in &self.nodes {
                *self.clip_generation.entry(node.alias.clone()).or_default() += 1;
            }
            self.clip_state.clear();
        }
    }

    fn refresh(&mut self) {
        self.refresh_status();
        match self.tab {
            SESSION if self.node().linux() => self.refresh_hypr(),
            CLIPBOARD => self.refresh_clipboards(None),
            PROCESSES => self.refresh_processes(),
            _ => {}
        }
    }

    // ---------- status ----------

    fn refresh_status(&mut self) {
        if self.status_pending > 0 {
            return;
        }
        self.last_status = Some(Instant::now());
        self.status_pending = self.nodes.len() + self.nodes.iter().any(|n| n.lilin) as usize;
        for node in self.nodes.clone() {
            self.spawn(move || {
                let result = backend::status(&node);
                apply(move |app| {
                    app.status_pending = app.status_pending.saturating_sub(1);
                    if result["online"] == true {
                        let history = app.histories.entry(node.alias.clone()).or_default();
                        history.push_back(jf(&result, "cpu").unwrap_or(0.0));
                        while history.len() > 40 {
                            history.pop_front();
                        }
                    }
                    app.statuses.insert(node.alias.clone(), result);
                })
            });
        }
        if self.nodes.iter().any(|n| n.lilin) {
            self.spawn(|| {
                let result = backend::lilin_load();
                apply(move |app| {
                    app.status_pending = app.status_pending.saturating_sub(1);
                    match result {
                        Ok(load) => {
                            app.sharing = Some(load);
                            app.sharing_error = None;
                        }
                        Err(error) => app.sharing_error = Some(error),
                    }
                })
            });
        }
    }

    fn screentime_tick(&mut self) {
        let node = self.node().clone();
        if !node.linux() {
            return;
        }
        let due = self.screentime.get(&node.alias).and_then(|s| s.at).is_none_or(|at| at.elapsed() >= Duration::from_secs(SCREENTIME_INTERVAL - 1));
        if due {
            self.screentime.entry(node.alias.clone()).or_default().at = Some(Instant::now());
            self.spawn(move || {
                let result = backend::screentime(&node);
                apply(move |app| {
                    let entry = app.screentime.entry(node.alias).or_default();
                    match result {
                        Ok(data) => {
                            entry.data = Some(data);
                            entry.error = None;
                            entry.read = util::now_hm();
                        }
                        Err(error) => entry.error = Some(error),
                    }
                })
            });
        }
    }

    // ---------- wake ----------

    fn wake(&mut self) {
        let node = self.node().clone();
        if !self.is_offline(&node) {
            let text = if self.status(&node).is_some() { format!("{} is already online.", node.hostname) } else { "Still checking this machine.".into() };
            self.notify(text, false);
            return;
        }
        if node.mac.is_none() {
            self.notify(format!("{} can't be woken over the network (no wired MAC in hosts.json).", node.hostname), true);
            return;
        }
        if self.waking.get(&node.alias).is_some_and(|w| !w.1) {
            return;
        }
        match backend::wake(&node) {
            Ok(message) => self.log_event(message),
            Err(error) => {
                self.notify(error, true);
                return;
            }
        }
        let started = Instant::now();
        self.waking.insert(node.alias.clone(), (started, false));
        self.spawn(move || {
            while started.elapsed() < Duration::from_secs(WAKE_TIMEOUT) {
                std::thread::sleep(Duration::from_secs(5));
                let result = backend::status(&node);
                if result["online"] == true {
                    return apply(move |app| {
                        app.waking.remove(&node.alias);
                        app.statuses.insert(node.alias.clone(), result);
                        app.log_event(format!("{} is awake.", node.hostname));
                        app.notify(format!("{} is awake", node.hostname), false);
                    });
                }
            }
            apply(move |app| {
                app.waking.insert(node.alias.clone(), (started, true));
                app.log_event(format!("{} did not answer within 2 minutes of the wake packet.", node.hostname));
            })
        });
    }

    // ---------- link test ----------

    fn speed_test(&mut self) {
        let node = self.node().clone();
        if node.is_local() || self.is_offline(&node) || self.speed_running.is_some() {
            return;
        }
        self.speed_running = Some(node.alias.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let name = node.hostname.clone();
            let phases = |phase: &str| match phase {
                "latency" => "Measuring latency…".to_string(),
                "upload" => format!("Sending 64 MB to {name}…"),
                _ => format!("Receiving 64 MB from {name}…"),
            };
            let measured = Arc::new(Mutex::new(Value::Null));
            let progress = |phase: &str, results: &Value| {
                *measured.lock().unwrap() = results.clone();
                let text = describe_speed(&name, results, &phases(phase));
                let alias = node.alias.clone();
                let _ = tx.send(Msg::Apply(apply(move |app| {
                    app.speed.insert(alias, (text, LAVENDER));
                })));
            };
            let result = backend::speed_test(&node, &progress);
            let measured = measured.lock().unwrap().clone();
            let _ = tx.send(Msg::Apply(apply(move |app| {
                app.speed_running = None;
                match result {
                    Ok(results) => {
                        app.speed.insert(node.alias.clone(), (describe_speed(&name, &results, &format!("tested {}", util::now_hm())), ACCENT));
                        app.log_event(format!("{name} link: {}", describe_speed(&name, &results, "")));
                    }
                    Err(error) => {
                        app.speed.insert(node.alias.clone(), (describe_speed(&name, &measured, &format!("test failed: {error}")), BAD));
                        app.log_event(format!("{name} link test failed: {error}"));
                    }
                }
            })));
        });
    }

    // ---------- processes ----------

    fn refresh_processes(&mut self) {
        let node = self.node().clone();
        self.process_generation += 1;
        let generation = self.process_generation;
        self.process_busy = true;
        self.last_process = Some(Instant::now());
        if self.process_data.is_none() && self.process_error.is_none() {
            self.process_message = Some(format!("Reading processes on {}… (takes {})", node.hostname, if node.linux() { "a second" } else { "about 5 s" }));
        }
        self.spawn(move || {
            let result = backend::call(&node, "processes", Value::Null);
            apply(move |app| {
                if generation == app.process_generation {
                    app.process_busy = false;
                }
                if app.node().alias != node.alias {
                    return;
                }
                match result {
                    Ok(data) => {
                        app.process_data = Some(data);
                        app.process_alias = Some(node.alias.clone());
                        app.process_error = None;
                        app.process_message = None;
                        app.process_updated = util::now_hms();
                    }
                    Err(error) => {
                        if app.process_error.as_deref() != Some(&error) {
                            app.log_event(format!("{}: can't read processes: {error}", node.hostname));
                        }
                        if app.process_data.is_some() && app.process_alias.as_deref() == Some(&node.alias) {
                            app.process_message = Some(format!("Refresh failed ({error}) · showing the read from {} · retrying", app.process_updated));
                        } else {
                            app.process_data = None;
                            app.process_alias = None;
                            app.process_message = Some(format!("Can't read processes on {}: {error} · retrying every {PROCESS_INTERVAL} s", node.hostname));
                        }
                        app.process_error = Some(error);
                    }
                }
            })
        });
    }

    /// Filtered by name, command, user or PID, then sorted. Unreadable values sort last.
    pub fn process_rows(&self) -> Vec<&Value> {
        let Some(data) = &self.process_data else { return Vec::new() };
        let needle = self.proc_filter.text.trim().to_lowercase();
        let mut rows: Vec<&Value> = util::ja(data, "processes")
            .iter()
            .filter(|p| !self.proc_mine || p["own"] == true)
            .filter(|p| {
                needle.is_empty()
                    || format!("{} {} {} {}", util::jtext(&p["pid"]), util::jtext(&p["name"]), util::jtext(&p["command"]), util::jtext(&p["user"]))
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect();
        let (column, descending) = self.proc_sort;
        let (key, _, _, numeric) = PROCESS_COLUMNS[column];
        let pid = |p: &Value| p["pid"].as_u64().unwrap_or(0);
        if numeric {
            let (mut present, missing): (Vec<&Value>, Vec<&Value>) = rows.into_iter().partition(|p| process_value(p, key).is_some());
            present.sort_by(|a, b| {
                let order = process_value(a, key).unwrap().total_cmp(&process_value(b, key).unwrap()).then(pid(b).cmp(&pid(a)));
                if descending { order.reverse() } else { order }
            });
            present.extend(missing);
            present
        } else {
            rows.sort_by(|a, b| {
                let order = if key == "pid" {
                    pid(a).cmp(&pid(b))
                } else {
                    util::jtext(&a[key]).to_lowercase().cmp(&util::jtext(&b[key]).to_lowercase()).then(pid(a).cmp(&pid(b)))
                };
                if descending { order.reverse() } else { order }
            });
            rows
        }
    }

    pub fn selected_process(&self) -> Option<Value> {
        let rows = self.process_rows();
        let pid = self.proc_cursor?;
        rows.iter().take(PROCESS_LIMIT).find(|p| p["pid"].as_u64() == Some(pid)).map(|p| (*p).clone())
    }

    fn move_process_cursor(&mut self, delta: i64) {
        let pids: Vec<u64> = self.process_rows().iter().take(PROCESS_LIMIT).filter_map(|p| p["pid"].as_u64()).collect();
        if pids.is_empty() {
            return;
        }
        let current = self.proc_cursor.and_then(|pid| pids.iter().position(|p| *p == pid));
        let index = match current {
            Some(i) => (i as i64 + delta).clamp(0, pids.len() as i64 - 1) as usize,
            None => 0,
        };
        self.proc_cursor = Some(pids[index]);
    }

    fn sort_processes(&mut self, column: usize) {
        let numeric = PROCESS_COLUMNS[column].3;
        self.proc_sort = if self.proc_sort.0 == column { (column, !self.proc_sort.1) } else { (column, numeric) };
    }

    fn end_process(&mut self) {
        let node = self.node().clone();
        let Some(process) = self.selected_process() else { return };
        if process["own"] != true || !node.linux() {
            return;
        }
        let message = format!(
            "End {} (PID {}) on {}?\n\nThis sends SIGTERM, asking it to close. Unsaved work in it may be lost.",
            util::jtext(&process["name"]),
            util::jtext(&process["pid"]),
            node.hostname
        );
        self.confirm(message, "End process", move |app| {
            let value = json!({"pid": process["pid"], "start": process["start"]});
            app.spawn(move || {
                let result = backend::call(&node, "process-term", value);
                apply(move |app| {
                    match result {
                        Ok(r) => {
                            let text = util::jtext(&r["text"]);
                            app.log_event(format!("{}: {text}", node.hostname));
                            app.notify(text, false);
                        }
                        Err(error) => {
                            app.log_event(format!("{}: could not end process: {error}", node.hostname));
                            app.notify(error, true);
                        }
                    }
                    if app.node().alias == node.alias {
                        app.refresh_processes();
                    }
                })
            });
        });
    }

    // ---------- session (Hyprland) ----------

    fn refresh_hypr(&mut self) {
        let node = self.node().clone();
        if !node.linux() {
            return;
        }
        self.spawn(move || {
            let result = backend::call(&node, "hypr", Value::Null);
            apply(move |app| {
                if app.node().alias != node.alias {
                    return;
                }
                match result {
                    Ok(data) => app.hypr = Some(data),
                    Err(error) => {
                        app.hypr = None;
                        app.hypr_message = error.clone();
                        app.log_event(format!("{}: {error}", node.hostname));
                    }
                }
            })
        });
    }

    pub fn workspaces(&self) -> Vec<Value> {
        let mut list: Vec<Value> = self.hypr.as_ref().map(|h| util::ja(h, "workspaces").to_vec()).unwrap_or_default();
        list.sort_by_key(|w| (w["id"].as_i64().unwrap_or(100000), util::jtext(&w["name"])));
        list
    }

    pub fn windows(&self) -> Vec<Value> {
        self.hypr.as_ref().map(|h| util::ja(h, "clients").to_vec()).unwrap_or_default()
    }

    /// Run a helper action on a machine; reports land in Maintenance's results.
    fn execute(&mut self, node: Node, action: &'static str, value: Value) {
        if matches!(action, "updates" | "services" | "journal" | "storage" | "sessions") {
            self.report.clear();
            self.report.text(&format!("{}: running {action}…", node.hostname), Style::new().fg(MUTED));
        }
        self.spawn(move || {
            let result = backend::call(&node, action, value);
            apply(move |app| match result {
                Ok(result) => {
                    let text = util::jtext(&result["text"]);
                    let failed = result["code"].as_i64().unwrap_or(0) != 0;
                    if app.node().alias == node.alias && matches!(action, "updates" | "services" | "journal" | "storage" | "sessions") {
                        app.report.clear();
                        app.report.push(Line::styled(format!("{} / {action}", node.hostname), Style::new().fg(LAVENDER).add_modifier(Modifier::BOLD)));
                        app.report.push(Line::raw(""));
                        app.report.text(&text, Style::new().fg(FG));
                    }
                    app.log_event(format!("{} / {action}: {}", node.hostname, if failed { "failed" } else { "done" }));
                    if failed {
                        app.notify(util::cut(&text, 250), true);
                    } else if matches!(action, "lock" | "notify" | "open-url") {
                        app.notify(format!("{}: {text}", node.hostname), false);
                    }
                    if matches!(action, "workspace" | "focus" | "reload") && app.node().alias == node.alias {
                        app.refresh_hypr();
                    }
                }
                Err(error) => {
                    app.log_event(format!("{} / {action}: {error}", node.hostname));
                    app.notify(error.clone(), true);
                    if app.node().alias == node.alias {
                        app.report.text(&error, Style::new().fg(BAD));
                    }
                }
            })
        });
    }

    fn peek(&mut self) {
        let node = self.node().clone();
        if !node.linux() {
            return;
        }
        self.notify(format!("Capturing {}'s screen…", node.hostname), false);
        self.spawn(move || {
            let result = backend::call(&node, "screenshot", Value::Null).and_then(|shot| {
                let data = util::unb64(shot["data"].as_str().unwrap_or(""))?;
                Ok((data, shot["bytes"].as_f64().unwrap_or(0.0)))
            });
            apply(move |app| match result {
                Ok((data, bytes)) => match app.open_local_image(&data, "jpg") {
                    Ok(()) => app.log_event(format!("{}: screen captured ({:.1} MB), opened here.", node.hostname, bytes / 1024f64.powi(2))),
                    Err(error) => app.notify(error, true),
                },
                Err(error) => {
                    app.log_event(format!("{}: could not capture the screen: {error}", node.hostname));
                    app.notify(error, true);
                }
            })
        });
    }

    /// Show image bytes in this desktop's image viewer; the file is removed after 5 minutes.
    fn open_local_image(&mut self, data: &[u8], suffix: &str) -> Result<(), String> {
        let path = util::runtime_dir().join(format!("archfleet-{}-{}.{suffix}", std::process::id(), self.previews.len()));
        util::write_private(&path, data).map_err(|e| e.to_string())?;
        self.previews.push((path.clone(), Instant::now()));
        let shown = path.to_string_lossy().to_string();
        #[cfg(unix)]
        {
            let env = crate::helper::hypr_environment().unwrap_or_default();
            crate::helper::spawn_detached(&["xdg-open", &shown], &env)
        }
        #[cfg(not(unix))]
        {
            std::process::Command::new("cmd").args(["/C", "start", "", &shown]).spawn().map(|_| ()).map_err(|e| e.to_string())
        }
    }

    // ---------- VPN ----------

    fn vpn_do(&mut self, node: Node, action: &'static str, value: Option<String>) {
        self.vpn_log.push(Line::styled(
            format!("{}: {}{}…", node.hostname, &action[4..], value.as_deref().map(|v| format!(" {v}")).unwrap_or_default()),
            Style::new().fg(MUTED),
        ));
        self.spawn(move || {
            let result = backend::call(&node, action, value.map(Value::String).unwrap_or(Value::Null));
            std::thread::sleep(Duration::from_secs(2)); // let the tunnel settle before the table re-reads it
            apply(move |app| {
                match result {
                    Ok(result) => {
                        let text = util::jtext(&result["text"]);
                        let failed = result["code"].as_i64().unwrap_or(0) != 0;
                        app.vpn_log.text(if text.is_empty() { "done" } else { &text }, Style::new().fg(if failed { BAD } else { FG }));
                        app.log_event(format!("{} / {action}: {}", node.hostname, if failed { "failed" } else { "done" }));
                        if failed {
                            app.notify(if text.is_empty() { "VPN action failed".into() } else { text }, true);
                        }
                    }
                    Err(error) => {
                        app.vpn_log.text(&error, Style::new().fg(BAD));
                        app.notify(error, true);
                    }
                }
                app.refresh_status();
            })
        });
    }

    // ---------- files ----------

    fn transfer(&mut self, download: bool) {
        if self.transfer_running {
            return;
        }
        let node = self.node().clone();
        let (local, remote) = (self.local_path.text.clone(), self.remote_path.text.clone());
        if local.trim().is_empty() || remote.trim().is_empty() {
            self.notify("Enter both paths first.", true);
            return;
        }
        let message = format!(
            "{} {}?\n\nLocal: {local}\nRemote: {remote}\n\nAn existing destination file will be overwritten.",
            if download { "Download from" } else { "Upload to" },
            node.hostname
        );
        self.confirm(message, if download { "Download" } else { "Upload" }, move |app| {
            app.transfer_running = true;
            app.transfer_status = "Transfer in progress…".into();
            app.spawn(move || {
                let result = backend::transfer(&node, &local, &remote, download);
                apply(move |app| {
                    app.transfer_running = false;
                    match result {
                        Ok(message) => {
                            app.transfer_status = message.clone();
                            app.log_event(message);
                            app.notify("Transfer complete", false);
                        }
                        Err(error) => {
                            app.transfer_status = error.clone();
                            app.log_event(format!("Transfer: {error}"));
                            app.notify(error, true);
                        }
                    }
                })
            });
        });
    }

    // ---------- clipboards ----------

    fn refresh_clipboards(&mut self, only: Option<usize>) {
        for (index, node) in self.nodes.clone().into_iter().enumerate() {
            if only.is_some_and(|o| o != index) {
                continue;
            }
            let generation = {
                let g = self.clip_generation.entry(node.alias.clone()).or_default();
                *g += 1;
                *g
            };
            self.clip_values.remove(&node.alias);
            self.clip_state.insert(node.alias.clone(), "Reading…".into());
            self.spawn(move || {
                let result = backend::clipboard(&node);
                apply(move |app| {
                    if app.clip_generation.get(&node.alias) != Some(&generation) || app.tab != CLIPBOARD {
                        return;
                    }
                    match result {
                        Ok(value) => app.show_clipboard(&node.alias, value),
                        Err(error) => {
                            app.clip_state.insert(node.alias.clone(), format!("Unavailable: {error}"));
                            app.log_event(format!("{}: clipboard read unavailable", node.hostname));
                        }
                    }
                })
            });
        }
    }

    fn show_clipboard(&mut self, alias: &str, value: Value) {
        let description = match value["kind"].as_str() {
            Some("text") => {
                let count = value["text"].as_str().unwrap_or("").chars().count();
                let mut d = format!("{count} characters");
                if count > 20000 {
                    d += " · Preview shows the first 20,000; Copy here copies the full text.";
                }
                d
            }
            Some("image") => format!("PNG · {} bytes", util::jtext(&value["bytes"])),
            Some("empty") => "Clipboard is empty.".into(),
            _ => "Copying supports plain text and PNG.".into(),
        };
        self.clip_state.insert(alias.to_string(), format!("{description} · {}", util::now_hms()));
        self.clip_scroll.remove(alias);
        self.clip_values.insert(alias.to_string(), value);
    }

    fn copy_clipboard(&mut self, index: usize, send_local: bool) {
        if self.clip_busy || index >= self.nodes.len() {
            return;
        }
        let target = self.nodes[index].clone();
        let local = self.nodes[self.local].clone();
        if send_local && target.is_local() {
            return;
        }
        let (source, destination) = if send_local { (local, target.clone()) } else { (target.clone(), local) };
        let snapshot = if send_local { None } else { self.clip_values.get(&target.alias).cloned() };
        if !send_local && !snapshot.as_ref().is_some_and(|v| v["kind"] == "text" || v["kind"] == "image") {
            return;
        }
        self.clip_busy = true;
        self.spawn(move || {
            let result = (|| {
                let value = match snapshot {
                    Some(v) => v,
                    None => backend::clipboard(&source)?,
                };
                if value["kind"] != "text" && value["kind"] != "image" {
                    return Err("The source clipboard is empty or has an unsupported format. Refresh it first.".to_string());
                }
                backend::set_clipboard(&destination, &value)?;
                Ok(value)
            })();
            apply(move |app| {
                app.clip_busy = false;
                match result {
                    Ok(value) => {
                        let message = format!("Copied {} clipboard to {}.", source.hostname, destination.hostname);
                        app.clip_result = message.clone();
                        app.log_event(message);
                        app.notify("Clipboard copied", false);
                        if app.tab == CLIPBOARD {
                            app.show_clipboard(&destination.alias, value);
                        }
                    }
                    Err(error) => {
                        app.clip_result = error.clone();
                        app.notify(error, true);
                        app.log_event("Clipboard copy failed");
                    }
                }
            })
        });
    }

    fn view_clipboard_image(&mut self, index: usize) {
        let Some(value) = self.nodes.get(index).and_then(|n| self.clip_values.get(&n.alias)).cloned() else { return };
        if value["kind"] != "image" {
            return;
        }
        let result = crate::clipboards::payload_bytes(&value).and_then(|(_, data)| self.open_local_image(&data, "png"));
        if let Err(error) = result {
            self.notify(error, true);
        }
    }

    // ---------- run on fleet ----------

    fn start_run(&mut self) {
        let command = self.run_command.text.trim().to_string();
        let targets: Vec<Node> = self.nodes.iter().zip(&self.run_targets).filter(|(_, on)| **on).map(|(n, _)| n.clone()).collect();
        if self.run_running {
            return;
        }
        if command.is_empty() {
            self.notify("Type a command first.", true);
            return;
        }
        if targets.is_empty() {
            self.notify("Tick at least one machine (Alt+1…9 or click).", true);
            return;
        }
        self.run_command.remember();
        self.run_running = true;
        self.run_failures = 0;
        self.run_left = targets.len();
        self.run_log.clear();
        self.run_log
            .push(Line::from(vec![Span::styled("$ ", Style::new().fg(MUTED)), Span::styled(command.clone(), Style::new().add_modifier(Modifier::BOLD))]));
        self.run_status = targets.iter().map(|n| (n.alias.clone(), "… running".to_string(), LAVENDER)).collect();
        let count = targets.len();
        let local_alias = self.nodes[self.local].alias.clone();
        for node in targets {
            let command = command.clone();
            let local_alias = local_alias.clone();
            self.spawn(move || {
                let result = backend::run_command(&node, &command, RUN_TIMEOUT);
                apply(move |app| {
                    let (label, color) = match result.code {
                        Some(0) => (format!("✓ {:.1} s", result.seconds), ACCENT),
                        None => (format!("✗ timed out after {RUN_TIMEOUT:.0} s"), BAD),
                        Some(255) if node.alias != local_alias => ("✗ connection failed".into(), BAD),
                        Some(code) => (format!("✗ exit {code} · {:.1} s", result.seconds), BAD),
                    };
                    app.run_failures += (result.code != Some(0)) as usize;
                    app.run_left = app.run_left.saturating_sub(1);
                    if let Some(entry) = app.run_status.iter_mut().find(|e| e.0 == node.alias) {
                        entry.1 = label.clone();
                        entry.2 = color;
                    }
                    app.run_log.push(Line::raw(""));
                    app.run_log.push(Line::from(vec![
                        Span::styled("━━ ", Style::new().fg(color)),
                        Span::styled(node.hostname.clone(), Style::new().fg(color).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" · {label}"), Style::new().fg(color)),
                    ]));
                    if result.output.is_empty() {
                        app.run_log.text("(no output)", Style::new().fg(DIM));
                    } else {
                        app.run_log.text(&result.output, Style::new().fg(FG));
                    }
                    if app.run_left == 0 {
                        app.run_running = false;
                        let outcome = if app.run_failures == 0 { "all succeeded".into() } else { format!("{} failed", app.run_failures) };
                        app.log_event(format!("Ran `{}` on {count} machine{}: {outcome}", util::cut(&command, 60), if count > 1 { "s" } else { "" }));
                    }
                })
            });
        }
    }

    // ---------- maintenance ----------

    fn report_heading(&mut self, title: &str, subtitle: &str) {
        self.report.clear();
        self.report.push(Line::from(vec![
            Span::styled(title.to_string(), Style::new().fg(LAVENDER).add_modifier(Modifier::BOLD)),
            Span::styled(if subtitle.is_empty() { String::new() } else { format!("   {subtitle}") }, Style::new().fg(MUTED)),
        ]));
    }

    fn show_news(&mut self) {
        let node = self.node().clone();
        self.report_heading("Arch Linux news", "reading archlinux.org…");
        self.spawn(move || {
            let result = backend::arch_news();
            apply(move |app| {
                let items = match result {
                    Ok(items) => items,
                    Err(error) => {
                        app.report.text(&format!("Couldn't read archlinux.org news: {error}"), Style::new().fg(BAD));
                        return;
                    }
                };
                let since_text = if node.linux() { app.status(&node).and_then(|s| util::js(s, "last_upgrade")).map(str::to_string) } else { None };
                let since = since_text.as_deref().and_then(util::parse_time);
                app.report_heading("Arch Linux news", &format!("checked {}", util::now_hm()));
                if node.linux() {
                    let line = match since {
                        Some(t) => format!("{}: last full upgrade {} ({}).", node.hostname, t.format("%Y-%m-%d"), util::ago(since_text.as_deref())),
                        None => format!("{}: last upgrade date unknown, so review the recent items.", node.hostname),
                    };
                    app.report.text(&line, Style::new().fg(SOFT));
                }
                app.report.push(Line::raw(""));
                let mut fresh = 0;
                for item in &items {
                    let new = since.is_some_and(|s| item.date > s);
                    fresh += new as usize;
                    let line = Line::from(vec![
                        Span::styled(format!("{}  ", item.date.format("%Y-%m-%d")), Style::new().fg(MUTED)),
                        Span::styled(if new { "NEW  " } else { "" }, Style::new().fg(WARN).add_modifier(Modifier::BOLD)),
                        Span::styled(item.title.clone(), if new { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() }),
                    ]);
                    app.report.link(line, item.link.clone());
                    if new {
                        app.report.link(Line::styled(format!("            {}", item.link), Style::new().fg(MUTED).add_modifier(Modifier::UNDERLINED)), item.link.clone());
                    }
                }
                app.report.push(Line::raw(""));
                if fresh > 0 {
                    app.report.text(
                        &format!(
                            "{fresh} item{} since {}'s last upgrade. News posts often mean a manual step before or after -Syu; read them first (click one to open it).",
                            if fresh > 1 { "s" } else { "" },
                            node.hostname
                        ),
                        Style::new().fg(WARN),
                    );
                } else if since.is_some() {
                    app.report.text(&format!("Nothing new since {}'s last upgrade.", node.hostname), Style::new().fg(ACCENT));
                }
            })
        });
    }

    fn compare_packages(&mut self) {
        let machines: Vec<Node> = self.nodes.iter().filter(|n| n.linux()).take(2).cloned().collect();
        if machines.len() < 2 {
            self.notify("Comparing packages needs two Linux machines in hosts.json.", true);
            return;
        }
        let title = format!("Explicit packages  ·  {} ↔ {}", machines[0].hostname, machines[1].hostname);
        self.report_heading(&title, "reading pacman…");
        self.spawn(move || {
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = machines.iter().map(|n| s.spawn(move || backend::call(n, "packages", Value::Null))).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            apply(move |app| {
                app.report_heading(&title, &format!("checked {}", util::now_hm()));
                let mut sets = Vec::new();
                for (node, result) in machines.iter().zip(results) {
                    match result {
                        Ok(r) => {
                            sets.push(util::ja(&r, "packages").iter().filter_map(Value::as_str).map(str::to_string).collect::<std::collections::BTreeSet<_>>())
                        }
                        Err(error) => {
                            app.report.text(
                                &format!("Can't compare: {} didn't answer ({error}). Wake it and press Compare packages again.", node.hostname),
                                Style::new().fg(BAD),
                            );
                        }
                    }
                }
                if sets.len() < 2 {
                    return;
                }
                let (a, b) = (&sets[0], &sets[1]);
                app.report.text(
                    &format!("{} on {}  ·  {} on {}  ·  {} on both", a.len(), machines[0].hostname, b.len(), machines[1].hostname, a.intersection(b).count()),
                    Style::new().fg(SOFT),
                );
                app.report.push(Line::raw(""));
                if a == b {
                    app.report.text("Both machines have the same explicitly installed packages.", Style::new().fg(ACCENT));
                    return;
                }
                for (name, only) in
                    [(&machines[0].hostname, a.difference(b).cloned().collect::<Vec<_>>()), (&machines[1].hostname, b.difference(a).cloned().collect())]
                {
                    app.report.push(Line::styled(format!("Only on {name} ({})", only.len()), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
                    if only.is_empty() {
                        app.report.text("nothing", Style::new().fg(DIM));
                    } else {
                        app.report.text(&only.join("   "), Style::new().fg(FG));
                    }
                    app.report.push(Line::raw(""));
                }
            })
        });
    }

    // ---------- dialogs ----------

    fn confirm(&mut self, message: String, label: &str, on_yes: impl FnOnce(&mut App) + 'static) {
        self.modal = Some(Modal::Confirm { message, label: label.into(), yes: false, on_yes: Some(Box::new(on_yes)) });
    }

    /// A one-line prompt that remembers what was typed last time, per machine and kind.
    fn ask(&mut self, key: &'static str, title: String, placeholder: &str, label: &str, on_ok: impl FnOnce(&mut App, String) + 'static) {
        let mut input = Input::default();
        if let Some(previous) = self.prompt_values.get(&(self.node().alias.clone(), key)) {
            input.set(previous);
        }
        let alias = self.node().alias.clone();
        self.modal = Some(Modal::Prompt {
            title,
            placeholder: placeholder.into(),
            label: label.into(),
            input,
            on_ok: Some(Box::new(move |app: &mut App, text: String| {
                app.prompt_values.insert((alias, key), text.clone());
                on_ok(app, text);
            })),
        });
    }

    fn modal_answer(&mut self, yes: bool) {
        match self.modal.take() {
            Some(Modal::Confirm { on_yes: Some(f), .. }) if yes => f(self),
            Some(Modal::Prompt { input, on_ok: Some(f), .. }) if yes => {
                let text = input.text.trim().to_string();
                if !text.is_empty() {
                    f(self, text);
                }
            }
            _ => {}
        }
    }

    // ---------- actions (keys and clicks) ----------

    fn act(&mut self, act: Act) {
        let node = self.node().clone();
        let linux = node.linux();
        match act {
            Act::Machine(i) => self.select_node(i),
            Act::Tab(i) => self.set_tab(i),
            Act::Focus(field) => self.focus = Some(field),
            Act::Row(TableId::Fleet, i) | Act::Row(TableId::Vpn, i) => self.select_node(i),
            Act::Row(TableId::Procs, i) => {
                self.proc_cursor = self.process_rows().get(i).and_then(|p| p["pid"].as_u64());
            }
            Act::Row(TableId::Workspaces, i) => {
                self.session_table = TableId::Workspaces;
                self.ws_cursor = i;
            }
            Act::Row(TableId::Windows, i) => {
                self.session_table = TableId::Windows;
                self.win_cursor = i;
            }
            Act::Header(TableId::Procs, i) => self.sort_processes(i),
            Act::Header(..) => {}
            Act::Link(url) => self.open_link(&url),
            Act::Wake => self.wake(),
            Act::Shell => self.terminal_request = Some(vec![backend::tool("ssh"), node.alias]),
            Act::Sftp => self.terminal_request = Some(vec![backend::tool("sftp"), node.alias]),
            Act::Tmux if linux => {
                self.terminal_request = Some(vec![
                    backend::tool("ssh"),
                    "-t".into(),
                    node.alias,
                    "if command -v tmux >/dev/null; then tmux new-session -A -s archfleet; else printf \"tmux is not installed on this machine.\\n\"; fi"
                        .into(),
                ])
            }
            Act::SpeedTest => self.speed_test(),
            Act::ProcMine => self.proc_mine = !self.proc_mine,
            Act::ProcEnd => self.end_process(),
            Act::Peek if linux => self.peek(),
            Act::Lock if linux => {
                let message = format!("Lock {}'s screen?\n\nhyprlock will ask for your password there to unlock it.", node.hostname);
                self.confirm(message, "Lock screen", move |app| app.execute(node, "lock", Value::Null));
            }
            Act::Notify if linux => {
                let from = self.nodes[self.local].hostname.clone();
                self.ask("notify", format!("Show a notification on {}", node.hostname), "Message, up to 500 characters", "Send", move |app, text| {
                    app.execute(node, "notify", json!({"text": text, "from": from}))
                });
            }
            Act::OpenUrl if linux => {
                self.ask("url", format!("Open a link in {}'s browser", node.hostname), "https://…", "Open", move |app, url| {
                    app.execute(node, "open-url", json!(url))
                });
            }
            Act::HyprRefresh if linux => self.refresh_hypr(),
            Act::HyprReload if linux => {
                let message = format!("Reload the Hyprland configuration on {}?", node.hostname);
                self.confirm(message, "Reload config", move |app| app.execute(node, "reload", Value::Null));
            }
            Act::GoWorkspace if linux => {
                if self.session_table == TableId::Workspaces && self.hypr.is_some() {
                    if let Some(ws) = self.workspaces().get(self.ws_cursor) {
                        let name = util::jtext(&ws["name"]);
                        self.execute(node, "workspace", json!(name));
                    }
                } else {
                    self.ask("workspace", format!("Go to a workspace on {}", node.hostname), "Workspace 1–99", "Go", move |app, n| {
                        app.execute(node, "workspace", json!(n))
                    });
                }
            }
            Act::FocusWindow if linux => {
                if let Some(window) = self.windows().get(self.win_cursor) {
                    let address = util::jtext(&window["address"]);
                    self.execute(node, "focus", json!(address));
                }
            }
            Act::VpnConnect | Act::VpnReconnect if self.vpn_linux_ok() => {
                self.vpn_do(node, if act == Act::VpnConnect { "vpn-connect" } else { "vpn-reconnect" }, None)
            }
            Act::VpnDisconnect if self.vpn_linux_ok() => {
                let lockdown = self.status(&node).and_then(|s| s["vpn"]["lockdown"].as_bool()).unwrap_or(false);
                let message = format!(
                    "Disconnect {}'s VPN?\n\n{}",
                    node.hostname,
                    if lockdown {
                        "Lockdown is on there: it loses internet until you reconnect (the LAN keeps working)."
                    } else {
                        "Its traffic goes out directly until you reconnect."
                    }
                );
                self.confirm(message, "Disconnect", move |app| app.vpn_do(node, "vpn-disconnect", None));
            }
            Act::VpnRelay if self.vpn_linux_ok() => {
                self.ask(
                    "vpn-relay",
                    format!("Switch {}'s VPN relay", node.hostname),
                    "country [city] or hostname, e.g. se sto · de fra · se-sto-wg-001",
                    "Switch",
                    move |app, where_| app.vpn_do(node, "vpn-relay", Some(where_)),
                );
            }
            Act::VpnRestart if !linux && node.vpn_str("restart").is_some() => {
                let message = format!(
                    "Restart the VPN tunnel on {}?\n\nWhat uses it pauses for a few seconds; the kill switch keeps it from leaking meanwhile.",
                    node.hostname
                );
                self.confirm(message, "Restart tunnel", move |app| app.vpn_do(node, "vpn-restart", None));
            }
            Act::VpnTest if !linux && node.vpn_str("test").is_some() => self.vpn_do(node, "vpn-test", None),
            Act::Upload => self.transfer(false),
            Act::Download => self.transfer(true),
            Act::ClipRefreshAll => self.refresh_clipboards(None),
            Act::ClipRefresh(i) => self.refresh_clipboards(Some(i)),
            Act::ClipCopy(i) => self.copy_clipboard(i, false),
            Act::ClipSend(i) => self.copy_clipboard(i, true),
            Act::ClipView(i) => self.view_clipboard_image(i),
            Act::Run => self.start_run(),
            Act::RunTarget(i) => {
                if let Some(t) = self.run_targets.get_mut(i) {
                    *t = !*t;
                }
            }
            Act::Report(action) if linux => self.execute(node, action, Value::Null),
            Act::News => self.show_news(),
            Act::Upgrade if linux => {
                let message = format!(
                    "Run a full system upgrade on {}?\n\nThis opens sudo pacman -Syu in SSH. Review its package list before accepting. Enter any password in the terminal.\n\nTip: Arch news shows posts that may need a manual step first.",
                    node.hostname
                );
                self.confirm(message, "Upgrade", move |app| {
                    app.terminal_request = Some(vec![backend::tool("ssh"), "-t".into(), node.alias, "sudo pacman -Syu".into()])
                });
            }
            Act::Packages => self.compare_packages(),
            Act::ModalYes => self.modal_answer(true),
            Act::ModalNo => self.modal_answer(false),
            _ => {}
        }
    }

    pub fn vpn_linux_ok(&self) -> bool {
        let node = self.node();
        node.linux() && self.status(node).is_some_and(|s| s["online"] == true && s["vpn"]["kind"] == "mullvad")
    }

    fn open_link(&mut self, url: &str) {
        #[cfg(unix)]
        let result = {
            let env = crate::helper::hypr_environment().unwrap_or_default();
            crate::helper::spawn_detached(&["xdg-open", url], &env)
        };
        #[cfg(not(unix))]
        let result = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn().map(|_| ()).map_err(|e| e.to_string());
        match result {
            Ok(()) => self.notify(format!("Opened {url}"), false),
            Err(error) => self.notify(error, true),
        }
    }

    // ---------- input ----------

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        if let Some(field) = self.focus {
            self.field_key(field, key);
            return;
        }
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(c @ '1'..='9') if alt && self.tab == RUN => self.act(Act::RunTarget(c as usize - '1' as usize)),
            KeyCode::Char(c @ '1'..='9') => self.select_node(c as usize - '1' as usize),
            KeyCode::Left => self.set_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Right | KeyCode::Tab => self.set_tab((self.tab + 1) % TABS.len()),
            KeyCode::BackTab => self.set_tab((self.tab + TABS.len() - 1) % TABS.len()),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('w') => self.wake(),
            KeyCode::Char('s') => self.act(Act::Shell),
            KeyCode::Char('f') => self.act(Act::Sftp),
            KeyCode::Char('p') => {
                if self.tab == PROCESSES {
                    self.refresh_processes()
                } else {
                    self.set_tab(PROCESSES)
                }
            }
            KeyCode::Char('c') => {
                if self.tab == CLIPBOARD {
                    self.refresh_clipboards(None)
                } else {
                    self.set_tab(CLIPBOARD)
                }
            }
            KeyCode::Up => self.nav(-1),
            KeyCode::Down => self.nav(1),
            KeyCode::PageUp => self.nav(-10),
            KeyCode::PageDown => self.nav(10),
            KeyCode::Home => self.nav(-100000),
            KeyCode::End => self.nav(100000),
            KeyCode::Enter => self.enter(),
            KeyCode::Delete if self.tab == PROCESSES => self.end_process(),
            KeyCode::Char(c) => self.tab_key(c),
            _ => {}
        }
    }

    fn tab_key(&mut self, c: char) {
        let act = match (self.tab, c) {
            (OVERVIEW, 't') => Act::Tmux,
            (OVERVIEW, 'l') => Act::SpeedTest,
            (PROCESSES, '/') => Act::Focus(Field::ProcFilter),
            (PROCESSES, 'm') => Act::ProcMine,
            (PROCESSES, 'o') => {
                self.proc_sort = ((self.proc_sort.0 + 1) % PROCESS_COLUMNS.len(), PROCESS_COLUMNS[(self.proc_sort.0 + 1) % PROCESS_COLUMNS.len()].3);
                return;
            }
            (PROCESSES, 'O') => {
                self.proc_sort.1 = !self.proc_sort.1;
                return;
            }
            (PROCESSES, 'e') => Act::ProcEnd,
            (SESSION, 'k') => Act::Peek,
            (SESSION, 'l') => Act::Lock,
            (SESSION, 'n') => Act::Notify,
            (SESSION, 'u') => Act::OpenUrl,
            (SESSION, 'g') => {
                let previous = self.session_table;
                self.session_table = TableId::Windows; // g always asks for a number
                self.act(Act::GoWorkspace);
                self.session_table = previous;
                return;
            }
            (SESSION, 'R') => Act::HyprReload,
            (SESSION, 't') => {
                self.session_table = if self.session_table == TableId::Windows { TableId::Workspaces } else { TableId::Windows };
                return;
            }
            (VPN, 'o') => Act::VpnConnect,
            (VPN, 'x') => Act::VpnDisconnect,
            (VPN, 'e') => Act::VpnReconnect,
            (VPN, 'l') => Act::VpnRelay,
            (VPN, 'R') => Act::VpnRestart,
            (VPN, 't') => Act::VpnTest,
            (FILES, 'i') => Act::Focus(Field::LocalPath),
            (FILES, 'u') => Act::Upload,
            (FILES, 'd') => Act::Download,
            (CLIPBOARD, 'a') => Act::ClipRefreshAll,
            (CLIPBOARD, 'h') => Act::ClipCopy(self.sel),
            (CLIPBOARD, 'l') => Act::ClipSend(self.sel),
            (CLIPBOARD, 'v') => Act::ClipView(self.sel),
            (RUN, 'i') => Act::Focus(Field::RunCommand),
            (MAINTENANCE, 'u') => Act::Report("updates"),
            (MAINTENANCE, 'n') => Act::News,
            (MAINTENANCE, 'U') => Act::Upgrade,
            (MAINTENANCE, 'v') => Act::Report("services"),
            (MAINTENANCE, 'j') => Act::Report("journal"),
            (MAINTENANCE, 'd') => Act::Report("storage"),
            (MAINTENANCE, 'l') => Act::Report("sessions"),
            (MAINTENANCE, 'k') => Act::Packages,
            _ => return,
        };
        self.act(act);
    }

    /// Up/Down/PgUp/PgDn: the tab's list, or its scroll.
    fn nav(&mut self, delta: i64) {
        let step = |cursor: usize, len: usize| -> usize { (cursor as i64 + delta).clamp(0, len.saturating_sub(1) as i64) as usize };
        match self.tab {
            OVERVIEW if delta.abs() == 1 => self.fleet_cursor = step(self.fleet_cursor, self.nodes.len()),
            PROCESSES => self.move_process_cursor(delta),
            SESSION if delta.abs() == 1 => match self.session_table {
                TableId::Workspaces => self.ws_cursor = step(self.ws_cursor, self.workspaces().len()),
                _ => self.win_cursor = step(self.win_cursor, self.windows().len()),
            },
            VPN if delta.abs() == 1 => self.vpn_cursor = step(self.vpn_cursor, self.nodes.len()),
            CLIPBOARD if delta.abs() == 1 => {
                let next = step(self.sel, self.nodes.len());
                self.select_node(next);
            }
            RUN => self.run_log.scroll(-delta.clamp(-1000, 1000) as i32 * 3),
            MAINTENANCE => self.report.scroll(-delta.clamp(-1000, 1000) as i32 * 3),
            tab => self.tab_scroll[tab] = (self.tab_scroll[tab] as i64 + delta.clamp(-1000, 1000) * 3).max(0) as usize,
        }
    }

    fn enter(&mut self) {
        match self.tab {
            OVERVIEW => self.select_node(self.fleet_cursor),
            VPN => self.select_node(self.vpn_cursor),
            SESSION => self.act(if self.session_table == TableId::Workspaces { Act::GoWorkspace } else { Act::FocusWindow }),
            FILES => self.focus = Some(Field::LocalPath),
            RUN => self.focus = Some(Field::RunCommand),
            _ => {}
        }
    }

    fn input(&mut self, field: Field) -> &mut Input {
        match field {
            Field::ProcFilter => &mut self.proc_filter,
            Field::LocalPath => &mut self.local_path,
            Field::RemotePath => &mut self.remote_path,
            Field::RunCommand => &mut self.run_command,
        }
    }

    fn field_key(&mut self, field: Field, key: KeyEvent) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.focus = None,
            KeyCode::Enter => match field {
                Field::RunCommand => self.start_run(),
                Field::LocalPath => self.focus = Some(Field::RemotePath),
                _ => self.focus = None,
            },
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match field {
                    Field::LocalPath => Some(Field::RemotePath),
                    Field::RemotePath => Some(Field::LocalPath),
                    _ => None,
                }
            }
            KeyCode::Up if field == Field::RunCommand => self.run_command.older(),
            KeyCode::Down if field == Field::RunCommand => self.run_command.newer(),
            KeyCode::Up if field == Field::ProcFilter => self.move_process_cursor(-1),
            KeyCode::Down if field == Field::ProcFilter => self.move_process_cursor(1),
            KeyCode::PageUp if field == Field::RunCommand => self.run_log.scroll(10),
            KeyCode::PageDown if field == Field::RunCommand => self.run_log.scroll(-10),
            KeyCode::Char(c @ '1'..='9') if alt => {
                if self.tab == RUN {
                    self.act(Act::RunTarget(c as usize - '1' as usize));
                } else {
                    self.select_node(c as usize - '1' as usize);
                }
            }
            _ => {
                self.input(field).key(key);
            }
        }
    }

    fn modal_key(&mut self, key: KeyEvent) {
        match self.modal.as_mut() {
            Some(Modal::Confirm { yes, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.modal_answer(false),
                KeyCode::Char('y') => self.modal_answer(true),
                KeyCode::Enter => {
                    let answer = *yes;
                    self.modal_answer(answer);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Char('l') => *yes = !*yes,
                _ => {}
            },
            Some(Modal::Prompt { input, .. }) => match key.code {
                KeyCode::Esc => self.modal_answer(false),
                KeyCode::Enter => self.modal_answer(true),
                _ => {
                    input.key(key);
                }
            },
            None => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        match (&mut self.modal, self.focus) {
            (Some(Modal::Prompt { input, .. }), _) => input.insert(&text.replace(['\r', '\n'], " ")),
            (None, Some(field)) => self.input(field).insert(&text.replace(['\r', '\n'], " ")),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: event::MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let act = self.hits.click_at(x, y).cloned();
                if self.modal.is_some() {
                    if matches!(act, Some(Act::ModalYes) | Some(Act::ModalNo)) {
                        self.act(act.unwrap());
                    }
                    return;
                }
                if !matches!(act, Some(Act::Focus(_))) {
                    self.focus = None;
                }
                if let Some(act) = act {
                    self.act(act);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if self.modal.is_some() {
                    return;
                }
                let rows: i64 = if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                match self.hits.scroll_at(x, y) {
                    Some(Scroll::Table(TableId::Procs)) => self.move_process_cursor(rows),
                    Some(Scroll::Table(TableId::Fleet)) => {
                        self.fleet_cursor = (self.fleet_cursor as i64 + rows.signum()).clamp(0, self.nodes.len() as i64 - 1) as usize
                    }
                    Some(Scroll::Table(TableId::Vpn)) => {
                        self.vpn_cursor = (self.vpn_cursor as i64 + rows.signum()).clamp(0, self.nodes.len() as i64 - 1) as usize
                    }
                    Some(Scroll::Table(TableId::Workspaces)) => {
                        self.ws_cursor = (self.ws_cursor as i64 + rows).clamp(0, self.workspaces().len().saturating_sub(1) as i64) as usize
                    }
                    Some(Scroll::Table(TableId::Windows)) => {
                        self.win_cursor = (self.win_cursor as i64 + rows).clamp(0, self.windows().len().saturating_sub(1) as i64) as usize
                    }
                    Some(Scroll::Activity) => self.activity.scroll(-rows as i32),
                    Some(Scroll::Report) => self.report.scroll(-rows as i32),
                    Some(Scroll::VpnLog) => self.vpn_log.scroll(-rows as i32),
                    Some(Scroll::RunLog) => self.run_log.scroll(-rows as i32),
                    Some(Scroll::Clip(i)) => {
                        if let Some(node) = self.nodes.get(i) {
                            let entry = self.clip_scroll.entry(node.alias.clone()).or_default();
                            *entry = (*entry as i64 + rows).max(0) as u16;
                        }
                    }
                    Some(Scroll::Tab) | None => self.tab_scroll[self.tab] = (self.tab_scroll[self.tab] as i64 + rows).max(0) as usize,
                }
            }
            _ => {}
        }
    }

    /// Once a second: timers. Returns whether anything on screen changed.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        let clock = util::now_hm();
        if clock != self.clock {
            self.clock = clock;
            changed = true;
        }
        if self.last_status.is_none_or(|t| t.elapsed() >= Duration::from_secs(STATUS_INTERVAL)) {
            self.refresh_status();
        }
        if self.tab == PROCESSES && !self.process_busy && self.last_process.is_none_or(|t| t.elapsed() >= Duration::from_secs(PROCESS_INTERVAL)) {
            self.refresh_processes();
        }
        if self.waking.values().any(|w| !w.1) {
            changed = true;
        }
        self.screentime_tick();
        let before = self.toasts.len();
        self.toasts.retain(|t| t.until > Instant::now());
        changed |= before != self.toasts.len();
        self.previews.retain(|(path, at)| {
            let keep = at.elapsed() < Duration::from_secs(300);
            if !keep {
                let _ = std::fs::remove_file(path);
            }
            keep
        });
        changed
    }
}

fn describe_speed(name: &str, results: &Value, tail: &str) -> String {
    let mut parts = Vec::new();
    if let Some(ms) = jf(results, "latency_ms") {
        parts.push(format!("ping {ms:.1} ms"));
    }
    if let Some(up) = jf(results, "upload") {
        parts.push(format!("to {name} {}", util::mbit(up)));
    }
    if let Some(down) = jf(results, "download") {
        parts.push(format!("from {name} {}", util::mbit(down)));
    }
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts.join("   ·   ")
}

pub fn process_value(p: &Value, key: &str) -> Option<f64> {
    let pair = |a: &str, b: &str| Some(jf(p, a)? + jf(p, b)?);
    match key {
        "disk" => pair("disk_read", "disk_write"),
        "net" => pair("net_rx", "net_tx"),
        _ => jf(p, key),
    }
}

// ---------- the terminal ----------

/// Pausing the input thread while ssh/sftp own the terminal, so it doesn't steal their keys.
#[derive(Default)]
struct Pause {
    want: bool,
    parked: bool,
}

fn input_thread(tx: Sender<Msg>, pause: Arc<(Mutex<Pause>, Condvar)>) {
    std::thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            {
                let (lock, cvar) = &*pause;
                let mut state = lock.lock().unwrap();
                if state.want {
                    state.parked = true;
                    cvar.notify_all();
                    while state.want {
                        state = cvar.wait(state).unwrap();
                    }
                    state.parked = false;
                }
            }
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(Msg::Term(ev)).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                if tx.send(Msg::Tick).is_err() {
                    return;
                }
            }
        }
    });
}

fn enter_screen() -> std::io::Result<()> {
    use ratatui::crossterm::{event::*, execute, terminal::*};
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
}

fn leave_screen() {
    use ratatui::crossterm::{event::*, execute, terminal::*};
    let _ = execute!(std::io::stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
    let _ = disable_raw_mode();
}

pub fn run(clipboard_tab: bool) -> i32 {
    let nodes = match config::load_nodes() {
        Ok(nodes) if !nodes.is_empty() => nodes,
        Ok(_) => {
            eprintln!(
                "No machines configured yet. Copy hosts.example.json to {} and edit it (one entry per SSH alias), or run install.sh again.",
                config::config_path().display()
            );
            return 1;
        }
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    let mut app = App::new(nodes, tx.clone(), if clipboard_tab { CLIPBOARD } else { OVERVIEW });

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_screen();
        hook(info);
    }));
    if let Err(error) = enter_screen() {
        eprintln!("archfleet: can't use this terminal: {error}");
        return 1;
    }
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(t) => t,
        Err(error) => {
            leave_screen();
            eprintln!("archfleet: {error}");
            return 1;
        }
    };
    let pause = Arc::new((Mutex::new(Pause::default()), Condvar::new()));
    input_thread(tx, pause.clone());

    app.log_event("Welcome home. Select a machine to get started.");
    app.select_node(app.sel);
    app.refresh_status();
    if app.tab == CLIPBOARD {
        app.refresh_clipboards(None);
    }

    let mut dirty = true;
    while !app.quit {
        if dirty {
            if terminal.draw(|frame| draw::draw(frame, &mut app)).is_err() {
                break;
            }
            dirty = false;
        }
        let Ok(first) = rx.recv() else { break };
        let mut message = Some(first);
        while let Some(msg) = message {
            match msg {
                Msg::Term(Event::Key(key)) => {
                    app.on_key(key);
                    dirty = true;
                }
                Msg::Term(Event::Mouse(mouse)) => {
                    let moved = matches!(mouse.kind, MouseEventKind::Moved | MouseEventKind::Drag(_));
                    if !moved {
                        app.on_mouse(mouse);
                        dirty = true;
                    }
                }
                Msg::Term(Event::Paste(text)) => {
                    app.on_paste(&text);
                    dirty = true;
                }
                Msg::Term(Event::Resize(..)) => dirty = true,
                Msg::Term(_) => {}
                Msg::Apply(f) => {
                    f(&mut app);
                    dirty = true;
                }
                Msg::Tick => dirty |= app.tick(),
            }
            message = rx.try_recv().ok();
        }
        if let Some(argv) = app.terminal_request.take() {
            let (lock, cvar) = &*pause;
            {
                let mut state = lock.lock().unwrap();
                state.want = true;
                while !state.parked {
                    state = cvar.wait(state).unwrap();
                }
            }
            leave_screen();
            let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
            if let Err(error) = status {
                app.notify(util::spawn_error(&argv[0], error), true);
            }
            let _ = enter_screen();
            let _ = terminal.clear();
            lock.lock().unwrap().want = false;
            cvar.notify_all();
            app.last_status = None;
            app.refresh_status();
            dirty = true;
        }
    }
    leave_screen();
    for (path, _) in &app.previews {
        let _ = std::fs::remove_file(path);
    }
    0
}
