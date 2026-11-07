//! Fast parallel disk walker.
//!
//! Walks with rayon (one task per directory), measures the space
//! files actually occupy on disk (`st_blocks`, so sparse files such as
//! Docker.raw are counted correctly), counts hard-linked files once, and on
//! the way records:
//!   * every directory of at least `DIR_RECORD_MIN` bytes,
//!   * very large files and installer images,
//!   * regenerable build artifacts (node_modules, Cargo target/, Unity Library/, ...).

use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};

pub const DIR_RECORD_MIN: u64 = 10_000_000;
pub const LARGE_FILE_MIN: u64 = 500_000_000;
pub const INSTALLER_MIN: u64 = 20_000_000;
const INSTALLER_EXTS: &[&str] = &["dmg", "pkg", "mpkg", "iso", "xip", "ipsw"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArtifactKind {
    NodeModules,
    CargoTarget,
    MavenTarget,
    UnityLibrary,
    UnrealCache,
    GodotCache,
    PythonVenv,
    GradleBuild,
    CMakeBuild,
    Pods,
    JsBuildCache,
    SwiftBuild,
    ElixirBuild,
    ZigCache,
    HaskellBuild,
    Terraform,
    FlutterBuild,
}

impl ArtifactKind {
    pub fn label(self) -> &'static str {
        use ArtifactKind::*;
        match self {
            NodeModules => "node_modules folders",
            CargoTarget => "Rust target/ build folders",
            MavenTarget => "Maven target/ build folders",
            UnityLibrary => "Unity project Library/ folders",
            UnrealCache => "Unreal Intermediate/ & DerivedDataCache/",
            GodotCache => "Godot .godot/ import caches",
            PythonVenv => "Python virtualenvs",
            GradleBuild => "Gradle build/ & .gradle/ folders",
            CMakeBuild => "CMake build folders",
            Pods => "CocoaPods Pods/ folders",
            JsBuildCache => "JS framework caches (.next, .turbo, .nuxt, ...)",
            SwiftBuild => "SwiftPM .build/ folders",
            ElixirBuild => "Elixir _build/ & deps/",
            ZigCache => "Zig caches",
            HaskellBuild => "Haskell .stack-work / dist-newstyle",
            Terraform => "Terraform .terraform/ provider caches",
            FlutterBuild => "Flutter build/ folders",
        }
    }

    pub fn how(self) -> &'static str {
        use ArtifactKind::*;
        match self {
            NodeModules => "Delete in projects you're not working on; `npm/yarn/pnpm install` restores them.  Interactive: npx npkill",
            CargoTarget => "cargo clean   (in each project)  ·  bulk: cargo install cargo-sweep && cargo sweep -r -t 30 ~",
            MavenTarget => "mvn clean   (in each project)",
            UnityLibrary => "Close Unity, delete the project's Library/ folder; Unity re-imports it on next open (takes a while)",
            UnrealCache => "Close the editor and delete Intermediate/ and DerivedDataCache/; regenerated on next open",
            GodotCache => "Delete .godot/; Godot re-imports assets on next open",
            PythonVenv => "Delete virtualenvs of inactive projects; recreate with `python -m venv` / `uv sync`",
            GradleBuild => "./gradlew clean   or delete build/ and .gradle/ in inactive projects",
            CMakeBuild => "Delete build folders of inactive projects; regenerate with cmake",
            Pods => "Delete Pods/ in inactive projects; `pod install` restores them",
            JsBuildCache => "Safe to delete; rebuilt on the next dev/build run",
            SwiftBuild => "swift package clean   (or delete .build/)",
            ElixirBuild => "mix clean / delete _build and deps; `mix deps.get` restores them",
            ZigCache => "Delete .zig-cache / zig-out; rebuilt on next build",
            HaskellBuild => "stack clean / cabal clean",
            Terraform => "Delete .terraform/; `terraform init` restores it",
            FlutterBuild => "flutter clean   (in each project)",
        }
    }
}

pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub size: u64,
    /// Last activity: newest mtime inside the folder or of the project's manifest.
    pub mtime: i64,
}

