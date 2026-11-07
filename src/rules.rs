//! Known space hogs and how to clean them, plus the logic that turns scan data
//! into categories of cleanup candidates.
//!
//! Paths may start with `~` (home) or `$TMP` (the per-user /var/folders dir) and
//! may contain `*` wildcards inside a path component.

use crate::scan::{ArtifactKind, Data};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Safety {
    /// Regenerable cache/build output; deleting it costs at most a re-download or rebuild.
    Safe,
    /// Your data or installs; look before you delete.
    Review,
    /// Managed by the system; shown for understanding only.
    Info,
}

pub struct Rule {
    pub title: &'static str,
    pub safety: Safety,
    pub how: &'static str,
    pub paths: &'static [&'static str],
    /// A command that cleans this category properly (offered with `x`).
    pub cmd: Option<&'static str>,
    /// Alternative cleanup commands as (description, command), offered with `x`.
    pub options: &'static [(&'static str, &'static str)],
    /// List the children of each matched folder as separate items.
    pub expand: bool,
    /// Items may be deleted from diskhog. False where deleting files directly would
    /// break things or delete data elsewhere (iCloud, Photos, Docker's disk image...).
    pub deletable: bool,
}

impl Rule {
    const fn cmd(mut self, c: &'static str) -> Self {
        self.cmd = Some(c);
        self
    }
    const fn options(mut self, o: &'static [(&'static str, &'static str)]) -> Self {
        self.options = o;
        self
    }
    const fn expand(mut self) -> Self {
        self.expand = true;
        self
    }
    const fn manual(mut self) -> Self {
        self.deletable = false;
        self
    }
}

const fn r(title: &'static str, safety: Safety, how: &'static str, paths: &'static [&'static str]) -> Rule {
    Rule { title, safety, how, paths, cmd: None, options: &[], expand: false, deletable: true }
}

use Safety::*;

