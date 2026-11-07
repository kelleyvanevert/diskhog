mod actions;
mod providers;
mod report;
mod rules;
mod scan;
mod tui;
mod util;

pub use util::human;

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Args {
    pub roots: Vec<PathBuf>,
    pub custom_roots: bool,
    pub top: usize,
    pub min: u64,
    pub depth: usize,
    pub stale_days: i64,
    pub color: bool,
    pub report: bool,
}

const HELP: &str = "\
diskhog — find out where your disk space went, and how to get it back

USAGE:
    diskhog [OPTIONS] [PATH ...]

With no PATH, scans your home folder plus /Applications, /Library, Homebrew,
your /var/folders temp dir, swap and /cores.

In a terminal it opens an interactive full-screen UI that shows results while
scanning and lets you select and remove items (press ? inside for keys).
Nothing is removed without you confirming it. When output is piped, or with
--report, it prints a static report instead.

OPTIONS:
    -p, --report           print a report instead of the interactive UI
    -n, --top <N>          report: number of suggestions to show       [default: 25]
    -m, --min <SIZE>       report: hide suggestions smaller than SIZE  [default: 100MB]
    -d, --depth <N>        report: depth of the 'where is it' tree     [default: 3]
        --stale-days <N>   folders older than this count as stale     [default: 90]
        --no-color         disable colors (also honours NO_COLOR)
    -h, --help             show this help

SIZE accepts e.g. 500MB, 2G, 1.5GB.";

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mul = match unit.trim().trim_end_matches('B') {
        "" => 1.0,
        "K" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        _ => return None,
    };
    Some((n * mul) as u64)
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        roots: vec![],
        custom_roots: false,
        top: 25,
        min: 100_000_000,
        depth: 3,
        stale_days: 90,
        color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        report: !(std::io::stdout().is_terminal() && std::io::stdin().is_terminal()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            "-n" | "--top" => a.top = val(&arg)?.parse().map_err(|_| "--top expects a number")?,
            "-d" | "--depth" => a.depth = val(&arg)?.parse().map_err(|_| "--depth expects a number")?,
            "--stale-days" => a.stale_days = val(&arg)?.parse().map_err(|_| "--stale-days expects a number")?,
            "-m" | "--min" => a.min = parse_size(&val(&arg)?).ok_or("--min expects a size like 500MB")?,
            "--no-color" => a.color = false,
            "-p" | "--report" => a.report = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s} (see --help)")),
            p => a.roots.push(PathBuf::from(p)),
        }
    }
    a.custom_roots = !a.roots.is_empty();
    Ok(a)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The per-user dir under /var/folders (parent of $TMPDIR), macOS only.
fn var_folders() -> Option<PathBuf> {
    let t = PathBuf::from(std::env::var_os("TMPDIR")?);
    let t = t.components().collect::<PathBuf>();
    if t.starts_with("/var/folders") || t.starts_with("/private/var/folders") {
        t.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn default_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.to_path_buf()];
    for p in ["/Applications", "/Library", "/opt/homebrew", "/usr/local", "/private/var/vm", "/cores", "/Users/Shared"] {
        roots.push(PathBuf::from(p));
    }
    roots.extend(var_folders());
    roots
}

/// Drop missing roots and roots nested inside another root.
fn normalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .filter(|r| r.exists())
        .map(|r| if r.is_absolute() { r } else { std::env::current_dir().unwrap().join(r) })
        .map(|r| r.components().collect())
        .collect();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in &roots {
        if !roots.iter().any(|o| o != r && r.starts_with(o)) && !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}

/// Number of Time Machine local snapshots on "/" (macOS), counted in the background.
fn local_snapshots() -> Option<usize> {
    let out = std::process::Command::new("tmutil").args(["listlocalsnapshots", "/"]).output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("com.apple.TimeMachine")).count()
    })
}

