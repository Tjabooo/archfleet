//! Running programs with a timeout, reading JSON loosely, and formatting numbers for people.
use serde_json::Value;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

pub type Res<T> = Result<T, String>;

/// Environment overrides for a child process (`hyprctl`, `wl-paste`, ... need the session's).
pub type Env = Vec<(String, String)>;

pub struct Out {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Out {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
    /// stdout and stderr together, like `capture_output` + `stdout + stderr`.
    pub fn both(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&self.stderr));
        text.trim().to_string()
    }
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `argv` to completion, feeding `input` on stdin. A timeout kills it and sets `timed_out`.
pub fn run(argv: &[String], input: Option<&[u8]>, timeout: Duration, env: Option<&Env>) -> Res<Out> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut command = Command::new(program);
    command.args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(env) = env {
        command.envs(env.iter().map(|(k, v)| (k, v)));
    }
    let mut child = command.spawn().map_err(|e| spawn_error(program, e))?;
    let writer = input.map(|data| {
        let mut stdin = child.stdin.take().expect("stdin piped");
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        })
    });
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let status = child.wait_timeout(timeout).map_err(|e| e.to_string())?;
    let timed_out = status.is_none();
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            child.wait().map_err(|e| e.to_string())?
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(Out {
        code: if timed_out { None } else { status.code() },
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        timed_out,
    })
}

pub fn spawn_error(program: &str, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound { format!("{program}: command not found") } else { format!("{program}: {error}") }
}

pub fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

pub fn secs(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `~/...` -> the home folder.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|p| p.is_file())
}

#[cfg(unix)]
pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// $XDG_RUNTIME_DIR (RAM, this user only), or /run/user/<uid>.
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uid())))
}

#[cfg(not(unix))]
pub fn runtime_dir() -> PathBuf {
    std::env::temp_dir()
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn local_hostname() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 256];
            let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
            if ok {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                return String::from_utf8_lossy(&buf[..end]).to_string();
            }
            std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
        }
        #[cfg(not(unix))]
        {
            std::env::var("COMPUTERNAME").unwrap_or_default()
        }
    })
}

/// Write a file only this user can read, atomically (temp file + rename).
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
    }
    std::fs::rename(&tmp, path)
}

// ---------- loose JSON access (the helper protocol is plain JSON objects) ----------

