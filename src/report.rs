use crate::rules::{Context, Safety, Suggestion};
use crate::scan::ScanResult;
use crate::{Args, human};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

pub struct RunInfo {
    pub disk: Option<(u64, u64)>,
    pub files: u64,
    pub elapsed: Duration,
    pub snapshots: Option<usize>,
}

struct Style(bool);

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(&self, s: &str) -> String { self.paint("1", s) }
    fn dim(&self, s: &str) -> String { self.paint("2", s) }
    fn cyan(&self, s: &str) -> String { self.paint("36", s) }
    fn green(&self, s: &str) -> String { self.paint("32", s) }
    fn yellow(&self, s: &str) -> String { self.paint("33", s) }
    fn heading(&self, s: &str) -> String { self.paint("1;4", s) }
    /// Size, colored by how much it matters.
    fn size(&self, b: u64, width: usize) -> String {
        let s = format!("{:>width$}", human(b));
        if b >= 10_000_000_000 {
            self.paint("1;31", &s)
        } else if b >= 1_000_000_000 {
            self.paint("1;33", &s)
        } else {
            s
        }
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn display_path(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn truncate_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max - 1;
    let head: String = s.chars().take(keep / 3).collect();
    let tail: String = s.chars().skip(n - (keep - keep / 3)).collect();
    format!("{head}…{tail}")
}

fn age(mtime: i64, now: i64) -> String {
    let days = (now - mtime).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1..=13 => format!("{days}d ago"),
        14..=59 => format!("{}w ago", days / 7),
        60..=729 => format!("{}mo ago", days / 30),
        _ => format!("{}y ago", days / 365),
    }
}

fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn print(args: &Args, ctx: &Context, scan: &ScanResult, sugg: &[Suggestion], info: &RunInfo) {
    let st = Style(args.color);
    let home = &ctx.home;
    let scanned: u64 = scan.roots.iter().map(|(_, s)| s).sum();

    // ── Header ────────────────────────────────────────────────────────────
    println!();
    println!("{}", st.bold("diskhog — where did my disk space go?"));
    println!();
    if let Some((total, avail)) = info.disk {
        let used = total.saturating_sub(avail);
        let frac = used as f64 / total.max(1) as f64;
        let free_str = format!("{} free", human(avail));
        let free_str = if frac > 0.9 { st.paint("1;31", &free_str) } else { st.green(&free_str) };
        println!(
            "  Disk     {} total · {} used · {}   {} {:.0}%",
            human(total),
            human(used),
            free_str,
            bar(frac, 20),
            frac * 100.0
        );
    }
    println!(
        "  Scanned  {} files · {} in {:.1}s  {}",
        thousands(info.files),
        human(scanned),
        info.elapsed.as_secs_f64(),
        st.dim(&format!("({} locations)", scan.roots.len()))
    );

    // ── Tree ──────────────────────────────────────────────────────────────
    println!();
    println!("{}", st.heading("WHERE THE SPACE IS"));
    let mut children: HashMap<&Path, Vec<(&Path, u64)>> = HashMap::new();
    for (p, &s) in &scan.dirs {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push((p.as_path(), s));
        }
    }
    for v in children.values_mut() {
        v.sort_by(|a, b| b.1.cmp(&a.1));
    }
    let mut roots: Vec<_> = scan.roots.iter().filter(|(_, s)| *s >= 100_000_000).collect();
    roots.sort_by(|a, b| b.1.cmp(&a.1));
    let mut lines: Vec<(String, u64, f64)> = Vec::new();
    for (root, size) in &roots {
        lines.push((display_path(root, home), *size, 1.0));
        let min = (*size / 40).max(1_000_000_000);
        tree(root, *size, min, &children, "", 1, args.depth, &mut lines);
    }
    let width = lines.iter().map(|l| l.0.chars().count()).max().unwrap_or(0).min(64);
    for (label, size, frac) in &lines {
        let label = truncate_middle(label, 64);
        let pad = width.saturating_sub(label.chars().count());
        println!("  {label}{}  {}  {}", " ".repeat(pad), st.size(*size, 8), st.dim(&bar(*frac, 12)));
    }

    // ── Suggestions ───────────────────────────────────────────────────────
    println!();
    println!("{}  {}", st.heading("CLEANUP SUGGESTIONS"), st.dim("(biggest wins first)"));
    let shown: Vec<&Suggestion> = sugg.iter().filter(|s| s.size >= args.min).take(args.top).collect();
    if shown.is_empty() {
        println!("  Nothing above {} found. Nice and tidy!", human(args.min));
    }
    for (i, s) in shown.iter().enumerate() {
        let tag = match s.safety {
            Safety::Safe => st.green("SAFE  "),
            Safety::Review => st.yellow("REVIEW"),
            Safety::Info => st.dim("INFO  "),
        };
        println!();
        println!("  {:>2}. {}  {}  {}", i + 1, st.size(s.size, 8), tag, st.bold(&s.title));
        let indent = " ".repeat(24);
        println!("{indent}{} {}", st.cyan("→"), st.cyan(&s.how));
        if let Some(note) = &s.note {
            println!("{indent}{}", st.yellow(note));
        }
        let max_items = 5;
        for item in s.items.iter().take(max_items) {
            let when = item.mtime.map(|m| format!("  {}", age(m, ctx.now))).unwrap_or_default();
            println!(
                "{indent}{}  {}{}",
                st.size(item.size, 8),
                truncate_middle(&display_path(&item.path, home), 90),
                st.dim(&when)
            );
        }
        if s.items.len() > max_items {
            let rest: u64 = s.items[max_items..].iter().map(|i| i.size).sum();
            println!("{indent}{}", st.dim(&format!("{:>8}  … and {} more", human(rest), s.items.len() - max_items)));
        }
    }

    // ── Totals ────────────────────────────────────────────────────────────
    let sum = |safety| sugg.iter().filter(|s| s.safety == safety && s.size >= args.min).map(|s| s.size).sum::<u64>();
    println!();
    println!(
        "  {} {} safe to clean  ·  {} worth reviewing",
        st.bold("Potential:"),
        st.green(&st.bold(&human(sum(Safety::Safe)))),
        st.yellow(&st.bold(&human(sum(Safety::Review))))
    );
    let hidden = sugg.iter().filter(|s| s.size >= args.min).count().saturating_sub(shown.len());
    if hidden > 0 {
        println!("  {}", st.dim(&format!("{hidden} smaller suggestions not shown (use --top {})", args.top + hidden)));
    }

    // ── Notes ─────────────────────────────────────────────────────────────
    let mut notes: Vec<String> = Vec::new();
    if let (Some((total, avail)), false) = (info.disk, args.custom_roots) {
        let used = total.saturating_sub(avail);
        let unseen = used.saturating_sub(scanned);
        if unseen > 5_000_000_000 {
            notes.push(format!(
                "{} of used space wasn't visible to this scan: macOS itself (~15–25 GB), other users, \
                 APFS/Time Machine snapshots, and folders macOS wouldn't let us read.",
                human(unseen)
            ));
        }
    }
    if let Some(n) = info.snapshots.filter(|&n| n > 0) {
        notes.push(format!(
            "{n} Time Machine local snapshot{} on /. macOS frees them when space runs low; \
             to remove now: tmutil listlocalsnapshots /  →  sudo tmutil deletelocalsnapshots <date>",
            if n == 1 { "" } else { "s" }
        ));
    }
    if !scan.denied.is_empty() {
        let mut examples: Vec<String> = scan.denied.iter().map(|p| display_path(p, home)).collect();
        examples.sort_by_key(|s| s.len());
        examples.truncate(3);
        notes.push(format!(
            "{} folders couldn't be read (e.g. {}). For a complete picture give your terminal \
             Full Disk Access: System Settings › Privacy & Security › Full Disk Access.",
            thousands(scan.errors),
            examples.join(", ")
        ));
    }
    notes.push("diskhog never deletes anything. APFS clones may be counted twice, so totals can be slightly high.".into());
    println!();
    println!("{}", st.heading("NOTES"));
    for n in notes {
        println!("  {} {}", st.dim("•"), n);
    }
    println!();
}

