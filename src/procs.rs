//! Per-process CPU, GPU, memory, disk and network on Linux.
//!
//! Rates need two readings. The previous call's reading is kept in $XDG_RUNTIME_DIR, so while the
//! Processes tab refreshes every few seconds each call answers at once, with rates over the time
//! since the last call; only a first call (or one after a long pause) samples for a second.
//! Without root, disk and GPU counters of other users' processes are unreadable (null); per-process
//! network covers TCP sockets only (ss).
use crate::helper::{cpu_sample, meminfo, read_int};
use crate::util::{self, Res, args, secs};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SAMPLE: f64 = 1.0;

#[derive(Clone, Default)]
struct Proc {
    name: String,
    ticks: u64,
    threads: u64,
    start: u64,
    rss: u64,
    uid: u32,
    io: Option<(u64, u64)>,
    gpu: Option<(HashMap<String, u64>, u64)>,
}

#[derive(Default)]
struct Snapshot {
    at: f64,
    cpu: (u64, u64),
    disk: (u64, u64),
    net: (u64, u64),
    tcp: Option<HashMap<String, (u32, u64, u64)>>,
    gpu_busy: Option<i64>,
    procs: HashMap<u32, Proc>,
}

fn page() -> u64 {
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(1) as u64
}
fn ticks_per_second() -> f64 {
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64
}

pub fn user_name(uid: u32, cache: &mut HashMap<u32, String>) -> String {
    cache
        .entry(uid)
        .or_insert_with(|| {
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut buf = vec![0 as libc::c_char; 4096];
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
            if rc == 0 && !result.is_null() { unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_string_lossy().to_string() } else { uid.to_string() }
        })
        .clone()
}

/// (name, ppid, cpu ticks, threads, start ticks, rss bytes) from /proc/pid/stat.
fn read_stat(pid: u32) -> Option<(String, u32, u64, u64, u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let fields: Vec<&str> = text.get(close + 2..)?.split_whitespace().collect();
    let num = |i: usize| fields.get(i).and_then(|v| v.parse::<u64>().ok());
    Some((text[open + 1..close].to_string(), num(1)? as u32, num(11)? + num(12)?, num(17)?, num(19)?, num(21)? * page()))
}

fn read_io(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut read = None;
    let mut write = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes: ") {
            read = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes: ") {
            write = v.trim().parse().ok();
        }
    }
    Some((read?, write?))
}

/// GPU engine time (ns) per engine and VRAM, summed over the process's DRM clients (each once).
fn read_gpu(pid: u32) -> Option<(HashMap<String, u64>, u64)> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut clients: HashMap<(String, String), (HashMap<String, u64>, u64)> = HashMap::new();
    for entry in dir.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else { continue };
        if !target.starts_with("/dev/dri/") {
            continue;
        }
        let fd = entry.file_name();
        let Ok(info) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd.to_string_lossy())) else { continue };
        let mut client = None;
        let mut pdev = String::new();
        let mut engines = HashMap::new();
        let mut vram = None;
        let mut resident = None;
        for line in info.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "drm-client-id" => client = Some(value.to_string()),
                "drm-pdev" => pdev = value.to_string(),
                "drm-memory-vram" => vram = Some(value.to_string()),
                "drm-resident-vram" => resident = Some(value.to_string()),
                _ => {
                    if let Some(engine) = key.strip_prefix("drm-engine-")
                        && !key.starts_with("drm-engine-capacity")
                        && let Some(ns) = value.split_whitespace().next().and_then(|v| v.parse().ok())
                    {
                        engines.insert(engine.to_string(), ns);
                    }
                }
            }
        }
        let Some(client) = client else { continue };
        let memory = vram.or(resident).unwrap_or_else(|| "0 KiB".into());
        let (number, unit) = memory.split_once(' ').unwrap_or((&memory, ""));
        let scale = match unit {
            "KiB" => 1024,
            "MiB" => 1024 * 1024,
            "GiB" => 1024 * 1024 * 1024,
            _ => 1,
        };
        clients.insert((pdev, client), (engines, number.parse::<u64>().unwrap_or(0) * scale));
    }
    let mut engines: HashMap<String, u64> = HashMap::new();
    let mut vram = 0;
    for (client_engines, memory) in clients.into_values() {
        for (name, value) in client_engines {
            *engines.entry(name).or_default() += value;
        }
        vram += memory;
    }
    Some((engines, vram))
}