pub const RULES: &[Rule] = &[
    // ── General ───────────────────────────────────────────────────────────
    r("Trash", Safe, "Empty the Trash to actually free this space", &["~/.Trash", "~/.local/share/Trash"])
        .cmd("osascript -e 'tell application \"Finder\" to empty trash'").manual(),
    r("Application caches", Safe, "Quit the app first; apps rebuild what they need", &["~/Library/Caches"]).expand(),
    r("Sandboxed app caches", Safe, "Quit the app first; rebuilt automatically",
      &["~/Library/Containers/*/Data/Library/Caches"]),
    r("Tool caches in ~/.cache", Safe, "Usually safe to delete; tools re-download what they need", &["~/.cache"]).expand(),
    r("Logs & crash reports", Safe, "Safe to delete", &["~/Library/Logs"]).expand(),
    r("Temporary files for your user (/var/folders)", Review,
      "Mostly cleared by a reboot", &["$TMP"]).manual(),
    r("Core dumps", Safe, "Crash memory dumps; safe to delete", &["/cores"]).cmd("sudo rm -f /cores/*").manual(),
    r("Swap & sleep image", Info, "Managed by macOS. Swap shrinks after a reboot", &["/private/var/vm"]).manual(),
    r("Applications", Review,
      "Walk through and remove apps you don't use (sorted by last opened with `s`)", &["/Applications/*", "~/Applications/*"]),

    // ── Containers & VMs ──────────────────────────────────────────────────
    r("Docker Desktop disk image", Review,
      "Pick images, containers, volumes and build cache below (o: unused for 3+ months), or press x for prune options",
      &["~/Library/Containers/com.docker.docker/Data/vms"]).options(&[
        ("Build cache not used for 30+ days", "docker builder prune -f --filter until=720h"),
        ("All build cache", "docker builder prune -af"),
        ("Stopped containers, and unused images created 30+ days ago",
         "docker container prune -f --filter until=720h && docker image prune -af --filter until=720h"),
        ("Everything unused except volumes (images, containers, networks, cache)", "docker system prune -af"),
        ("Volumes not used by any container (deletes their data, e.g. databases!)", "docker volume prune -af"),
      ]).manual(),
    r("OrbStack / Colima / Podman", Review, "Prune unused images and containers",
      &["~/.orbstack", "~/Library/Group Containers/*.dev.orbstack", "~/.colima", "~/.local/share/containers"])
        .cmd("docker system prune -a").manual(),
    r("Virtual machines", Review, "Delete VMs you no longer use",
      &["~/Parallels", "~/Virtual Machines.localized", "~/VirtualBox VMs", "~/Library/Containers/com.utmapp.UTM/Data/Documents",
        "~/.vagrant.d/boxes"]).expand(),

    // ── Apple developer tools ─────────────────────────────────────────────
    r("Xcode DerivedData (build cache)", Safe, "Per-project build caches; rebuilt on next build",
      &["~/Library/Developer/Xcode/DerivedData"]).expand(),
    r("Xcode device support files", Safe, "Re-created when that device/OS version connects again",
      &["~/Library/Developer/Xcode/* DeviceSupport"]).expand(),
    r("Xcode archives", Review, "Keep archives of builds you still need to symbolicate",
      &["~/Library/Developer/Xcode/Archives"]).expand(),
    r("iOS Simulator devices", Review, "Pick simulators below (sorted by last booted with s), or x to remove ones whose runtime is gone",
      &["~/Library/Developer/CoreSimulator/Devices"]).cmd("xcrun simctl delete unavailable").manual(),
    r("Simulator & SwiftUI preview caches", Safe, "Rebuilt automatically",
      &["~/Library/Developer/CoreSimulator/Caches", "~/Library/Developer/Xcode/UserData/Previews", "/Library/Developer/CoreSimulator/Caches"]),
    r("Simulator runtimes (iOS/watchOS/visionOS)", Review,
      "Pick runtimes you no longer test on (removed with simctl), or x for the not-used-since options",
      &["/Library/Developer/CoreSimulator/Images", "/Library/Developer/CoreSimulator/Cryptex"])
        .options(&[
          ("Runtimes not used for 90+ days", "xcrun simctl runtime delete --notUsedSinceDays 90"),
          ("Runtimes not used for 180+ days", "xcrun simctl runtime delete --notUsedSinceDays 180"),
        ]).manual(),
    r("CocoaPods caches", Safe, "Re-downloaded on next pod install", &["~/Library/Caches/CocoaPods", "~/.cocoapods/repos"])
        .cmd("pod cache clean --all"),

    // ── Game engines ──────────────────────────────────────────────────────
    r("Unity Editor installs", Review, "Remove Editor versions you no longer use (or via Unity Hub › Installs)",
      &["/Applications/Unity/Hub/Editor/*"]),
    r("Unity package & Asset Store caches", Safe, "Re-downloaded when needed",
      &["~/Library/Unity/cache", "~/Library/Unity/Asset Store-5.x"]).expand(),
    r("Unreal Engine installs", Review, "Remove unused versions (or via Epic Games Launcher)", &["/Users/Shared/Epic Games/UE_*"]),
    r("Unreal shared DerivedDataCache", Safe, "Rebuilt on demand",
      &["~/Library/Application Support/Epic/UnrealEngine/Common/DerivedDataCache"]),

    // ── Package managers & toolchains ─────────────────────────────────────
    r("Homebrew download cache", Safe, "Old downloads and bottles", &["~/Library/Caches/Homebrew"]).cmd("brew cleanup --prune=all"),
    r("Homebrew packages", Review, "Removes unused dependencies and old versions. `brew leaves` shows what you installed",
      &["/opt/homebrew/Cellar", "/opt/homebrew/Caskroom", "/usr/local/Cellar", "/usr/local/Caskroom"])
        .cmd("brew autoremove && brew cleanup").expand().manual(),
    r("npm cache", Safe, "Re-downloaded when needed", &["~/.npm"]).cmd("npm cache clean --force"),
    r("Yarn cache", Safe, "Re-downloaded when needed", &["~/Library/Caches/Yarn", "~/.yarn/berry/cache", "~/.cache/yarn"])
        .cmd("yarn cache clean"),
    r("pnpm store", Safe, "Removes packages no project references", &["~/Library/pnpm/store", "~/.local/share/pnpm/store", "~/.pnpm-store"])
        .cmd("pnpm store prune"),
    r("Bun cache", Safe, "Re-downloaded when needed", &["~/.bun/install/cache"]).cmd("bun pm cache rm"),
    r("Node.js versions (nvm/fnm/volta)", Review, "Remove versions you no longer use",
      &["~/.nvm/versions/node", "~/Library/Application Support/fnm/node-versions", "~/.fnm/node-versions", "~/.volta/tools/image/node"])
        .expand(),
    r("Cargo registry & git cache", Safe, "Downloaded crate sources; re-fetched when needed",
      &["~/.cargo/registry", "~/.cargo/git"]).expand(),
    r("Rust toolchains", Review, "Remove old nightlies (`rustup toolchain uninstall <name>` is the tidy way)",
      &["~/.rustup/toolchains"]).cmd("rustup toolchain list").expand(),
    r("Gradle caches", Safe, "Re-downloaded on next build", &["~/.gradle/caches", "~/.gradle/wrapper/dists", "~/.gradle/daemon"]).expand(),
    r("Maven repository", Safe, "Re-downloaded on next build", &["~/.m2/repository"]),
    r("Python package caches", Safe, "Re-downloaded when needed",
      &["~/Library/Caches/pip", "~/.cache/pip", "~/.cache/uv", "~/Library/Caches/pypoetry", "~/.cache/pypoetry"]),
    r("Conda package cache", Safe, "Cached package tarballs",
      &["~/miniconda3/pkgs", "~/anaconda3/pkgs", "~/miniforge3/pkgs", "~/mambaforge/pkgs", "~/opt/anaconda3/pkgs",
        "~/opt/miniconda3/pkgs", "/opt/homebrew/Caskroom/miniconda/base/pkgs", "/opt/homebrew/Caskroom/miniforge/base/pkgs"])
        .cmd("conda clean --all -y").manual(),
    r("Python versions (pyenv)", Review, "Remove versions you no longer use", &["~/.pyenv/versions"]).expand(),
    r("Go caches", Safe, "Re-downloaded / rebuilt when needed", &["~/go/pkg/mod", "~/Library/Caches/go-build", "~/.cache/go-build"])
        .cmd("go clean -modcache && go clean -cache").manual(),
    r("Dart/Flutter pub cache", Safe, "Re-downloaded on next pub get", &["~/.pub-cache"]).cmd("dart pub cache clean --force"),
    r("Browser test binaries (Playwright/Puppeteer/Cypress)", Safe, "Re-downloaded on next install",
      &["~/Library/Caches/ms-playwright", "~/.cache/ms-playwright", "~/.cache/puppeteer", "~/Library/Caches/Cypress", "~/.cache/Cypress"])
        .expand(),
    r("Android emulators", Review, "Delete emulators you don't use (or Android Studio › Device Manager)", &["~/.android/avd"]).expand(),
    r("Android SDK system images", Review, "Remove images you don't use (or Android Studio › SDK Manager)",
      &["~/Library/Android/sdk/system-images/*", "~/Android/Sdk/system-images/*"]).expand(),

    // ── Editors & browsers ────────────────────────────────────────────────
    r("VS Code / Cursor caches & workspace storage", Safe, "Quit the editor first; rebuilt automatically",
      &["~/Library/Application Support/*/CachedData", "~/Library/Application Support/*/CachedExtensionVSIXs",
        "~/Library/Application Support/*/User/workspaceStorage"]),
    r("JetBrains IDE data (incl. old versions)", Review, "Delete folders of IDE versions you no longer use",
      &["~/Library/Application Support/JetBrains/*"]),
    r("Browser service-worker caches", Safe, "Quit the browser first; sites re-cache what they need",
      &["~/Library/Application Support/Google/Chrome/*/Service Worker", "~/Library/Application Support/Microsoft Edge/*/Service Worker",
        "~/Library/Application Support/BraveSoftware/Brave-Browser/*/Service Worker", "~/Library/Application Support/Arc/User Data/*/Service Worker"]),
    r("Electron app caches (Slack, Teams, Discord, ...)", Safe, "Quit the app first; rebuilt automatically",
      &["~/Library/Application Support/*/Cache", "~/Library/Application Support/*/Code Cache",
        "~/Library/Application Support/*/GPUCache", "~/Library/Application Support/*/Service Worker/CacheStorage"]),

    // ── AI models ─────────────────────────────────────────────────────────
    r("Ollama models", Review, "Use `ollama rm <model>`; deleting blobs directly confuses Ollama", &["~/.ollama/models"])
        .cmd("ollama list").manual(),
    r("Hugging Face models & datasets", Review, "Delete models you no longer use",
      &["~/.cache/huggingface/hub/*", "~/.cache/huggingface/datasets/*"]),
    r("LM Studio models", Review, "Delete models you no longer use", &["~/.lmstudio/models/*/*", "~/.cache/lm-studio/models/*/*"]),
    r("PyTorch hub cache", Safe, "Re-downloaded when needed", &["~/.cache/torch"]),

    // ── Personal media & backups ──────────────────────────────────────────
    r("iPhone/iPad backups", Review, "Old device backups (or Finder › device › Manage Backups)",
      &["~/Library/Application Support/MobileSync/Backup"]).expand(),
    r("iOS firmware downloads", Safe, "Re-downloaded when needed", &["~/Library/iTunes/* Software Updates"]).expand(),
    r("Messages attachments", Review,
      "System Settings › General › Storage › Messages, or Messages › Settings › Keep messages: 1 Year",
      &["~/Library/Messages/Attachments"]).manual(),
    r("Mail downloads", Safe, "Attachments you opened from Mail", &["~/Library/Containers/com.apple.mail/Data/Library/Mail Downloads"]),
    r("Mail", Review, "Delete big attachments / old mailboxes inside Mail", &["~/Library/Mail"]).manual(),
    r("Photos library", Review, "Photos › Settings › iCloud › Optimize Mac Storage (full-res stays in iCloud)",
      &["~/Pictures/*.photoslibrary"]).manual(),
    r("Music & TV downloads", Review, "Remove downloads inside Music/TV (right-click › Remove Download)",
      &["~/Music/Music/Media.localized", "~/Music/Music/Media", "~/Music/iTunes/iTunes Media", "~/Movies/TV/Media.localized"]).manual(),
    r("GarageBand / Logic sound libraries", Review, "GarageBand/Logic › Sound Library › Remove…",
      &["/Library/Application Support/GarageBand", "/Library/Application Support/Logic", "/Library/Audio/Apple Loops"]).manual(),
    r("Adobe media caches", Safe, "Rebuilt automatically (or Premiere › Settings › Media Cache › Delete)",
      &["~/Library/Application Support/Adobe/Common/Media Cache Files", "~/Library/Application Support/Adobe/Common/Media Cache"]),
    r("Spotify cache", Safe, "Quit Spotify first; offline songs need re-downloading",
      &["~/Library/Application Support/Spotify/PersistentCache", "~/Library/Caches/com.spotify.client"]),
    r("Steam games", Review, "Uninstall via Steam › Library so Steam knows", &["~/Library/Application Support/Steam/steamapps/common/*"])
        .manual(),
    r("iCloud Drive (downloaded files)", Review,
      "Deleting here deletes from iCloud! Use right-click › Remove Download, or turn on Optimize Mac Storage",
      &["~/Library/Mobile Documents"]).manual(),
    r("Cloud storage (OneDrive, Google Drive, Dropbox)", Review,
      "Deleting here deletes in the cloud! Use right-click › Free Up Space / Make online-only",
      &["~/Library/CloudStorage/*"]).manual(),
];

