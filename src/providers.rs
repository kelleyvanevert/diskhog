//! Providers: categories whose contents are better described by a tool than by
//! the filesystem. Docker's disk image is one opaque 100+ GB file, but Docker knows
//! which images, containers, volumes and cache entries are inside, how big they are
//! and when they were last used, and can remove each of them properly.
//! The same goes for simulators and `xcrun simctl`.

use crate::scan::Shared;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Rule titles the providers attach to (see rules.rs).
pub const DOCKER: &str = "Docker Desktop disk image";
pub const SIM_DEVICES: &str = "iOS Simulator devices";
pub const SIM_RUNTIMES: &str = "Simulator runtimes (iOS/watchOS/visionOS)";
pub const ALL: &[&str] = &[DOCKER, SIM_DEVICES, SIM_RUNTIMES];

#[derive(Clone, Debug)]
pub struct PItem {
    /// Identity in the UI. A real path where there is one (simulators), otherwise
    /// a synthetic `docker://kind/id`.
    pub path: PathBuf,
    pub label: String,
    pub size: u64,
    pub mtime: Option<i64>,
    pub selectable: bool,
    pub note: Option<String>,
    /// Command that removes this item.
    pub cmd: Vec<String>,
    /// Removal order within a batch (containers before the images they use).
    pub order: u8,
}

#[derive(Clone, Debug)]
pub enum PState {
    Loading,
    Ready(Vec<PItem>),
    Unavailable(String),
}

pub fn spawn_all(sh: &Arc<Shared>) {
    for &p in ALL {
        sh.data.lock().unwrap().providers.insert(p, PState::Loading);
        refresh(sh, p);
    }
}

/// Re-query a provider in the background. Old data stays visible meanwhile.
pub fn refresh(sh: &Arc<Shared>, provider: &'static str) {
    let sh = sh.clone();
    std::thread::spawn(move || {
        let state = match provider {
            DOCKER => docker(),
            SIM_DEVICES => sim_devices(),
            SIM_RUNTIMES => sim_runtimes(),
            _ => return,
        };
        sh.data.lock().unwrap().providers.insert(provider, state);
        sh.bump();
    });
}

/// Drop an item right after it was removed, before the provider is re-queried.
pub fn forget(sh: &Shared, provider: &str, path: &std::path::Path) {
    if let Some(PState::Ready(items)) = sh.data.lock().unwrap().providers.get_mut(provider) {
        items.retain(|i| i.path != path);
    }
    sh.bump();
}