#[allow(clippy::too_many_arguments)]
fn tree(
    dir: &Path,
    root_size: u64,
    min: u64,
    children: &HashMap<&Path, Vec<(&Path, u64)>>,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<(String, u64, f64)>,
) {
    if depth > max_depth {
        return;
    }
    let Some(kids) = children.get(dir) else { return };
    let kids: Vec<_> = kids.iter().filter(|(_, s)| *s >= min).take(8).collect();
    for (i, &&(path, size)) in kids.iter().enumerate() {
        let last = i == kids.len() - 1;
        let mut name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Collapse chains like Unity/Hub/Editor where one child holds nearly everything.
        let mut node = path;
        while !is_package(node) {
            match children.get(node).map(|v| v.as_slice()) {
                Some([(only, s), rest @ ..])
                    if *s >= size / 10 * 9 && rest.first().is_none_or(|r| r.1 < min) && *s >= min =>
                {
                    name.push('/');
                    name.push_str(&only.file_name().unwrap_or_default().to_string_lossy());
                    node = only;
                }
                _ => break,
            }
        }
        out.push((
            format!("{prefix}{}{name}", if last { "└─ " } else { "├─ " }),
            size,
            size as f64 / root_size as f64,
        ));
        if !is_package(node) {
            let next = format!("{prefix}{}", if last { "   " } else { "│  " });
            tree(node, root_size, min, children, &next, depth + 1, max_depth, out);
        }
    }
}

/// macOS bundles (apps, libraries) are shown as a whole, not drilled into.
fn is_package(p: &Path) -> bool {
    const EXTS: &[&str] =
        &["app", "bundle", "framework", "photoslibrary", "xcarchive", "fcpbundle", "logicx", "band", "plugin"];
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTS.contains(&e))
}