#[derive(Clone)]
pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: Option<i64>,
    /// `mtime` is when it was last opened (apps), not last modified.
    pub opened: bool,
    pub selectable: bool,
    /// Extra info shown after the name, e.g. "running" or "everything else in here".
    pub note: Option<String>,
    /// Shown instead of the path (for tool-provided items like Docker images).
    pub label: Option<String>,
    /// Removed by running this command instead of deleting files.
    pub cmd: Option<Vec<String>>,
    /// Order within a removal batch (lower first).
    pub order: u8,
}

#[derive(Clone)]
pub struct Category {
    pub id: String,
    pub title: String,
    pub safety: Safety,
    pub how: String,
    /// Cleanup commands as (description, command).
    pub cmds: Vec<(String, String)>,
    /// Tool that provides this category's items, if any.
    pub provider: Option<&'static str>,
    /// Header of the age column: "Modified", "Launched", "Last used".
    pub age_label: &'static str,
    pub size: u64,
    pub items: Vec<Item>,
    pub note: Option<String>,
    /// Folders to re-measure after running `cmd`.
    pub refresh: Vec<PathBuf>,
}

pub struct Context {
    pub home: PathBuf,
    pub tmp: Option<PathBuf>,
    pub now: i64,
    pub stale_days: i64,
}