/// Run a command with a timeout; Ok(stdout) on success, Err(short message) otherwise.
pub fn run(args: &[String], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("`{}` not found", args[0]),
            _ => e.to_string(),
        })?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return Err(format!("`{}` timed out", args.join(" ")));
            }
        }
    };
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let msg = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string();
        Err(msg)
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn json(args: &[&str], timeout: u64) -> Result<Value, String> {
    let out = run(&argv(args), Duration::from_secs(timeout))?;
    serde_json::from_str(&out).map_err(|e| format!("unexpected output from {}: {e}", args[0]))
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

// ── Docker ────────────────────────────────────────────────────────────────

fn docker() -> PState {
    let df = match json(&["docker", "system", "df", "-v", "--format", "json"], 180) {
        Ok(v) => v,
        Err(e) if e.contains("not found") => return PState::Unavailable("Docker CLI not found".into()),
        Err(e) if e.contains("connect") || e.contains("daemon") => {
            return PState::Unavailable(
                "Docker isn't running. Start Docker Desktop to see images, containers and volumes".into(),
            );
        }
        Err(e) => return PState::Unavailable(format!("Docker: {e}")),
    };
    let arr = |k: &str| df.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let (images, containers, volumes, cache) = (arr("Images"), arr("Containers"), arr("Volumes"), arr("BuildCache"));

    // Container details: which image and volumes each uses, and when it last ran.
    struct Ctr {
        image: String,
        running: bool,
        last: Option<i64>,
        volumes: Vec<String>,
    }
    let mut ctrs: HashMap<String, Ctr> = HashMap::new();
    let ids: Vec<&str> = containers.iter().map(|c| s(c, "ID")).filter(|i| !i.is_empty()).collect();
    if !ids.is_empty() {
        let mut args = vec!["docker", "container", "inspect"];
        args.extend(&ids);
        if let Ok(Value::Array(list)) = json(&args, 60) {
            for c in list {
                let state = c.get("State").cloned().unwrap_or(Value::Null);
                let running = state.get("Running").and_then(Value::as_bool).unwrap_or(false);
                let last = [s(&state, "StartedAt"), s(&state, "FinishedAt")].iter().filter_map(|t| parse_time(t)).max();
                let volumes = c
                    .get("Mounts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|m| s(m, "Type") == "volume")
                    .map(|m| s(m, "Name").to_string())
                    .collect();
                ctrs.insert(
                    s(&c, "Id").to_string(),
                    Ctr { image: s(&c, "Image").to_string(), running, last: if running { Some(now()) } else { last }, volumes },
                );
            }
        }
    }

    // When each image was last pulled/tagged.
    let mut tagged: HashMap<String, i64> = HashMap::new();
    let image_ids: HashSet<&str> = images.iter().map(|i| s(i, "ID")).filter(|i| !i.is_empty()).collect();
    if !image_ids.is_empty() {
        let mut args = vec!["docker", "image", "inspect"];
        args.extend(image_ids.iter());
        if let Ok(Value::Array(list)) = json(&args, 60) {
            for i in list {
                if let Some(t) = i.get("Metadata").map(|m| s(m, "LastTagTime")).and_then(parse_time) {
                    tagged.insert(s(&i, "Id").to_string(), t);
                }
            }
        }
    }

    // When each volume was created.
    let mut vol_created: HashMap<String, i64> = HashMap::new();
    let vol_names: Vec<&str> = volumes.iter().map(|v| s(v, "Name")).filter(|n| !n.is_empty()).collect();
    if !vol_names.is_empty() {
        let mut args = vec!["docker", "volume", "inspect"];
        args.extend(&vol_names);
        if let Ok(Value::Array(list)) = json(&args, 60) {
            for v in list {
                if let Some(t) = parse_time(s(&v, "CreatedAt")) {
                    vol_created.insert(s(&v, "Name").to_string(), t);
                }
            }
        }
    }

    let mut items = Vec::new();

    for c in &containers {
        let id = s(c, "ID");
        let info = ctrs.get(id);
        let running = info.is_some_and(|i| i.running) || s(c, "State") == "running";
        items.push(PItem {
            path: PathBuf::from(format!("docker://container/{id}")),
            label: format!("container   {}  ({})", s(c, "Names"), s(c, "Image")),
            size: parse_size(s(c, "Size")),
            mtime: info.and_then(|i| i.last).or_else(|| parse_time(s(c, "CreatedAt"))),
            selectable: !running,
            note: running.then(|| "running".into()),
            cmd: argv(&["docker", "rm", id]),
            order: 0,
        });
    }

    // Images: one item per image ID (an image can have several tags).
    let mut seen = HashSet::new();
    for img in &images {
        let id = s(img, "ID");
        if !seen.insert(id) {
            continue;
        }
        let tags: Vec<String> = images
            .iter()
            .filter(|i| s(i, "ID") == id && s(i, "Repository") != "<none>")
            .map(|i| format!("{}:{}", s(i, "Repository"), s(i, "Tag")))
            .collect();
        let users: Vec<&Ctr> = ctrs.values().filter(|c| c.image == id).collect();
        let in_use = users.iter().any(|c| c.running);
        let last = users.iter().filter_map(|c| c.last).chain(tagged.get(id).copied()).max();
        let name = match tags.as_slice() {
            [] => format!("<untagged> {}", &id.trim_start_matches("sha256:")[..12.min(id.len())]),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first}  (+{} tags)", rest.len()),
        };
        let mut cmd = argv(&["docker", "rmi"]);
        if tags.is_empty() { cmd.push(id.to_string()) } else { cmd.extend(tags.iter().cloned()) }
        items.push(PItem {
            path: PathBuf::from(format!("docker://image/{id}")),
            label: format!("image       {name}"),
            size: parse_size(s(img, "UniqueSize")),
            mtime: last.or_else(|| parse_time(s(img, "CreatedAt"))),
            selectable: !in_use,
            note: if in_use {
                Some("used by a running container".into())
            } else if !users.is_empty() {
                Some(format!("used by {} stopped container(s)", users.len()))
            } else {
                None
            },
            cmd,
            order: 1,
        });
    }

    for v in &volumes {
        let name = s(v, "Name");
        let users: Vec<&Ctr> = ctrs.values().filter(|c| c.volumes.iter().any(|n| n == name)).collect();
        let in_use = users.iter().any(|c| c.running);
        let anonymous = name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit());
        let project = s(v, "Labels")
            .split(',')
            .find_map(|kv| kv.strip_prefix("com.docker.compose.project="))
            .map(str::to_string);
        let display = if anonymous { format!("<anonymous> {}", &name[..12]) } else { name.to_string() };
        let note = if in_use {
            Some("used by a running container".into())
        } else if s(v, "Links") == "0" {
            Some(match &project {
                Some(p) => format!("not used by any container · from {p}"),
                None => "not used by any container".into(),
            })
        } else {
            None
        };
        items.push(PItem {
            path: PathBuf::from(format!("docker://volume/{name}")),
            label: format!("volume      {display}"),
            size: parse_size(s(v, "Size")),
            mtime: users.iter().filter_map(|c| c.last).max().or_else(|| vol_created.get(name).copied()),
            selectable: !in_use,
            note,
            cmd: argv(&["docker", "volume", "rm", name]),
            order: 2,
        });
    }

    // Build cache: entries >= 10 MB individually (docker accepts one id per prune);
    // the many tiny ones as a single row, cleaned with the prune options.
    let (mut small_n, mut small_size) = (0, 0);
    for b in &cache {
        let size = parse_size(s(b, "Size"));
        if s(b, "InUse") == "true" {
            continue;
        }
        if size < 10_000_000 {
            small_n += 1;
            small_size += size;
            continue;
        }
        let id = s(b, "ID");
        let desc = s(b, "Description").trim_start_matches("mount / from exec ").replace("/bin/sh -c ", "");
        items.push(PItem {
            path: PathBuf::from(format!("docker://cache/{id}")),
            label: format!("build cache {desc}"),
            size,
            mtime: parse_time(s(b, "LastUsedAt")).or_else(|| parse_time(s(b, "CreatedAt"))),
            selectable: true,
            note: None,
            cmd: vec!["docker".into(), "builder".into(), "prune".into(), "-f".into(), "--filter".into(), format!("id={id}")],
            order: 3,
        });
    }
    if small_n > 0 {
        items.push(PItem {
            path: PathBuf::from("docker://cache/small"),
            label: format!("build cache {small_n} small entries"),
            size: small_size,
            mtime: None,
            selectable: false,
            note: Some("use x › build cache prune".into()),
            cmd: vec![],
            order: 3,
        });
    }
    items.retain(|i| i.size > 0);
    PState::Ready(items)
}