pub struct FileHit {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct DirInfo {
    pub size: u64,
    /// Newest modification time of anything inside (recursive).
    pub mtime: i64,
}

/// Everything discovered so far. The UI reads it while the scan is running.
#[derive(Default)]
pub struct Data {
    /// Every directory >= DIR_RECORD_MIN, every hotspot target and every finished root.
    pub dirs: HashMap<PathBuf, DirInfo>,
    pub artifacts: Vec<Artifact>,
    pub large: Vec<FileHit>,
    pub installers: Vec<FileHit>,
    /// Finished roots with their total size.
    pub roots: Vec<(PathBuf, u64)>,
    pub denied: Vec<PathBuf>,
    /// App bundle -> last opened (from Spotlight).
    pub last_used: HashMap<PathBuf, i64>,
}

pub const PHASE_HOTSPOTS: u8 = 1;
pub const PHASE_FULL: u8 = 2;
pub const PHASE_DONE: u8 = 3;

#[derive(Default)]
pub struct Shared {
    pub data: Mutex<Data>,
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub phase: AtomicU8,
    /// Bumped whenever `data` changes, so the UI knows when to re-analyze.
    pub version: AtomicU64,
    inodes: Mutex<HashSet<(u64, u64)>>,
}

impl Shared {
    pub fn bump(&self) {
        self.version.fetch_add(1, Relaxed);
    }

    fn record_dir(&self, path: PathBuf, info: DirInfo) {
        self.data.lock().unwrap().dirs.insert(path, info);
        self.bump();
    }

    fn error(&self, path: &Path, e: &io::Error) {
        if e.kind() == io::ErrorKind::NotFound {
            return; // vanished while scanning
        }
        self.errors.fetch_add(1, Relaxed);
        if e.kind() == io::ErrorKind::PermissionDenied {
            let mut d = self.data.lock().unwrap();
            if d.denied.len() < 200 {
                d.denied.push(path.to_path_buf());
            }
        }
    }

    /// Forget `path` (deleted or moved away) and shrink its ancestors by `size`.
    pub fn apply_removal(&self, path: &Path, size: u64) {
        let mut d = self.data.lock().unwrap();
        d.dirs.retain(|p, _| !p.starts_with(path));
        d.artifacts.retain(|a| !a.path.starts_with(path));
        d.large.retain(|f| !f.path.starts_with(path));
        d.installers.retain(|f| !f.path.starts_with(path));
        for anc in path.ancestors().skip(1) {
            if let Some(i) = d.dirs.get_mut(anc) {
                i.size = i.size.saturating_sub(size);
            }
        }
        for (r, s) in d.roots.iter_mut() {
            if path.starts_with(&*r) && path != r.as_path() {
                *s = s.saturating_sub(size);
            }
        }
        drop(d);
        let _ = self.bytes.fetch_update(Relaxed, Relaxed, |b| Some(b.saturating_sub(size)));
        self.bump();
    }

    /// Re-measure `path` after something outside changed it (e.g. `npm cache clean`).
    pub fn refresh(&self, path: &Path) {
        let old = {
            let mut d = self.data.lock().unwrap();
            let old = d.dirs.get(path).map_or(0, |i| i.size);
            d.dirs.retain(|p, _| !p.starts_with(path));
            old
        };
        let new = match fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => {
                let empty_skip = HashSet::new();
                let empty_pre = HashMap::new();
                let walk = Walk { sh: self, skip: &empty_skip, pre: &empty_pre, defer: None };
                let info = walk.scan_dir(path, Some(md.dev()), false);
                self.record_dir(path.to_path_buf(), info);
                info.size
            }
            _ => 0,
        };
        // The walk added `new` to the byte counter; account for what disappeared.
        let _ = self.bytes.fetch_update(Relaxed, Relaxed, |b| Some(b.saturating_sub(old)));
        let mut d = self.data.lock().unwrap();
        for anc in path.ancestors().skip(1) {
            if let Some(i) = d.dirs.get_mut(anc) {
                i.size = (i.size + new).saturating_sub(old);
            }
        }
        for (r, s) in d.roots.iter_mut() {
            if path.starts_with(&*r) && path != r.as_path() {
                *s = (*s + new).saturating_sub(old);
            }
        }
        drop(d);
        self.bump();
    }
}