/// Each own TCP socket ("local peer") -> (pid, bytes received, bytes sent), from `ss -tinpH`.
fn read_tcp() -> Option<HashMap<String, (u32, u64, u64)>> {
    let out = util::run(&args(&["ss", "-tinpH"]), None, secs(5.0), None).ok()?;
    out.code?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sockets = HashMap::new();
    let mut current: Option<(String, u32)> = None;
    let counter = |line: &str, name: &str| -> u64 {
        line.split_whitespace().find_map(|word| word.strip_prefix(name).and_then(|v| v.strip_prefix(':'))).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    for line in text.lines() {
        if !line.is_empty() && !line.starts_with(char::is_whitespace) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let pid = line.find(",pid=").and_then(|i| line[i + 5..].split(|c: char| !c.is_ascii_digit()).next()).and_then(|p| p.parse::<u32>().ok());
            current = match (pid, parts.len() > 4) {
                (Some(pid), true) => Some((format!("{} {}", parts[3], parts[4]), pid)),
                _ => None,
            };
        } else if let Some((key, pid)) = current.take() {
            sockets.insert(key, (pid, counter(line, "bytes_received"), counter(line, "bytes_sent")));
        }
    }
    Some(sockets)
}

/// Bytes read/written by whole physical disks (no partitions, loop, zram or device-mapper).
fn read_disks() -> (u64, u64) {
    let disks: Vec<String> = std::fs::read_dir("/sys/block")
        .map(|d| d.flatten().filter(|e| e.path().join("device").exists()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    let (mut read, mut written) = (0, 0);
    for line in std::fs::read_to_string("/proc/diskstats").unwrap_or_default().lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 9 && disks.iter().any(|d| d == fields[2]) {
            read += fields[5].parse::<u64>().unwrap_or(0) * 512;
            written += fields[9].parse::<u64>().unwrap_or(0) * 512;
        }
    }
    (read, written)
}

fn read_network() -> (u64, u64) {
    let (mut rx, mut tx) = (0, 0);
    for line in std::fs::read_to_string("/proc/net/dev").unwrap_or_default().lines().skip(2) {
        let Some((name, values)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = values.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if fields.len() > 8 {
            rx += fields[0];
            tx += fields[8];
        }
    }
    (rx, tx)
}

fn gpu_card() -> Option<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|e| e.path().join("device"))
        .collect();
    cards.sort();
    cards.into_iter().find(|d| d.join("gpu_busy_percent").exists())
}

fn take(card: Option<&Path>) -> Snapshot {
    use std::os::unix::fs::MetadataExt;
    let mut procs = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Some((name, ppid, ticks, threads, start, rss)) = read_stat(pid) else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            if pid == 2 || ppid == 2 {
                continue; // kernel threads
            }
            procs.insert(pid, Proc { name, ticks, threads, start, rss, uid: meta.uid(), io: read_io(pid), gpu: read_gpu(pid) });
        }
    }
    Snapshot {
        at: util::now_secs(),
        cpu: cpu_sample(),
        disk: read_disks(),
        net: read_network(),
        tcp: read_tcp(),
        gpu_busy: card.and_then(|c| read_int(&c.join("gpu_busy_percent"))),
        procs,
    }
}

// ---------- the previous reading, between calls ----------

fn cache_path() -> PathBuf {
    util::runtime_dir().join("archfleet-processes.sample")
}

fn store(s: &Snapshot) {
    let procs: Map<String, Value> = s
        .procs
        .iter()
        .map(|(pid, p)| {
            let gpu = p.gpu.as_ref().map(|(engines, _)| json!(engines));
            (pid.to_string(), json!([p.start, p.ticks, p.io.map(|i| [i.0, i.1]), gpu]))
        })
        .collect();
    let tcp = s.tcp.as_ref().map(|t| t.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1, v.2]))).collect::<Map<_, _>>());
    let data = json!({"at": s.at, "cpu": [s.cpu.0, s.cpu.1], "disk": [s.disk.0, s.disk.1], "net": [s.net.0, s.net.1],
                      "tcp": tcp, "gpu_busy": s.gpu_busy, "procs": procs});
    let _ = util::write_private(&cache_path(), data.to_string().as_bytes());
}