/// (total, available) bytes of the filesystem containing `p`.
pub fn disk_usage(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_bsize as u64;
    Some((s.f_blocks as u64 * bs, s.f_bavail as u64 * bs))
}

/// Where we remember how many bytes the last full scan saw, for the progress %.
fn expected_file(home: &Path, roots: &[PathBuf]) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    roots.hash(&mut h);
    let base = if home.join("Library/Caches").is_dir() { home.join("Library/Caches") } else { home.join(".cache") };
    base.join("diskhog").join(format!("last-total-{:x}", h.finish()))
}

fn main() {
    let mut args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("diskhog: {e}");
            std::process::exit(2);
        }
    };
    let home = home();
    let raw = if args.custom_roots { std::mem::take(&mut args.roots) } else { default_roots(&home) };
    args.roots = normalize_roots(raw);
    if args.roots.is_empty() {
        eprintln!("diskhog: nothing to scan");
        std::process::exit(2);
    }

    // Scanning "/" would otherwise double count the data volume and wander into mounts.
    let skip: HashSet<PathBuf> = ["/System/Volumes", "/Volumes", "/dev", "/net", "/home"]
        .iter()
        .map(PathBuf::from)
        .collect();

    // The walk is bound by filesystem metadata calls; beyond one thread per core
    // APFS lock contention eats the gains (measured), so stick to the core count.
    let threads = std::env::var("DISKHOG_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));
    let _ = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let ctx = rules::Context { home: home.clone(), tmp: var_folders(), now, stale_days: args.stale_days };

    let sh = Arc::new(scan::Shared::default());
    let targets = rules::hotspot_targets(&ctx, &args.roots);
    {
        let (sh, home) = (sh.clone(), home.clone());
        std::thread::spawn(move || {
            let times = actions::app_launch_times(&home);
            sh.data.lock().unwrap().last_used = times;
            sh.bump();
        });
    }
    if !args.custom_roots {
        providers::spawn_all(&sh);
    }
    let start = Instant::now();
    {
        let (sh, roots) = (sh.clone(), args.roots.clone());
        std::thread::spawn(move || scan::run(&sh, &roots, targets, &skip));
    }

    let cache = expected_file(&home, &args.roots);
    let expected = std::fs::read_to_string(&cache)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| disk_usage(&home).map(|(t, a)| t - a))
        .unwrap_or(1);
    let save = move |total: u64| {
        let _ = std::fs::create_dir_all(cache.parent().unwrap());
        let _ = std::fs::write(&cache, total.to_string());
    };

    if !args.report {
        let app = tui::App::new(sh, ctx, args.roots.clone(), expected, Box::new(save));
        if let Err(e) = app.run() {
            eprintln!("diskhog: {e}");
        }
        // Don't wait for a scan that's still running.
        std::process::exit(0);
    }

    // Report mode: wait for the scan with a progress line, then print.
    let show_progress = std::io::stderr().is_terminal();
    let spin = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut i = 0;
    while sh.phase.load(Relaxed) != scan::PHASE_DONE {
        if show_progress {
            let bytes = sh.bytes.load(Relaxed);
            eprint!(
                "\r\x1b[2K{} Scanning… {:>3}%  {} files · {}",
                spin[i % spin.len()],
                (bytes * 100 / expected.max(1)).min(99),
                util::thousands(sh.files.load(Relaxed)),
                human(bytes)
            );
            let _ = std::io::stderr().flush();
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(80));
    }
    if show_progress {
        eprint!("\r\x1b[2K");
    }
    let elapsed = start.elapsed();
    save(sh.bytes.load(Relaxed));
    let data = sh.data.lock().unwrap();
    let cats = rules::analyze(&data, &ctx);
    let info = report::RunInfo {
        disk: disk_usage(&home),
        files: sh.files.load(Relaxed),
        errors: sh.errors.load(Relaxed),
        elapsed,
        snapshots: local_snapshots(),
    };
    report::print(&args, &ctx, &data, &cats, &info);
}
