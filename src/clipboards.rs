//! Clipboard payloads and the Wayland adapter. Content never goes on a command line.
use crate::util::{self, Res};
use serde_json::{Value, json};

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Full text sent along with a history entry (clipdeck caches it) ...
pub const TEXT_CAP: usize = 64 * 1024;
/// ... and for the whole list.
pub const TEXT_BUDGET: usize = 2 * 1024 * 1024;

pub fn empty() -> Value {
    json!({"kind": "empty", "mime": "", "bytes": 0})
}

pub fn from_bytes(data: &[u8], mime: &str) -> Res<Value> {
    if data.len() > MAX_BYTES {
        return Err("Clipboard exceeds the 8 MiB transfer limit.".into());
    }
    if mime == "image/png" {
        if data.len() < 24 || !data.starts_with(PNG_SIGNATURE) || &data[12..16] != b"IHDR" {
            return Err("The clipboard image is not a valid PNG.".into());
        }
        let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
        if width == 0 || height == 0 || width as u64 * height as u64 > 64_000_000 {
            return Err("Clipboard image dimensions exceed the supported limit.".into());
        }
        return Ok(json!({"kind": "image", "mime": mime, "data": util::b64(data), "bytes": data.len(),
                         "width": width, "height": height}));
    }
    if !mime.starts_with("text/plain") {
        return Err("Supported clipboard formats are plain text and PNG.".into());
    }
    let text = std::str::from_utf8(data).map_err(|_| "Clipboard text is not valid UTF-8.")?;
    Ok(json!({"kind": "text", "mime": "text/plain;charset=utf-8", "text": text, "bytes": data.len()}))
}

/// A clipboard value -> (its normalized form, the raw bytes to paste).
pub fn payload_bytes(payload: &Value) -> Res<(Value, Vec<u8>)> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("text") => {
            let text = payload.get("text").and_then(Value::as_str).ok_or("Clipboard text must be a string.")?;
            let raw = text.as_bytes().to_vec();
            Ok((from_bytes(&raw, "text/plain")?, raw))
        }
        Some("image") => {
            let data = payload.get("data").and_then(Value::as_str);
            let Some(data) = data.filter(|d| d.len() <= MAX_BYTES.div_ceil(3) * 4 + 4) else {
                return Err("Clipboard image exceeds the 8 MiB transfer limit.".into());
            };
            let raw = util::unb64(data)?;
            Ok((from_bytes(&raw, "image/png")?, raw))
        }
        Some("empty") => Ok((empty(), Vec::new())),
        _ => Err("This clipboard format cannot be copied. Use plain text or PNG.".into()),
    }
}

#[cfg(unix)]
pub use wayland::*;

#[cfg(unix)]
mod wayland {
    use super::*;
    use crate::util::{Env, args, secs};

    pub fn read_wayland(env: &Env) -> Res<Value> {
        let listed = util::run(&args(&["wl-paste", "--list-types"]), None, secs(6.0), Some(env))?;
        if !listed.ok() {
            let error = listed.err();
            if error.contains("Nothing is copied") || error.contains("No selection") {
                return Ok(empty());
            }
            return Err(if error.is_empty() { "Could not read the Wayland clipboard.".into() } else { error });
        }
        let types: Vec<String> = String::from_utf8_lossy(&listed.stdout).lines().map(str::to_string).collect();
        if types.is_empty() {
            return Ok(empty());
        }
        let has = |t: &str| types.iter().any(|x| x == t);
        let mime = if has("image/png") {
            Some("image/png")
        } else {
            ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"].into_iter().find(|t| has(t))
        };
        let Some(mime) = mime else {
            return Ok(json!({"kind": "unsupported", "types": types, "bytes": 0}));
        };
        let result = util::run(&args(&["wl-paste", "--no-newline", "--type", mime]), None, secs(6.0), Some(env))?;
        if !result.ok() {
            let error = result.err();
            return Err(if error.is_empty() { "The clipboard owner stopped responding.".into() } else { error });
        }
        if result.stdout.is_empty() {
            return Ok(empty()); // an empty selection can still offer text types
        }
        from_bytes(&result.stdout, if mime == "image/png" { "image/png" } else { "text/plain" })
    }