fn load() -> Option<Snapshot> {
    let v: Value = serde_json::from_slice(&std::fs::read(cache_path()).ok()?).ok()?;
    let pair = |key: &str| Some((v[key][0].as_u64()?, v[key][1].as_u64()?));
    let mut procs = HashMap::new();
    for (pid, p) in v["procs"].as_object()? {
        let gpu = p[3].as_object().map(|e| (e.iter().map(|(k, n)| (k.clone(), n.as_u64().unwrap_or(0))).collect(), 0));
        let io = p[2].as_array().and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?)));
        procs.insert(pid.parse().ok()?, Proc { start: p[0].as_u64()?, ticks: p[1].as_u64()?, io, gpu, ..Default::default() });
    }
    let tcp = v["tcp"].as_object().map(|t| t.iter().filter_map(|(k, x)| Some((k.clone(), (x[0].as_u64()? as u32, x[1].as_u64()?, x[2].as_u64()?)))).collect());
    Some(Snapshot { at: v["at"].as_f64()?, cpu: pair("cpu")?, disk: pair("disk")?, net: pair("net")?, tcp, gpu_busy: v["gpu_busy"].as_i64(), procs })
}

fn rate(after: u64, before: u64, interval: f64) -> f64 {
    after.saturating_sub(before) as f64 / interval
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}

fn display_name(pid: u32, comm: &str) -> (String, String) {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return (comm.to_string(), String::new()) };
    let parts: Vec<String> = raw.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).to_string()).collect();
    let command = util::cut(&parts.join(" "), 300);
    let base = raw.split(|&b| b == 0).next().map(|a| String::from_utf8_lossy(a).rsplit('/').next().unwrap_or("").to_string()).unwrap_or_default();
    // comm is cut at 15 characters; prefer the full executable name when it matches.
    let name = if comm.len() == 15 && base.starts_with(comm) { base } else { comm.to_string() };
    (name, command)
}

