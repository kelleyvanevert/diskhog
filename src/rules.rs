//! Known space hogs and how to clean them, plus the logic that turns a scan
//! into a ranked list of suggestions.
//!
//! Paths may start with `~` (home) or `$TMP` (the per-user /var/folders dir) and
//! may contain `*` wildcards inside a path component. Rules never touch the disk:
//! they are matched against the directory sizes the scanner already recorded.

use crate::scan::{ArtifactKind, ScanResult};
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
}

const fn r(title: &'static str, safety: Safety, how: &'static str, paths: &'static [&'static str]) -> Rule {
    Rule { title, safety, how, paths }
}

use Safety::*;

pub const RULES: &[Rule] = &[
    // ── General ───────────────────────────────────────────────────────────
    r("Trash", Safe, "Empty the Trash (Finder › Empty Trash)", &["~/.Trash", "~/.local/share/Trash"]),
    r("Application caches", Safe,
      "Quit apps, then: rm -rf ~/Library/Caches/*   (apps rebuild what they need)", &["~/Library/Caches"]),
    r("Sandboxed app caches", Safe,
      "Quit the apps, then delete these Caches folders", &["~/Library/Containers/*/Data/Library/Caches"]),
    r("Tool caches in ~/.cache", Safe, "Usually safe: rm -rf ~/.cache/*", &["~/.cache"]),
    r("Logs & crash reports", Safe, "rm -rf ~/Library/Logs/*", &["~/Library/Logs"]),
    r("Temporary files & system caches for your user (/var/folders)", Review,
      "Mostly cleared by a reboot; the C/ folder holds caches that are safe to delete with apps closed", &["$TMP"]),
    r("Core dumps", Safe, "sudo rm -f /cores/*", &["/cores"]),
    r("Swap & sleep image", Info,
      "Managed by macOS. Swap shrinks after a reboot; the sleep image equals your RAM size", &["/private/var/vm"]),
    r("Applications", Review,
      "Uninstall apps you don't use (move to Trash, or use the vendor's uninstaller)", &["/Applications/*", "~/Applications/*"]),

    // ── Containers & VMs ──────────────────────────────────────────────────
    r("Docker Desktop disk image", Review,
      "docker system prune -a --volumes   (unused images, stopped containers, volumes)  · see usage: docker system df",
      &["~/Library/Containers/com.docker.docker/Data/vms"]),
    r("OrbStack / Colima / Podman", Review,
      "docker system prune -a   (OrbStack/Colima)  ·  podman system prune -a",
      &["~/.orbstack", "~/Library/Group Containers/*.dev.orbstack", "~/.colima", "~/.local/share/containers"]),
    r("Virtual machines", Review,
      "Delete VMs you no longer use from their app (Parallels, VMware, UTM, VirtualBox); Vagrant: vagrant box prune",
      &["~/Parallels", "~/Virtual Machines.localized", "~/VirtualBox VMs", "~/Library/Containers/com.utmapp.UTM/Data/Documents", "~/.vagrant.d/boxes"]),

    // ── Apple developer tools ─────────────────────────────────────────────
    r("Xcode DerivedData (build cache)", Safe, "rm -rf ~/Library/Developer/Xcode/DerivedData/*",
      &["~/Library/Developer/Xcode/DerivedData"]),
    r("Xcode device support files", Safe,
      "rm -rf ~/Library/Developer/Xcode/*DeviceSupport/*   (re-created when a device connects)",
      &["~/Library/Developer/Xcode/* DeviceSupport"]),
    r("Xcode archives", Review,
      "Delete old archives in Xcode › Window › Organizer (keep builds you still need to symbolicate)",
      &["~/Library/Developer/Xcode/Archives"]),
    r("iOS Simulator devices", Review,
      "xcrun simctl delete unavailable   (and delete simulators you don't use in Xcode › Devices)",
      &["~/Library/Developer/CoreSimulator/Devices"]),
    r("Simulator & SwiftUI preview caches", Safe,
      "rm -rf ~/Library/Developer/CoreSimulator/Caches/* ~/Library/Developer/Xcode/UserData/Previews/*",
      &["~/Library/Developer/CoreSimulator/Caches", "~/Library/Developer/Xcode/UserData/Previews", "/Library/Developer/CoreSimulator/Caches"]),
    r("Simulator runtimes (iOS/watchOS/visionOS)", Review,
      "xcrun simctl runtime list   →   xcrun simctl runtime delete <id>   (or Xcode › Settings › Components)",
      &["/Library/Developer/CoreSimulator/Images", "/Library/Developer/CoreSimulator/Cryptex"]),
    r("CocoaPods caches", Safe, "pod cache clean --all", &["~/Library/Caches/CocoaPods", "~/.cocoapods/repos"]),

    // ── Game engines ──────────────────────────────────────────────────────
    r("Unity Editor installs", Review,
      "Remove Editor versions you no longer use in Unity Hub › Installs", &["/Applications/Unity/Hub/Editor/*"]),
    r("Unity package & Asset Store caches", Safe,
      "rm -rf ~/Library/Unity/cache   ·  old Asset Store downloads can be re-downloaded",
      &["~/Library/Unity/cache", "~/Library/Unity/Asset Store-5.x"]),
    r("Unreal Engine installs", Review,
      "Remove unused engine versions in Epic Games Launcher › Unreal Engine › Library", &["/Users/Shared/Epic Games/UE_*"]),
    r("Unreal shared DerivedDataCache", Safe, "Delete it; rebuilt on demand",
      &["~/Library/Application Support/Epic/UnrealEngine/Common/DerivedDataCache"]),

    // ── Package managers & toolchains ─────────────────────────────────────
    r("Homebrew download cache", Safe, "brew cleanup --prune=all", &["~/Library/Caches/Homebrew"]),
    r("Homebrew packages", Review,
      "brew autoremove && brew cleanup   (unused deps & old versions)  ·  `brew leaves` shows what you installed",
      &["/opt/homebrew/Cellar", "/opt/homebrew/Caskroom", "/usr/local/Cellar", "/usr/local/Caskroom"]),
    r("npm cache", Safe, "npm cache clean --force", &["~/.npm"]),
    r("Yarn cache", Safe, "yarn cache clean", &["~/Library/Caches/Yarn", "~/.yarn/berry/cache", "~/.cache/yarn"]),
    r("pnpm store", Safe, "pnpm store prune", &["~/Library/pnpm/store", "~/.local/share/pnpm/store", "~/.pnpm-store"]),
    r("Bun cache", Safe, "bun pm cache rm", &["~/.bun/install/cache"]),
    r("Node.js versions (nvm/fnm/volta)", Review, "nvm ls → nvm uninstall <old>   (fnm uninstall / volta uninstall)",
      &["~/.nvm/versions/node", "~/Library/Application Support/fnm/node-versions", "~/.fnm/node-versions", "~/.volta/tools/image/node"]),
    r("Cargo registry & git cache", Safe,
      "cargo install cargo-cache && cargo cache --autoclean   (or rm -rf ~/.cargo/registry/{cache,src})",
      &["~/.cargo/registry", "~/.cargo/git"]),
    r("Rust toolchains", Review, "rustup toolchain list → rustup toolchain uninstall <old>", &["~/.rustup/toolchains"]),
    r("Gradle caches", Safe, "rm -rf ~/.gradle/caches ~/.gradle/wrapper/dists   (re-downloaded on next build)",
      &["~/.gradle/caches", "~/.gradle/wrapper/dists", "~/.gradle/daemon"]),
    r("Maven repository", Safe, "rm -rf ~/.m2/repository   (re-downloaded on next build)", &["~/.m2/repository"]),
    r("Python package caches", Safe, "pip cache purge  ·  uv cache clean  ·  poetry cache clear --all .",
      &["~/Library/Caches/pip", "~/.cache/pip", "~/.cache/uv", "~/Library/Caches/pypoetry", "~/.cache/pypoetry"]),
    r("Conda package cache", Safe, "conda clean --all",
      &["~/miniconda3/pkgs", "~/anaconda3/pkgs", "~/miniforge3/pkgs", "~/mambaforge/pkgs", "~/opt/anaconda3/pkgs",
        "~/opt/miniconda3/pkgs", "/opt/homebrew/Caskroom/miniconda/base/pkgs", "/opt/homebrew/Caskroom/miniforge/base/pkgs"]),
    r("Python versions (pyenv)", Review, "pyenv versions → pyenv uninstall <old>", &["~/.pyenv/versions"]),
    r("Go caches", Safe, "go clean -modcache && go clean -cache",
      &["~/go/pkg/mod", "~/Library/Caches/go-build", "~/.cache/go-build"]),
    r("Dart/Flutter pub cache", Safe, "dart pub cache clean", &["~/.pub-cache"]),
    r("Browser test binaries (Playwright/Puppeteer/Cypress)", Safe,
      "npx playwright uninstall --all  ·  rm -rf ~/.cache/puppeteer  ·  npx cypress cache clear",
      &["~/Library/Caches/ms-playwright", "~/.cache/ms-playwright", "~/.cache/puppeteer", "~/Library/Caches/Cypress", "~/.cache/Cypress"]),
    r("Android emulators", Review, "Delete unused virtual devices in Android Studio › Device Manager", &["~/.android/avd"]),
    r("Android SDK system images", Review, "Remove unused images in Android Studio › SDK Manager",
      &["~/Library/Android/sdk/system-images", "~/Android/Sdk/system-images"]),

    // ── Editors & browsers ────────────────────────────────────────────────
    r("VS Code / Cursor caches & workspace storage", Safe,
      "Quit the editor, then delete these folders (rebuilt automatically)",
      &["~/Library/Application Support/*/CachedData", "~/Library/Application Support/*/CachedExtensionVSIXs",
        "~/Library/Application Support/*/User/workspaceStorage"]),
    r("JetBrains IDE data (incl. old versions)", Review,
      "Delete folders of IDE versions you no longer use (or JetBrains Toolbox › uninstall old versions)",
      &["~/Library/Application Support/JetBrains/*"]),
    r("Browser service-worker caches", Safe, "Quit the browser, then delete; or clear site data in its settings",
      &["~/Library/Application Support/Google/Chrome/*/Service Worker", "~/Library/Application Support/Microsoft Edge/*/Service Worker",
        "~/Library/Application Support/BraveSoftware/Brave-Browser/*/Service Worker", "~/Library/Application Support/Arc/User Data/*/Service Worker"]),
    r("Electron app caches (Slack, Teams, Discord, ...)", Safe, "Quit the app, then delete these folders",
      &["~/Library/Application Support/*/Cache", "~/Library/Application Support/*/Code Cache",
        "~/Library/Application Support/*/GPUCache", "~/Library/Application Support/*/Service Worker/CacheStorage"]),

    // ── AI models ─────────────────────────────────────────────────────────
    r("Ollama models", Review, "ollama list → ollama rm <model>", &["~/.ollama/models"]),
    r("Hugging Face cache", Review, "hf cache scan → hf cache delete   (older: huggingface-cli delete-cache)", &["~/.cache/huggingface"]),
    r("LM Studio models", Review, "Delete models in LM Studio › My Models", &["~/.lmstudio/models", "~/.cache/lm-studio/models"]),
    r("PyTorch hub cache", Safe, "rm -rf ~/.cache/torch", &["~/.cache/torch"]),

    // ── Personal media & backups ──────────────────────────────────────────
    r("iPhone/iPad backups", Review, "Finder › your device › Manage Backups… → delete old ones",
      &["~/Library/Application Support/MobileSync/Backup"]),
    r("iOS firmware downloads", Safe, "rm -rf ~/Library/iTunes/*Software Updates", &["~/Library/iTunes/* Software Updates"]),
    r("Messages attachments", Review,
      "System Settings › General › Storage › Messages, or Messages › Settings › Keep messages: 1 Year",
      &["~/Library/Messages/Attachments"]),
    r("Mail downloads", Safe, "Safe to delete (attachments you opened from Mail)",
      &["~/Library/Containers/com.apple.mail/Data/Library/Mail Downloads"]),
    r("Mail", Review, "Delete big attachments / old mailboxes in Mail", &["~/Library/Mail"]),
    r("Photos library", Review, "Photos › Settings › iCloud › Optimize Mac Storage (full-res stays in iCloud)",
      &["~/Pictures/*.photoslibrary"]),
    r("Music & TV downloads", Review, "Remove downloads in Music/TV (right-click › Remove Download)",
      &["~/Music/Music/Media.localized", "~/Music/Music/Media", "~/Music/iTunes/iTunes Media", "~/Movies/TV/Media.localized"]),
    r("GarageBand / Logic sound libraries", Review, "GarageBand/Logic › Sound Library › Remove…",
      &["/Library/Application Support/GarageBand", "/Library/Application Support/Logic", "/Library/Audio/Apple Loops"]),
    r("Adobe media caches", Safe, "Premiere/After Effects › Settings › Media Cache › Delete",
      &["~/Library/Application Support/Adobe/Common/Media Cache Files", "~/Library/Application Support/Adobe/Common/Media Cache"]),
    r("Spotify cache", Safe, "Spotify › Settings › Storage › Clear cache",
      &["~/Library/Application Support/Spotify/PersistentCache", "~/Library/Caches/com.spotify.client"]),
    r("Steam games", Review, "Uninstall games in Steam › Library", &["~/Library/Application Support/Steam/steamapps"]),
    r("iCloud Drive (downloaded files)", Review,
      "Enable Optimize Mac Storage (System Settings › Apple Account › iCloud › Drive), or right-click › Remove Download",
      &["~/Library/Mobile Documents"]),
    r("Cloud storage (OneDrive, Google Drive, Dropbox)", Review, "Right-click big folders › Free Up Space / Make online-only",
      &["~/Library/CloudStorage/*"]),
];

pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: Option<i64>,
}

pub struct Suggestion {
    pub title: String,
    pub safety: Safety,
    pub how: String,
    pub size: u64,
    pub items: Vec<Item>,
    pub note: Option<String>,
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
    Artifact(usize),
}

fn expand(pattern: &str, ctx: &Context) -> Option<PathBuf> {
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
            (Some(Component::Normal(a)), Some(Component::Normal(b))) => {
                match (a.to_str(), b.to_str()) {
                    (Some(a), Some(b)) if wild(a, b) => {}
                    _ => return false,
                }
            }
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// All recorded directories matching a (possibly wildcarded) pattern.
fn matches(pattern: &Path, scan: &ScanResult) -> Vec<(PathBuf, u64)> {
    let s = pattern.to_string_lossy();
    if !s.contains('*') {
        return scan.dirs.get(pattern).map(|&sz| vec![(pattern.to_path_buf(), sz)]).unwrap_or_default();
    }
    // Literal prefix before the first wildcard component narrows the search.
    let prefix: PathBuf = pattern
        .components()
        .take_while(|c| !c.as_os_str().to_string_lossy().contains('*'))
        .collect();
    scan.dirs
        .iter()
        .filter(|(p, _)| p.starts_with(&prefix) && glob_match(pattern, p))
        .map(|(p, &sz)| (p.clone(), sz))
        .collect()
}

pub fn analyze(scan: &ScanResult, ctx: &Context) -> Vec<Suggestion> {
    // 1. Every path claimed by a rule or an artifact (first claim wins).
    let mut claims: HashMap<PathBuf, (Owner, u64)> = HashMap::new();
    for (i, rule) in RULES.iter().enumerate() {
        for pat in rule.paths {
            let Some(pat) = expand(pat, ctx) else { continue };
            for (path, size) in matches(&pat, scan) {
                claims.entry(path).or_insert((Owner::Rule(i), size));
            }
        }
    }
    // Build artifacts inside a rule's territory belong to that rule: e.g. the Yarn
    // cache stores packages in node_modules folders, and apps bundle them too.
    let rule_claimed: Vec<bool> = scan
        .artifacts
        .iter()
        .map(|a| a.path.ancestors().any(|p| claims.contains_key(p)))
        .collect();
    for (j, a) in scan.artifacts.iter().enumerate() {
        if !rule_claimed[j] {
            claims.entry(a.path.clone()).or_insert((Owner::Artifact(j), a.size));
        }
    }

    // 2. Exclusive sizes: subtract each claim from its nearest claimed ancestor,
    //    so e.g. "Application caches" doesn't also count the Homebrew cache.
    let mut excl: HashMap<PathBuf, u64> = claims.iter().map(|(p, (_, s))| (p.clone(), *s)).collect();
    for (path, (_, size)) in &claims {
        if let Some(anc) = path.ancestors().skip(1).find(|a| claims.contains_key(*a)) {
            let e = excl.get_mut(anc).unwrap();
            *e = e.saturating_sub(*size);
        }
    }
    let claimed_ancestor = |p: &Path| p.ancestors().skip(1).any(|a| claims.contains_key(a));

    // 3. Group into suggestions.
    let mut by_rule: BTreeMap<usize, Vec<Item>> = BTreeMap::new();
    let mut by_kind: BTreeMap<ArtifactKind, Vec<Item>> = BTreeMap::new();
    for (path, (owner, _)) in &claims {
        let size = excl[path];
        if size == 0 {
            continue;
        }
        match *owner {
            Owner::Rule(i) => by_rule.entry(i).or_default().push(Item { path: path.clone(), size, mtime: None }),
            Owner::Artifact(j) => {
                let a = &scan.artifacts[j];
                by_kind.entry(a.kind).or_default().push(Item { path: path.clone(), size, mtime: Some(a.mtime) })
            }
        }
    }

    let mut out = Vec::new();
    for (i, items) in by_rule {
        let rule = &RULES[i];
        out.push(suggestion(rule.title.to_string(), rule.safety, rule.how.to_string(), items, None));
    }

    let stale_cutoff = ctx.now - ctx.stale_days * 86_400;
    for (kind, items) in by_kind {
        let n = items.len();
        let stale: u64 = items.iter().filter(|i| i.mtime.is_some_and(|m| m < stale_cutoff)).map(|i| i.size).sum();
        let note = (stale > 0).then(|| {
            format!("{} of it in folders untouched for {}+ days", crate::human(stale), ctx.stale_days)
        });
        let title = format!("{} ({} {})", kind.label(), n, if n == 1 { "folder" } else { "folders" });
        out.push(suggestion(title, Safe, kind.how().to_string(), items, note));
    }

    let installers: Vec<Item> = scan
        .installers
        .iter()
        .filter(|f| !claimed_ancestor(&f.path))
        .map(|f| Item { path: f.path.clone(), size: f.size, mtime: Some(f.mtime) })
        .collect();
    if !installers.is_empty() {
        out.push(suggestion(
            "Installer images (.dmg .pkg .iso .xip .ipsw)".into(),
            Safe,
            "Once the software is installed these can usually go".into(),
            installers,
            None,
        ));
    }

    let large: Vec<Item> = scan
        .large_files
        .iter()
        .filter(|f| !claimed_ancestor(&f.path))
        .map(|f| Item { path: f.path.clone(), size: f.size, mtime: Some(f.mtime) })
        .collect();
    if !large.is_empty() {
        out.push(suggestion(
            format!("Other large files (≥ {})", crate::human(crate::scan::LARGE_FILE_MIN)),
            Review,
            "Your own files: delete, archive to an external drive, or move to cloud storage".into(),
            large,
            None,
        ));
    }

    out.sort_by(|a, b| b.size.cmp(&a.size));
    out
}

fn suggestion(title: String, safety: Safety, how: String, mut items: Vec<Item>, note: Option<String>) -> Suggestion {
    items.sort_by(|a, b| b.size.cmp(&a.size));
    let size = items.iter().map(|i| i.size).sum();
    Suggestion { title, safety, how, size, items, note }
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