    /// The newest cliphist entries, [{id, preview}]. with_text: text entries (valid UTF-8, up to
    /// TEXT_CAP each, TEXT_BUDGET in all) also carry their full `text`.
    pub fn cliphist_history(limit: usize, with_text: bool) -> Res<Vec<Value>> {
        let result = util::run(&args(&["cliphist", "list"]), None, secs(6.0), None)?;
        if !result.ok() {
            let error = result.err();
            return Err(if error.is_empty() { "Could not read the clipboard history.".into() } else { error });
        }
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&result.stdout).lines() {
            let (id, preview) = line.split_once('\t').unwrap_or((line, ""));
            if let Ok(id) = id.parse::<u64>() {
                entries.push(json!({"id": id, "preview": preview}));
                if entries.len() >= limit {
                    break;
                }
            }
        }
        if with_text {
            let mut budget = TEXT_BUDGET;
            for entry in entries.iter_mut() {
                let preview = entry["preview"].as_str().unwrap_or("");
                if preview.starts_with("[[ binary data") || budget == 0 {
                    continue;
                }
                let id = entry["id"].to_string();
                let Ok(r) = util::run(&args(&["cliphist", "decode", &id]), None, secs(6.0), None) else { continue };
                if !r.ok() || r.stdout.len() > TEXT_CAP.min(budget) || r.stdout.starts_with(PNG_SIGNATURE) {
                    continue;
                }
                if let Ok(text) = String::from_utf8(r.stdout) {
                    budget -= text.len();
                    entry["text"] = Value::String(text);
                }
            }
        }
        Ok(entries)
    }

    pub fn cliphist_decode(id: u64) -> Res<Vec<u8>> {
        let result = util::run(&args(&["cliphist", "decode", &id.to_string()]), None, secs(6.0), None)?;
        if !result.ok() {
            let error = result.err();
            return Err(if error.is_empty() { "That history entry is gone.".into() } else { error });
        }
        Ok(result.stdout)
    }

    /// One history entry's raw bytes (base64), exactly as `cliphist decode` gives them to wl-copy.
    pub fn cliphist_entry(value: &Value) -> Res<Value> {
        let id = value.as_u64().filter(|_| !value.is_boolean()).ok_or("Invalid clipboard history entry.")?;
        let data = cliphist_decode(id)?;
        if data.len() > MAX_BYTES {
            return Err("Clipboard exceeds the 8 MiB transfer limit.".into());
        }
        Ok(json!({"data": util::b64(&data), "bytes": data.len()}))
    }

    /// Put bytes on the Wayland clipboard. wl-copy leaves a child behind to serve the selection and
    /// it inherits stderr: a pipe there would never reach EOF, so errors go to a file instead.
    pub fn wl_copy(data: &[u8], mime: Option<&str>, clear: bool, env: Option<&Env>) -> Res<()> {
        use std::io::{Read, Seek, Write};
        use std::process::{Command, Stdio};
        let path = util::runtime_dir().join(format!("archfleet-wl-copy-{}.err", std::process::id()));
        let mut errors = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&path);
        let mut command = Command::new("wl-copy");
        if clear {
            command.arg("--clear");
        } else if let Some(mime) = mime {
            command.args(["--type", mime]);
        }
        if let Some(env) = env {
            command.envs(env.iter().map(|(k, v)| (k, v)));
        }
        command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(errors.try_clone().map_err(|e| e.to_string())?);
        let mut child = command.spawn().map_err(|e| util::spawn_error("wl-copy", e))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
        }
        use wait_timeout::ChildExt;
        let status = match child.wait_timeout(secs(6.0)).map_err(|e| e.to_string())? {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("wl-copy did not finish.".into());
            }
        };
        if !status.success() {
            let mut text = String::new();
            let _ = errors.rewind();
            let _ = errors.read_to_string(&mut text);
            let text = text.trim();
            return Err(if text.is_empty() { "Could not set the Wayland clipboard.".into() } else { text.into() });
        }
        Ok(())
    }

    pub fn write_wayland(payload: &Value, env: &Env) -> Res<Value> {
        let (normalized, data) = payload_bytes(payload)?;
        let kind = normalized["kind"].as_str().unwrap_or("");
        wl_copy(&data, normalized["mime"].as_str(), kind == "empty", Some(env))?;
        Ok(json!({"written": true, "kind": kind, "bytes": data.len()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_png() {
        let v = from_bytes("åäö".as_bytes(), "text/plain").unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["bytes"], 6);
        assert!(from_bytes(&[0xff, 0xfe], "text/plain").is_err());
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2u32.to_be_bytes());
        png.extend_from_slice(&3u32.to_be_bytes());
        let v = from_bytes(&png, "image/png").unwrap();
        assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(2), Some(3)));
        let (again, raw) = payload_bytes(&v).unwrap();
        assert_eq!(raw, png);
        assert_eq!(again["kind"], "image");
        assert!(payload_bytes(&json!({"kind": "unsupported"})).is_err());
    }
}