/// The scan runs two walks at the same time:
///  1. hotspots: the folders cleanup rules know about (caches, Docker, simulators,
///     ...). They hold most reclaimable bytes, so their results show up within seconds.
///  2. everything else under the roots (finding build artifacts, large files, ...).
///     It skips the hotspots; their sizes are added to the parent folders at the end.
pub fn run(sh: &Shared, roots: &[PathBuf], targets: Vec<PathBuf>, skip: &HashSet<PathBuf>) {
    sh.phase.store(PHASE_HOTSPOTS, Relaxed);
    let mut targets: Vec<PathBuf> = targets
        .into_iter()
        .filter(|t| fs::symlink_metadata(t).is_ok_and(|m| m.is_dir()))
        .collect();
    targets.sort();
    targets.dedup();
    let target_set: HashSet<PathBuf> = targets.iter().cloned().collect();

    let (hot, roots_done) = rayon::join(
        || {
            let r = scan_hotspots(sh, &targets, skip);
            sh.phase.store(PHASE_FULL, Relaxed);
            r
        },
        || {
            let walk = Walk { sh, skip, pre: &HashMap::new(), defer: Some(&target_set) };
            roots
                .par_iter()
                .filter(|root| !target_set.contains(*root))
                .filter_map(|root| {
                    let md = fs::metadata(root).ok()?;
                    // Stay on the root's filesystem, except when scanning "/" itself: on
                    // macOS the data volume is firmlinked into "/" with a different st_dev.
                    let dev = (root != Path::new("/")).then(|| md.dev());
                    Some((root.clone(), walk.walk_dir(root, dev, true).0))
                })
                .collect::<Vec<_>>()
        },
    );

    // Merge: add each outermost hotspot's size to its ancestors and roots.
    let mut d = sh.data.lock().unwrap();
    for (root, info) in roots_done {
        d.dirs.insert(root, info);
    }
    for (t, info) in &hot {
        if t.ancestors().skip(1).any(|a| hot.contains_key(a)) {
            continue; // already included in an outer hotspot
        }
        for anc in t.ancestors().skip(1) {
            if let Some(i) = d.dirs.get_mut(anc) {
                i.size += info.size;
                i.mtime = i.mtime.max(info.mtime);
            }
            if roots.iter().any(|r| r == anc) {
                break;
            }
        }
    }
    for root in roots {
        if let Some(info) = d.dirs.get(root).copied().or_else(|| hot.get(root).copied()) {
            d.dirs.insert(root.clone(), info);
            d.roots.push((root.clone(), info.size));
        }
    }
    drop(d);
    sh.phase.store(PHASE_DONE, Relaxed);
    sh.bump();
}

/// Scan the hotspot folders. Ones nested inside others (~/Library/Caches/Yarn inside
/// ~/Library/Caches) go in an earlier wave, so the outer one reuses their result.
fn scan_hotspots(sh: &Shared, targets: &[PathBuf], skip: &HashSet<PathBuf>) -> HashMap<PathBuf, DirInfo> {
    let mut by_depth: Vec<&PathBuf> = targets.iter().collect();
    by_depth.sort_by_key(|t| std::cmp::Reverse(t.components().count()));
    let mut wave: HashMap<&PathBuf, usize> = HashMap::new();
    for t in &by_depth {
        let w = targets
            .iter()
            .filter(|o| o.starts_with(t) && o != t)
            .filter_map(|o| wave.get(o))
            .max()
            .map_or(0, |w| w + 1);
        wave.insert(t, w);
    }
    let max_wave = wave.values().copied().max().unwrap_or(0);

    let mut pre: HashMap<PathBuf, DirInfo> = HashMap::new();
    for w in 0..=max_wave {
        let batch: Vec<&PathBuf> = targets.iter().filter(|t| wave[t] == w).collect();
        let walk = Walk { sh, skip, pre: &pre, defer: None };
        let done: Vec<(PathBuf, DirInfo)> = batch
            .par_iter()
            .filter_map(|t| {
                let md = fs::symlink_metadata(t).ok()?;
                let info = walk.scan_dir(t, Some(md.dev()), false);
                sh.record_dir((*t).clone(), info);
                Some(((*t).clone(), info))
            })
            .collect();
        pre.extend(done);
    }
    pre
}