// ── Simulators ────────────────────────────────────────────────────────────

/// "com.apple.CoreSimulator.SimRuntime.iOS-18-5" -> "iOS 18.5"
fn runtime_name(id: &str) -> String {
    let short = id.rsplit('.').next().unwrap_or(id);
    match short.split_once('-') {
        Some((os, ver)) => format!("{os} {}", ver.replace('-', ".")),
        None => short.to_string(),
    }
}

fn sim_devices() -> PState {
    let v = match json(&["xcrun", "simctl", "list", "devices", "--json"], 60) {
        Ok(v) => v,
        Err(e) => return PState::Unavailable(format!("simctl: {e}")),
    };
    let mut items = Vec::new();
    for (runtime, devs) in v.get("devices").and_then(Value::as_object).into_iter().flatten() {
        for d in devs.as_array().into_iter().flatten() {
            let udid = s(d, "udid");
            let booted = s(d, "state") == "Booted";
            let available = d.get("isAvailable").and_then(Value::as_bool).unwrap_or(true);
            let size = d.get("dataPathSize").and_then(Value::as_u64).unwrap_or(0)
                + d.get("logPathSize").and_then(Value::as_u64).unwrap_or(0);
            let data_path = PathBuf::from(s(d, "dataPath"));
            items.push(PItem {
                path: data_path.parent().map(|p| p.to_path_buf()).unwrap_or(data_path),
                label: format!("{}  ·  {}", s(d, "name"), runtime_name(runtime)),
                size,
                mtime: parse_time(s(d, "lastBootedAt")),
                selectable: !booted,
                note: if booted {
                    Some("running".into())
                } else if !available {
                    Some("its runtime is no longer installed".into())
                } else {
                    None
                },
                cmd: argv(&["xcrun", "simctl", "delete", udid]),
                order: 0,
            });
        }
    }
    items.retain(|i| i.size > 0);
    PState::Ready(items)
}