pub fn jf(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
pub fn js<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}
pub fn jb(v: &Value, key: &str) -> bool {
    truthy(v.get(key).unwrap_or(&Value::Null))
}
pub fn ja<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}
/// Python's idea of truth, for JSON values.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
/// A value shown as text: strings as they are, numbers without a trailing ".0".
pub fn jtext(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

// ---------- formatting ----------

pub fn gib(value: f64) -> String {
    format!("{:.1} GiB", value / 1024f64.powi(3))
}

pub fn uptime(seconds: f64) -> String {
    let minutes = seconds.max(0.0) as u64 / 60;
    let (days, hours) = (minutes / 60 / 24, minutes / 60 % 24);
    if days > 0 { format!("{days}d {hours}h") } else { format!("{hours}h {}m", minutes % 60) }
}

pub fn meter(percent: f64, width: usize) -> String {
    let count = ((percent / 100.0 * width as f64).round().max(0.0) as usize).min(width);
    "━".repeat(count) + &"─".repeat(width - count)
}

/// Memory in binary units, short enough for a table cell.
pub fn size(value: Option<f64>) -> String {
    match value {
        None => "—".into(),
        Some(v) if v >= 1024f64.powi(3) => format!("{:.1} GiB", v / 1024f64.powi(3)),
        Some(v) => format!("{:.0} MiB", v / 1024f64.powi(2)),
    }
}

/// A byte rate in decimal units: 0, 820 B/s, 12 kB/s, 1.4 MB/s.
pub fn speed(value: Option<f64>) -> String {
    let Some(value) = value else { return "—".into() };
    for (unit, scale) in [("GB/s", 1e9), ("MB/s", 1e6), ("kB/s", 1e3)] {
        if value >= scale {
            let n = value / scale;
            return if n < 10.0 { format!("{n:.1} {unit}") } else { format!("{n:.0} {unit}") };
        }
    }
    if value > 0.0 { format!("{value:.0} B/s") } else { "0".into() }
}

pub fn percent_text(value: Option<f64>) -> String {
    match value {
        None => "—".into(),
        Some(v) if v < 0.05 => "0%".into(),
        Some(v) if v < 10.0 => format!("{v:.1}%"),
        Some(v) => format!("{v:.0}%"),
    }
}

pub fn celsius(value: Option<f64>) -> String {
    value.map_or("—".into(), |v| format!("{v:.0}°C"))
}

pub fn mbit(rate: f64) -> String {
    format!("{:.0} Mbit/s", rate * 8.0 / 1e6)
}

/// A byte rate as a bare Mbit/s number: 0, 0.7, 27.
pub fn mbits(rate: Option<f64>) -> String {
    let value = rate.unwrap_or(0.0) * 8.0 / 1e6;
    if value < 0.05 {
        "0".into()
    } else if value < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// Bytes sent, decimal: 858 MB, 3.6 GB.
pub fn size_text(value: Option<f64>) -> String {
    let value = value.unwrap_or(0.0);
    for (unit, scale) in [("GB", 1e9), ("MB", 1e6), ("kB", 1e3)] {
        if value >= scale {
            let n = value / scale;
            return if n < 10.0 { format!("{n:.1} {unit}") } else { format!("{n:.0} {unit}") };
        }
    }
    format!("{value:.0} B")
}

/// Parse the timestamps the helpers send: ISO 8601 with or without an offset.
pub fn parse_time(stamp: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
    if let Ok(t) = DateTime::parse_from_rfc3339(stamp) {
        return Some(t);
    }
    for format in ["%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%z"] {
        if let Ok(t) = DateTime::parse_from_str(stamp, format) {
            return Some(t);
        }
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(stamp, format) {
            return Local.from_local_datetime(&t).single().map(|t| t.fixed_offset());
        }
    }
    None
}

/// How long ago an ISO timestamp was, in whole days: today, yesterday, 4 days ago.
pub fn ago(stamp: Option<&str>) -> String {
    let Some(when) = stamp.and_then(parse_time) else { return "unknown".into() };
    let today = chrono::Local::now().date_naive();
    let days = (today - when.with_timezone(&chrono::Local).date_naive()).num_days();
    match days {
        d if d <= 0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

/// Unix seconds -> '40 s ago', '12 min ago', '3 h ago', '2 d ago'.
pub fn since(epoch: Option<f64>) -> String {
    let Some(epoch) = epoch.filter(|e| *e > 0.0) else { return "—".into() };
    let seconds = (now_secs() - epoch).max(0.0);
    if seconds < 90.0 {
        format!("{seconds:.0} s ago")
    } else if seconds < 5400.0 {
        format!("{:.0} min ago", seconds / 60.0)
    } else if seconds < 172800.0 {
        format!("{:.0} h ago", seconds / 3600.0)
    } else {
        format!("{:.0} d ago", seconds / 86400.0)
    }
}

/// Milliseconds since the epoch -> local HH:MM.
pub fn clock(ms: f64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms as i64).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

pub fn lasted(ms: f64) -> String {
    let seconds = (ms / 1000.0).round().max(0.0) as u64;
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else {
        format!("{} h {} min", seconds / 3600, seconds % 3600 / 60)
    }
}

/// 3h 42m · 12m · <1m · 0m
pub fn st_duration(seconds: Option<f64>) -> String {
    let seconds = seconds.unwrap_or(0.0).round() as u64;
    if seconds == 0 {
        return "0m".into();
    }
    if seconds < 60 {
        return "<1m".into();
    }
    let (hours, minutes) = (seconds / 60 / 60, seconds / 60 % 60);
    if hours > 0 { format!("{hours}h {minutes:02}m") } else { format!("{minutes}m") }
}

pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
pub fn now_hm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// Cut to `max` characters (not bytes).
pub fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// PowerShell single-quoted string.
pub fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A PowerShell script as `-EncodedCommand` wants it: base64 of UTF-16LE.
pub fn ps_encode(script: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}
pub fn unb64(text: &str) -> Res<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| "Invalid base64 data.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(speed(Some(0.0)), "0");
        assert_eq!(speed(Some(820.0)), "820 B/s");
        assert_eq!(speed(Some(12_000.0)), "12 kB/s");
        assert_eq!(speed(Some(1_400_000.0)), "1.4 MB/s");
        assert_eq!(speed(None), "—");
        assert_eq!(uptime(90_000.0), "1d 1h");
        assert_eq!(uptime(3_660.0), "1h 1m");
        assert_eq!(meter(50.0, 4), "━━──");
        assert_eq!(size(Some(512.0 * 1024.0 * 1024.0)), "512 MiB");
        assert_eq!(st_duration(Some(13_320.0)), "3h 42m");
        assert_eq!(st_duration(Some(30.0)), "<1m");
        assert_eq!(mbits(Some(125_000.0)), "1.0");
        assert_eq!(percent_text(Some(0.01)), "0%");
        assert_eq!(lasted(125_000.0), "2 min");
        assert_eq!(jtext(&serde_json::json!(3.0)), "3");
    }

    #[test]
    fn times() {
        assert!(parse_time("2026-11-08T12:00:00+0100").is_some());
        assert!(parse_time("2026-11-08T12:00:00+01:00").is_some());
        assert!(parse_time("2026-11-08T12:00:00").is_some());
        assert_eq!(ago(None), "unknown");
    }

    #[test]
    fn runs_with_timeout() {
        let out = run(&args(&["sh", "-c", "cat; echo err >&2"]), Some(b"hi"), secs(5.0), None).unwrap();
        assert_eq!(out.text(), "hi");
        assert_eq!(out.err(), "err");
        let out = run(&args(&["sleep", "5"]), None, secs(0.2), None).unwrap();
        assert!(out.timed_out);
    }
}