struct Walk<'a> {
    sh: &'a Shared,
    skip: &'a HashSet<PathBuf>,
    /// Already measured subtrees, used instead of walking them again.
    pre: &'a HashMap<PathBuf, DirInfo>,
    /// Subtrees measured by the other (hotspot) walk; skipped here, merged later.
    defer: Option<&'a HashSet<PathBuf>>,
}

impl Walk<'_> {
    /// Size and newest mtime of everything below `dir`. With `detect`, also looks
    /// for build artifacts, large files and installers.
    fn scan_dir(&self, dir: &Path, dev: Option<u64>, detect: bool) -> DirInfo {
        self.walk_dir(dir, dev, detect).0
    }

    /// Like `scan_dir`, plus whether a deferred hotspot lies somewhere below `dir`.
    fn walk_dir(&self, dir: &Path, dev: Option<u64>, detect: bool) -> (DirInfo, bool) {
        let sh = self.sh;
        let mut total = 0u64;
        let mut files = 0u64;
        let mut newest = 0i64;
        let mut manifest_mtime = 0i64;
        // Names of subdirectories and project marker files, for artifact detection.
        let mut markers: Vec<OsString> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();

        let listed = list::list_dir(dir, |e| {
            newest = newest.max(e.mtime);
            match e.kind {
                list::Kind::Dir => {
                    if dev.is_some_and(|d| d != e.dev) {
                        return; // another filesystem (mounted disk image, network share, ...)
                    }
                    let name = OsStr::from_bytes(e.name);
                    subdirs.push(dir.join(name));
                    markers.push(name.to_owned());
                }
                list::Kind::File => {
                    if e.nlink > 1 && !sh.inodes.lock().unwrap().insert((e.dev, e.ino)) {
                        return; // hard link we've already counted
                    }
                    total += e.size;
                    files += 1;
                    let name = OsStr::from_bytes(e.name);
                    if is_marker(name) {
                        markers.push(name.to_owned());
                        manifest_mtime = manifest_mtime.max(e.mtime);
                    }
                    if detect && e.size >= INSTALLER_MIN {
                        let path = dir.join(name);
                        let is_installer = path
                            .extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| INSTALLER_EXTS.contains(&x.to_ascii_lowercase().as_str()));
                        let hit = FileHit { path, size: e.size, mtime: e.mtime };
                        let mut d = sh.data.lock().unwrap();
                        if is_installer {
                            d.installers.push(hit);
                        } else if e.size >= LARGE_FILE_MIN {
                            d.large.push(hit);
                        }
                    }
                }
                list::Kind::Other => {}
            }
        });
        if let Err(e) = listed {
            sh.error(dir, &e);
        }
        sh.files.fetch_add(files, Relaxed);
        sh.bytes.fetch_add(total, Relaxed);

        let markers = &markers;
        let sub: (u64, i64, bool) = subdirs
            .into_par_iter()
            .map(|path| {
                if !self.skip.is_empty() && self.skip.contains(&path) {
                    return (0, 0, false);
                }
                if let Some(info) = self.pre.get(&path) {
                    return (info.size, info.mtime, false);
                }
                if self.defer.is_some_and(|d| d.contains(&path)) {
                    return (0, 0, true);
                }
                let kind = if detect { detect_artifact(&path, markers) } else { None };
                let (info, has_deferred) = self.walk_dir(&path, dev, detect && kind.is_none());
                if let Some(kind) = kind {
                    sh.data.lock().unwrap().artifacts.push(Artifact {
                        kind,
                        path: path.clone(),
                        size: info.size,
                        mtime: info.mtime.max(manifest_mtime),
                    });
                    sh.bump();
                }
                // Folders holding a hotspot are kept regardless of size: the hotspot's
                // size gets added to them when the walks are merged.
                if info.size >= DIR_RECORD_MIN || has_deferred {
                    sh.record_dir(path, info);
                }
                (info.size, info.mtime, has_deferred)
            })
            .reduce(|| (0, 0, false), |a, b| (a.0 + b.0, a.1.max(b.1), a.2 || b.2));

        (DirInfo { size: total + sub.0, mtime: newest.max(sub.1) }, sub.2)
    }
}