fn sim_runtimes() -> PState {
    let v = match json(&["xcrun", "simctl", "runtime", "list", "-j"], 60) {
        Ok(v) => v,
        Err(e) => return PState::Unavailable(format!("simctl: {e}")),
    };
    let items = v
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, r)| PItem {
            path: PathBuf::from(format!("sim://runtime/{id}")),
            label: format!("{}  ({})", runtime_name(s(r, "runtimeIdentifier")), s(r, "build")),
            size: r.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0),
            mtime: parse_time(s(r, "lastUsedAt")),
            selectable: r.get("deletable").and_then(Value::as_bool).unwrap_or(false),
            note: None,
            cmd: argv(&["xcrun", "simctl", "runtime", "delete", id]),
            order: 0,
        })
        .collect();
    PState::Ready(items)
}

// ── Parsing helpers ───────────────────────────────────────────────────────

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Docker's human sizes: "829MB", "2.145GB", "1.76kB", "0B" (decimal units).
pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..end].parse().unwrap_or(0.0);
    let unit = s[end..].split_whitespace().next().unwrap_or("B");
    let mul = match unit {
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => 1.0,
    };
    (n * mul) as u64
}

/// Timestamps as Docker and simctl print them: "2026-11-03T22:52:01.64Z",
/// "2026-11-04 14:16:31 +0100 CET", "2026-10-27 21:25:41.17 +0000 UTC".
pub fn parse_time(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if y < 1971 {
        return None; // "0001-01-01T00:00:00Z" means never
    }
    let mut t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit()).trim_start();
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let digits: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).take(4).collect();
        if digits.len() == 4 {
            let off = digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60;
            t -= if sign == '+' { off } else { -off };
        }
    }
    Some(t)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("829MB"), 829_000_000);
        assert_eq!(parse_size("2.145GB"), 2_145_000_000);
        assert_eq!(parse_size("1.76kB"), 1760);
        assert_eq!(parse_size("0B"), 0);
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), None);
        assert_eq!(parse_time("2026-11-03T22:52:01.642682215Z"), Some(1_793_746_321));
        assert_eq!(parse_time("2026-11-03 23:52:01 +0100 CET"), Some(1_793_746_321));
        assert_eq!(parse_time("2026-11-03 22:52:01.177553131 +0000 UTC"), Some(1_793_746_321));
        assert_eq!(parse_time("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_time(""), None);
    }

    #[test]
    fn runtimes() {
        assert_eq!(runtime_name("com.apple.CoreSimulator.SimRuntime.iOS-18-5"), "iOS 18.5");
        assert_eq!(runtime_name("com.apple.CoreSimulator.SimRuntime.watchOS-11-5"), "watchOS 11.5");
    }
}