#[derive(Clone, Copy)]
enum Owner {
    Rule(usize),
    /// Child of an expanded rule's folder.
    RuleChild(usize),
    Artifact(usize),
}

pub fn expand(pattern: &str, ctx: &Context) -> Option<PathBuf> {
    if let Some(rest) = pattern.strip_prefix("~/") {
        Some(ctx.home.join(rest))
    } else if pattern == "$TMP" {
        ctx.tmp.clone()
    } else {
        Some(PathBuf::from(pattern))
    }
}

/// `*` wildcard match within a single path component.
fn wild(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if s.len() < first.len() + last.len() || !s.starts_with(first) || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn glob_match(pattern: &Path, path: &Path) -> bool {
    let (mut p, mut q) = (pattern.components(), path.components());
    loop {
        match (p.next(), q.next()) {
            (None, None) => return true,
            (Some(Component::Normal(a)), Some(Component::Normal(b))) => match (a.to_str(), b.to_str()) {
                (Some(a), Some(b)) if wild(a, b) => {}
                _ => return false,
            },
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

fn literal_prefix(pattern: &Path) -> PathBuf {
    pattern.components().take_while(|c| !c.as_os_str().to_string_lossy().contains('*')).collect()
}

/// Expand a wildcard pattern against the filesystem (for the hotspot phase).
fn glob_fs(pattern: &Path) -> Vec<PathBuf> {
    let mut current = vec![PathBuf::new()];
    for comp in pattern.components() {
        let s = comp.as_os_str().to_string_lossy();
        if !s.contains('*') {
            current.iter_mut().for_each(|p| p.push(comp));
            continue;
        }
        let mut next = Vec::new();
        for dir in &current {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name();
                if e.file_type().is_ok_and(|t| t.is_dir()) && name.to_str().is_some_and(|n| wild(&s, n)) {
                    next.push(dir.join(name));
                }
            }
        }
        current = next;
    }
    current
}

/// Concrete folders to scan first: everything the rules point at, under the roots.
pub fn hotspot_targets(ctx: &Context, roots: &[PathBuf]) -> Vec<PathBuf> {
    RULES
        .iter()
        .flat_map(|r| r.paths.iter())
        .filter_map(|p| expand(p, ctx))
        .flat_map(|p| if p.to_string_lossy().contains('*') { glob_fs(&p) } else { vec![p] })
        .filter(|p| roots.iter().any(|r| p.starts_with(r)))
        .collect()
}

/// All recorded directories matching a (possibly wildcarded) pattern.
fn matches(pattern: &Path, d: &Data) -> Vec<(PathBuf, u64)> {
    if !pattern.to_string_lossy().contains('*') {
        return d.dirs.get(pattern).map(|i| vec![(pattern.to_path_buf(), i.size)]).unwrap_or_default();
    }
    let prefix = literal_prefix(pattern);
    d.dirs
        .iter()
        .filter(|(p, _)| p.starts_with(&prefix) && glob_match(pattern, p))
        .map(|(p, i)| (p.clone(), i.size))
        .collect()
}

pub fn analyze(d: &Data, ctx: &Context) -> Vec<Category> {
    let mut children: HashMap<&Path, Vec<&Path>> = HashMap::new();
    for p in d.dirs.keys() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(p);
        }
    }

    // 1. Every path claimed by a rule, an expanded rule's child, or an artifact (first claim wins).
    let mut claims: HashMap<PathBuf, (Owner, u64)> = HashMap::new();
    let mut refresh: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();
    for (i, rule) in RULES.iter().enumerate() {
        for pat in rule.paths {
            let Some(pat) = expand(pat, ctx) else { continue };
            for (path, size) in matches(&pat, d) {
                refresh.entry(i).or_default().push(path.clone());
                claims.entry(path).or_insert((Owner::Rule(i), size));
            }
        }
    }
    let expanded: Vec<(usize, PathBuf)> = claims
        .iter()
        .filter_map(|(p, (o, _))| match o {
            Owner::Rule(i) if RULES[*i].expand => Some((*i, p.clone())),
            _ => None,
        })
        .collect();
    for (i, p) in expanded {
        for child in children.get(p.as_path()).into_iter().flatten() {
            let size = d.dirs[*child].size;
            claims.entry(child.to_path_buf()).or_insert((Owner::RuleChild(i), size));
        }
    }
    // Build artifacts inside a rule's territory belong to that rule: e.g. the Yarn
    // cache stores packages in node_modules folders, and apps bundle them too.
    // node_modules etc. inside tool-managed places (VS Code extensions, nvm's global
    // packages, app support folders) belong to that tool, not to one of your projects.
    let library = ctx.home.join("Library");
    let tool_managed = |p: &Path| {
        p.starts_with(&library)
            || ["/opt/homebrew", "/usr/local", "/Library", "/Applications"].iter().any(|r| p.starts_with(r))
            || p.strip_prefix(&ctx.home)
                .ok()
                .and_then(|rest| rest.components().next())
                .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    };
    for (j, a) in d.artifacts.iter().enumerate() {
        if !tool_managed(&a.path) && !a.path.ancestors().any(|p| claims.contains_key(p)) {
            claims.insert(a.path.clone(), (Owner::Artifact(j), a.size));
        }
    }

    // 2. Exclusive sizes: subtract each claim from its nearest claimed ancestor,
    //    so e.g. "Application caches" doesn't also count the Homebrew cache.
    let mut excl: HashMap<&Path, u64> = claims.iter().map(|(p, (_, s))| (p.as_path(), *s)).collect();
    for (path, (_, size)) in &claims {
        if let Some(anc) = path.ancestors().skip(1).find(|a| claims.contains_key(*a)) {
            let e = excl.get_mut(anc).unwrap();
            *e = e.saturating_sub(*size);
        }
    }
    let claimed_ancestor = |p: &Path| p.ancestors().skip(1).any(|a| claims.contains_key(a));
    let mtime_of = |p: &Path| d.dirs.get(p).map(|i| i.mtime).filter(|&m| m > 0);

    // 3. Group into categories.
    let mut by_rule: BTreeMap<usize, Vec<Item>> = BTreeMap::new();
    let mut by_kind: BTreeMap<ArtifactKind, Vec<Item>> = BTreeMap::new();
    for (path, (owner, _)) in &claims {
        let size = excl[path.as_path()];
        let remainder = matches!(owner, Owner::Rule(i) if RULES[*i].expand);
        if size == 0 || (remainder && size < 1_000_000) {
            continue;
        }
        match *owner {
            Owner::Rule(i) | Owner::RuleChild(i) => {
                let rule = &RULES[i];
                let (mtime, opened) = match d.last_used.get(path) {
                    Some(&t) => (Some(t), true),
                    None => (mtime_of(path), false),
                };
                by_rule.entry(i).or_default().push(Item {
                    path: path.clone(),
                    size,
                    mtime: if remainder { None } else { mtime },
                    opened,
                    selectable: rule.deletable && !remainder,
                    note: remainder.then(|| "everything else in here (small items)".into()),
                    label: None,
                    cmd: None,
                    order: 0,
                });
            }
            Owner::Artifact(j) => {
                let a = &d.artifacts[j];
                by_kind.entry(a.kind).or_default().push(Item {
                    path: path.clone(),
                    size,
                    mtime: Some(a.mtime),
                    opened: false,
                    selectable: true,
                    note: None,
                    label: None,
                    cmd: None,
                    order: 0,
                });
            }
        }
    }

    let mut out = Vec::new();
    for (i, items) in by_rule {
        let rule = &RULES[i];
        out.push(category(
            format!("rule:{i}"),
            rule.title.to_string(),
            rule.safety,
            rule.how.to_string(),
            rule_cmds(rule),
            items,
            None,
            refresh.remove(&i).unwrap_or_default(),
        ));
    }

    let stale_cutoff = ctx.now - ctx.stale_days * 86_400;
    for (kind, items) in by_kind {
        let n = items.len();
        let stale: u64 = items.iter().filter(|i| i.mtime.is_some_and(|m| m < stale_cutoff)).map(|i| i.size).sum();
        let note = (stale > 0).then(|| format!("{} untouched for {}+ days", crate::human(stale), ctx.stale_days));
        let title = format!("{} ({} {})", kind.label(), n, if n == 1 { "folder" } else { "folders" });
        out.push(category(format!("art:{kind:?}"), title, Safe, kind.how().to_string(), vec![], items, note, vec![]));
    }

    // Files inside app bundles or system-managed folders aren't yours to delete.
    let system_owned = |p: &Path| {
        p.starts_with("/Library/Updates")
            || p.starts_with("/Library/Developer")
            || p.starts_with("/System")
            || p.ancestors().skip(1).any(|a| a.extension().is_some_and(|e| e == "app"))
    };
    let files = |list: &[crate::scan::FileHit]| -> Vec<Item> {
        list.iter()
            .filter(|f| !claimed_ancestor(&f.path) && !system_owned(&f.path))
            .map(|f| Item {
                path: f.path.clone(),
                size: f.size,
                mtime: Some(f.mtime),
                opened: false,
                selectable: true,
                note: None,
                label: None,
                cmd: None,
                order: 0,
            })
            .collect()
    };
    let installers = files(&d.installers);
    if !installers.is_empty() {
        out.push(category(
            "installers".into(),
            "Installer images (.dmg .pkg .iso .xip .ipsw)".into(),
            Safe,
            "Once the software is installed these can usually go".into(),
            vec![],
            installers,
            None,
            vec![],
        ));
    }
    let large = files(&d.large);
    if !large.is_empty() {
        out.push(category(
            "large".into(),
            format!("Other large files (≥ {})", crate::human(crate::scan::LARGE_FILE_MIN)),
            Review,
            "Your own files: delete, archive to an external drive, or move to cloud storage".into(),
            vec![],
            large,
            None,
            vec![],
        ));
    }

    merge_providers(&mut out, d);
    out.retain(|c| c.size >= 1_000_000);
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.id.cmp(&b.id)));
    out
}