/// Files whose presence identifies a project type (see `detect_artifact`).
fn is_marker(name: &OsStr) -> bool {
    const NAMES: &[&str] = &[
        "Cargo.toml", "pom.xml", "project.godot", "build.gradle", "build.gradle.kts", "settings.gradle",
        "settings.gradle.kts", "pubspec.yaml", "CMakeLists.txt", "Podfile", "package.json", "Package.swift",
        "mix.exs", "build.zig",
    ];
    let b = name.as_bytes();
    NAMES.iter().any(|n| n.as_bytes() == b) || b.ends_with(b".uproject") || b.ends_with(b".tf")
}

/// Directory listing: one entry per child with the few attributes we need.
mod list {
    use std::io;
    use std::path::Path;

    #[derive(PartialEq, Eq)]
    pub enum Kind {
        File,
        Dir,
        Other,
    }

    pub struct Entry<'a> {
        pub name: &'a [u8],
        pub kind: Kind,
        pub dev: u64,
        pub ino: u64,
        pub nlink: u32,
        /// Bytes allocated on disk.
        pub size: u64,
        pub mtime: i64,
    }

    /// macOS: `getattrlistbulk` returns names, types and sizes for many entries
    /// per syscall, instead of one `lstat` per file. Several times faster.
    #[cfg(target_os = "macos")]
    pub fn list_dir(dir: &Path, mut f: impl FnMut(Entry<'_>)) -> io::Result<()> {
        use libc::{attrlist, attrreference_t, attribute_set_t, c_void};
        use std::cell::RefCell;
        use std::mem::size_of;
        use std::os::unix::ffi::OsStrExt;

        const ATTR_CMN_ERROR: u32 = 0x2000_0000;
        const VREG: u32 = 1;
        const VDIR: u32 = 2;

        thread_local!(static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 256 * 1024]));

        struct Fd(i32);
        impl Drop for Fd {
            fn drop(&mut self) {
                unsafe { libc::close(self.0) };
            }
        }

        fn rd<T: Copy>(buf: &[u8], at: usize) -> T {
            assert!(at + size_of::<T>() <= buf.len());
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at) as *const T) }
        }

        let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(fd);

        let mut al: attrlist = unsafe { std::mem::zeroed() };
        al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FILEID;
        al.fileattr = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;

        BUF.with(|b| {
            let mut buf = b.borrow_mut();
            loop {
                let n = unsafe {
                    libc::getattrlistbulk(
                        fd.0,
                        &mut al as *mut attrlist as *mut c_void,
                        buf.as_mut_ptr() as *mut c_void,
                        buf.len(),
                        0,
                    )
                };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                if n == 0 {
                    return Ok(());
                }
                let mut off = 0usize;
                for _ in 0..n {
                    // Layout: length, returned attrs, then each returned attribute in bit order
                    // (error first, then name, devid, objtype, modtime, fileid; then file attrs).
                    let len = rd::<u32>(&buf, off) as usize;
                    let mut p = off + 4;
                    let ret: attribute_set_t = rd(&buf, p);
                    p += size_of::<attribute_set_t>();
                    let has = |bit: u32| ret.commonattr & bit != 0;
                    let mut err = 0u32;
                    if has(ATTR_CMN_ERROR) {
                        err = rd(&buf, p);
                        p += 4;
                    }
                    let mut name: &[u8] = b"";
                    if has(libc::ATTR_CMN_NAME) {
                        let r: attrreference_t = rd(&buf, p);
                        let start = (p as isize + r.attr_dataoffset as isize) as usize;
                        let end = (start + r.attr_length as usize).min(buf.len());
                        name = &buf[start..end];
                        if let Some(stripped) = name.strip_suffix(&[0]) {
                            name = stripped;
                        }
                        p += size_of::<attrreference_t>();
                    }
                    let mut dev = 0u64;
                    if has(libc::ATTR_CMN_DEVID) {
                        dev = rd::<i32>(&buf, p) as u32 as u64;
                        p += 4;
                    }
                    let mut objtype = 0u32;
                    if has(libc::ATTR_CMN_OBJTYPE) {
                        objtype = rd(&buf, p);
                        p += 4;
                    }
                    let mut mtime = 0i64;
                    if has(libc::ATTR_CMN_MODTIME) {
                        mtime = rd(&buf, p);
                        p += size_of::<libc::timespec>();
                    }
                    let mut ino = 0u64;
                    if has(libc::ATTR_CMN_FILEID) {
                        ino = rd(&buf, p);
                        p += 8;
                    }
                    let mut nlink = 1u32;
                    if ret.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
                        nlink = rd(&buf, p);
                        p += 4;
                    }
                    let mut size = 0u64;
                    if ret.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
                        size = rd::<i64>(&buf, p).max(0) as u64;
                    }
                    off += len;

                    if err != 0 || name.is_empty() {
                        continue;
                    }
                    let kind = match objtype {
                        VREG => Kind::File,
                        VDIR => Kind::Dir,
                        _ => Kind::Other,
                    };
                    f(Entry { name, kind, dev, ino, nlink, size, mtime });
                }
            }
        })
    }

    /// Portable fallback: readdir + lstat.
    #[cfg(not(target_os = "macos"))]
    pub fn list_dir(dir: &Path, mut f: impl FnMut(Entry<'_>)) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(md) = entry.metadata() else { continue };
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                Kind::Dir
            } else if ft.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            let name = entry.file_name();
            f(Entry {
                name: name.as_bytes(),
                kind,
                dev: md.dev(),
                ino: md.ino(),
                nlink: md.nlink() as u32,
                size: md.blocks() * 512,
                mtime: md.mtime(),
            });
        }
        Ok(())
    }
}