pub fn collect() -> Res<Value> {
    let card = gpu_card();
    let mut after = take(card.as_deref());
    let before = match load().filter(|b| (0.5..=10.0).contains(&(after.at - b.at))) {
        Some(before) => before,
        None => {
            std::thread::sleep(secs(SAMPLE));
            std::mem::replace(&mut after, take(card.as_deref()))
        }
    };
    store(&after);
    let interval = (after.at - before.at).max(0.001);
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let own = util::uid();
    let hz = ticks_per_second();

    let mut tcp: HashMap<u32, (u64, u64)> = HashMap::new();
    if let Some(now) = &after.tcp {
        for (key, (pid, received, sent)) in now {
            let (old_rx, old_tx) = match before.tcp.as_ref().and_then(|t| t.get(key)) {
                Some((old_pid, rx, tx)) if old_pid == pid => (*rx, *tx),
                _ => (0, 0),
            };
            let total = tcp.entry(*pid).or_default();
            total.0 += received.saturating_sub(old_rx);
            total.1 += sent.saturating_sub(old_tx);
        }
    }

    let mut users = HashMap::new();
    let mut rows = Vec::with_capacity(after.procs.len());
    for (pid, new) in &after.procs {
        let old = before.procs.get(pid).filter(|o| o.start == new.start).unwrap_or(new);
        let mine = new.uid == own;
        let io = match (new.io, old.io) {
            (Some(n), Some(o)) => Some((rate(n.0, o.0, interval), rate(n.1, o.1, interval))),
            _ => None,
        };
        let (gpu, vram) = match &new.gpu {
            Some((engines, vram)) => {
                let empty = HashMap::new();
                let old_engines = old.gpu.as_ref().map(|g| &g.0).unwrap_or(&empty);
                let busy: f64 =
                    engines.iter().map(|(name, value)| rate(*value, *old_engines.get(name).unwrap_or(value), interval) / 1e9 * 100.0).fold(0.0, |a, b| a + b);
                (Some(round(busy.min(100.0), 1)), Some(*vram))
            }
            None => (None, None),
        };
        let network = (after.tcp.is_some() && mine).then(|| tcp.get(pid).copied().unwrap_or((0, 0)));
        let (name, command) = display_name(*pid, &new.name);
        rows.push(json!({
            "pid": pid, "name": name, "user": user_name(new.uid, &mut users), "own": mine, "start": new.start,
            "cpu": round(rate(new.ticks, old.ticks, interval) / hz * 100.0 / cpus, 2),
            "memory": new.rss, "gpu": gpu, "vram": vram,
            "disk_read": io.map(|i| i.0.round()), "disk_write": io.map(|i| i.1.round()),
            "net_rx": network.map(|n| (n.0 as f64 / interval).round()), "net_tx": network.map(|n| (n.1 as f64 / interval).round()),
            "threads": new.threads, "command": command,
        }));
    }

    let total = after.cpu.0.saturating_sub(before.cpu.0);
    let idle = after.cpu.1.saturating_sub(before.cpu.1);
    let (memory_used, memory_total) = meminfo();
    let busy: Vec<i64> = [before.gpu_busy, after.gpu_busy].into_iter().flatten().collect();
    Ok(json!({
        "platform": "linux", "interval": round(interval, 3), "cpus": cpus as u64, "processes": rows,
        "tcp_available": after.tcp.is_some(),
        "totals": {
            "cpu": if total > 0 { round(100.0 * (1.0 - idle as f64 / total as f64), 1) } else { 0.0 },
            "memory_used": memory_used, "memory_total": memory_total,
            "gpu": (!busy.is_empty()).then(|| round(busy.iter().sum::<i64>() as f64 / busy.len() as f64, 1)),
            "vram_used": card.as_ref().and_then(|c| read_int(&c.join("mem_info_vram_used"))),
            "vram_total": card.as_ref().and_then(|c| read_int(&c.join("mem_info_vram_total"))),
            "disk_read": rate(after.disk.0, before.disk.0, interval).round(),
            "disk_write": rate(after.disk.1, before.disk.1, interval).round(),
            "net_rx": rate(after.net.0, before.net.0, interval).round(),
            "net_tx": rate(after.net.1, before.net.1, interval).round(),
        },
    }))
}

/// SIGTERM one of this user's processes, after checking it is still the same process.
pub fn terminate(value: &Value) -> Res<Value> {
    use std::os::unix::fs::MetadataExt;
    if !value.is_object() {
        return Err("Choose a process first.".into());
    }
    let pid = value["pid"].as_u64().ok_or("Invalid process.")?;
    let start = value["start"].as_u64().ok_or("Invalid process.")?;
    if pid <= 1 || pid > i32::MAX as u64 {
        return Err("Invalid process.".into());
    }
    let pid = pid as u32;
    let (name, _, _, _, current_start, _) = read_stat(pid).ok_or("That process has already exited.")?;
    let owner = std::fs::metadata(format!("/proc/{pid}")).map_err(|_| "That process has already exited.")?.uid();
    if current_start != start {
        return Err("That process has already exited (its PID now belongs to another process).".into());
    }
    if owner != util::uid() {
        return Err("Only your own processes can be ended without root.".into());
    }
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(json!({"code": 0, "text": format!("Sent SIGTERM to {name} ({pid}).")}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_own_process() {
        let data = collect().unwrap();
        let me = std::process::id() as u64;
        let rows = data["processes"].as_array().unwrap();
        assert!(rows.iter().any(|p| p["pid"].as_u64() == Some(me) && p["own"] == true));
        assert!(data["totals"]["memory_total"].as_u64().unwrap() > 0);
        // a second call right after uses the stored reading
        let again = collect().unwrap();
        assert!(again["interval"].as_f64().unwrap() < 5.0);
    }

    #[test]
    fn terminate_checks() {
        assert!(terminate(&json!(null)).is_err());
        assert!(terminate(&json!({"pid": 1, "start": 0})).is_err());
        let me = std::process::id();
        assert!(terminate(&json!({"pid": me, "start": 1})).unwrap_err().contains("already exited"));
    }
}