#[allow(clippy::too_many_arguments)]
fn category(
    id: String,
    title: String,
    safety: Safety,
    how: String,
    cmds: Vec<(String, String)>,
    mut items: Vec<Item>,
    note: Option<String>,
    refresh: Vec<PathBuf>,
) -> Category {
    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    let size = items.iter().map(|i| i.size).sum();
    let age_label = if items.iter().any(|i| i.opened) { "Launched" } else { "Modified" };
    Category { id, title, safety, how, cmds, provider: None, age_label, size, items, note, refresh }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard() {
        assert!(wild("*", "anything"));
        assert!(wild("UE_*", "UE_5.4"));
        assert!(wild("* DeviceSupport", "iOS DeviceSupport"));
        assert!(wild("*.photoslibrary", "Photos Library.photoslibrary"));
        assert!(!wild("*.photoslibrary", "Photos"));
        assert!(wild("a*b*c", "aXXbYYc"));
        assert!(!wild("a*b*c", "aXXcYYb"));
    }

    #[test]
    fn glob() {
        let p = Path::new("/Users/x/Library/Containers/*/Data/Library/Caches");
        assert!(glob_match(p, Path::new("/Users/x/Library/Containers/com.foo/Data/Library/Caches")));
        assert!(!glob_match(p, Path::new("/Users/x/Library/Containers/com.foo/Data/Library")));
    }
}