/// Decide whether `path` is a regenerable build artifact, based on its name and
/// the names of its siblings (project marker files).
fn detect_artifact(path: &Path, siblings: &[OsString]) -> Option<ArtifactKind> {
    use ArtifactKind::*;
    let name = path.file_name()?.to_str()?;
    let has = |n: &str| siblings.iter().any(|s| s == n);
    let has_ext = |e: &str| {
        siblings
            .iter()
            .any(|s| Path::new(s).extension().is_some_and(|x| x == e))
    };
    let gradle = || {
        has("build.gradle") || has("build.gradle.kts") || has("settings.gradle") || has("settings.gradle.kts")
    };

    Some(match name {
        "node_modules" => NodeModules,
        "target" if has("Cargo.toml") => CargoTarget,
        "target" if has("pom.xml") => MavenTarget,
        "Library" if has("Assets") && has("ProjectSettings") => UnityLibrary,
        "Intermediate" | "DerivedDataCache" if has_ext("uproject") => UnrealCache,
        ".godot" if has("project.godot") => GodotCache,
        ".venv" | "venv" | "env" if path.join("pyvenv.cfg").exists() => PythonVenv,
        "build" | ".gradle" if gradle() => GradleBuild,
        "build" | ".dart_tool" if has("pubspec.yaml") => FlutterBuild,
        "build" if has("CMakeLists.txt") => CMakeBuild,
        n if n.starts_with("cmake-build-") && has("CMakeLists.txt") => CMakeBuild,
        "Pods" if has("Podfile") => Pods,
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular" if has("package.json") => {
            JsBuildCache
        }
        ".build" if has("Package.swift") => SwiftBuild,
        "_build" | "deps" if has("mix.exs") => ElixirBuild,
        ".zig-cache" | "zig-cache" | "zig-out" if has("build.zig") => ZigCache,
        ".stack-work" | "dist-newstyle" => HaskellBuild,
        ".terraform" if has_ext("tf") => Terraform,
        _ => return None,
    })
}
