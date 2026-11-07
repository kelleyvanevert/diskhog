mod report;
mod rules;
mod scan;

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Args {
    pub roots: Vec<PathBuf>,
    pub custom_roots: bool,
    pub top: usize,
    pub min: u64,
    pub depth: usize,
    pub stale_days: i64,
    pub color: bool,
}

const HELP: &str = "\
diskhog — find out where your disk space went, and how to get it back

USAGE:
    diskhog [OPTIONS] [PATH ...]

With no PATH, scans your home folder plus /Applications, /Library, Homebrew,
your /var/folders temp dir, swap and /cores. Nothing is ever deleted.

OPTIONS:
    -n, --top <N>          number of suggestions to show        [default: 25]
    -m, --min <SIZE>       hide suggestions smaller than SIZE   [default: 100MB]
    -d, --depth <N>        depth of the 'where is it' tree      [default: 3]
        --stale-days <N>   build folders older than this count as stale [default: 90]
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

pub fn human(b: u64) -> String {
    let b = b as f64;
    if b >= 1e12 {
        format!("{:.1} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{b} B")
    }
}

/// Number of Time Machine local snapshots on "/" (macOS), counted in the background.
fn local_snapshots() -> Option<usize> {
    let out = std::process::Command::new("tmutil").args(["listlocalsnapshots", "/"]).output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("com.apple.TimeMachine")).count()
    })
}

/// (total, available) bytes of the filesystem containing `p`.
fn disk_usage(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_bsize as u64;
    Some((s.f_blocks as u64 * bs, s.f_bavail as u64 * bs))
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

    let snapshots = std::thread::spawn(local_snapshots);
    let progress = scan::Progress::default();
    let done = AtomicBool::new(false);
    let start = Instant::now();
    let show_progress = std::io::stderr().is_terminal();

    let result = std::thread::scope(|sc| {
        if show_progress {
            sc.spawn(|| {
                let spin = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                let mut i = 0;
                while !done.load(Relaxed) {
                    eprint!(
                        "\r\x1b[2K{} Scanning… {} files · {}",
                        spin[i % spin.len()],
                        report::thousands(progress.files.load(Relaxed)),
                        human(progress.bytes.load(Relaxed))
                    );
                    let _ = std::io::stderr().flush();
                    i += 1;
                    std::thread::sleep(Duration::from_millis(80));
                }
                eprint!("\r\x1b[2K");
            });
        }
        let r = scan::scan(&args.roots, &skip, &progress);
        done.store(true, Relaxed);
        r
    });
    let elapsed = start.elapsed();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let ctx = rules::Context { home: home.clone(), tmp: var_folders(), now, stale_days: args.stale_days };
    let suggestions = rules::analyze(&result, &ctx);

    let info = report::RunInfo {
        disk: disk_usage(&home),
        files: progress.files.load(Relaxed),
        elapsed,
        snapshots: snapshots.join().ok().flatten(),
    };
    report::print(&args, &ctx, &result, &suggestions, &info);
}