fn rule_cmds(rule: &Rule) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = rule.cmd.map(|c| ("Clean up".to_string(), c.to_string())).into_iter().collect();
    v.extend(rule.options.iter().map(|(d, c)| (d.to_string(), c.to_string())));
    v
}

/// Replace the file-level view of provider categories (Docker, simulators) with the
/// items the tool reports, or add a note while it's loading / unavailable.
fn merge_providers(out: &mut Vec<Category>, d: &Data) {
    use crate::providers::PState;
    for &title in crate::providers::ALL {
        let Some(i) = RULES.iter().position(|r| r.title == title) else { continue };
        let rule = &RULES[i];
        let id = format!("rule:{i}");
        let state = d.providers.get(title);
        let pos = match out.iter().position(|c| c.id == id) {
            Some(p) => p,
            None => {
                // Not found on disk (e.g. runtimes live in system-managed volumes):
                // only show it when the tool has something to say.
                if !matches!(state, Some(PState::Ready(items)) if !items.is_empty()) {
                    continue;
                }
                out.push(category(id, rule.title.into(), rule.safety, rule.how.into(), rule_cmds(rule), vec![], None, vec![]));
                out.len() - 1
            }
        };
        let c = &mut out[pos];
        c.provider = Some(title);
        match state {
            Some(PState::Ready(items)) => {
                let fs_size = c.size;
                let mut new: Vec<Item> = items
                    .iter()
                    .map(|p| Item {
                        path: p.path.clone(),
                        size: p.size,
                        mtime: p.mtime,
                        opened: false,
                        selectable: p.selectable,
                        note: p.note.clone(),
                        label: Some(p.label.clone()),
                        cmd: (!p.cmd.is_empty()).then(|| p.cmd.clone()),
                        order: p.order,
                    })
                    .collect();
                new.sort_by(|a, b| b.size.cmp(&a.size));
                let listed: u64 = new.iter().map(|i| i.size).sum();
                if fs_size > listed + 100_000_000 {
                    new.push(Item {
                        path: c.items.first().map(|i| i.path.clone()).unwrap_or_default(),
                        size: fs_size - listed,
                        mtime: None,
                        opened: false,
                        selectable: false,
                        note: Some(match title {
                            crate::providers::DOCKER => "layers shared between images, and space Docker freed but hasn't returned to macOS yet".into(),
                            _ => "other data in this folder".into(),
                        }),
                        label: Some("not attributed to an item".into()),
                        cmd: None,
                        order: 9,
                    });
                }
                c.size = fs_size.max(listed);
                c.items = new;
                c.age_label = "Last used";
            }
            Some(PState::Loading) => {
                c.note = Some(format!("Asking {} for details…", if title == crate::providers::DOCKER { "Docker" } else { "simctl" }))
            }
            Some(PState::Unavailable(msg)) => c.note = Some(msg.clone()),
            None => {}
        }
    }
}
