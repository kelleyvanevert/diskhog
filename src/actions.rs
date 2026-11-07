//! Things that change the disk: moving to Trash, deleting, and app usage lookups.

use crate::scan::Shared;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Trash,
    Delete,
}

pub struct Task {
    pub path: PathBuf,
    pub size: u64,
    pub mode: Mode,
    /// Remove by running this command (Docker, simctl) instead of touching files.
    pub cmd: Option<Vec<String>>,
    pub provider: Option<&'static str>,
    pub order: u8,
    /// What to show while it runs (label or path).
    pub label: String,
}

/// The removal queue and its progress, shared with the UI. Removals run one at a
/// time in a background worker; more can be queued while it's busy.
#[derive(Default)]
pub struct Job {
    queue: Mutex<VecDeque<Task>>,
    pub running: AtomicBool,
    /// Path, mode and display label of the item being removed right now.
    pub current: Mutex<Option<(PathBuf, Mode, String)>>,
    /// Providers whose items were removed in this batch (to re-query afterwards).
    pub touched: Mutex<HashSet<&'static str>>,
    /// Current batch (resets when a new batch starts on an idle queue).
    pub done_items: AtomicUsize,
    pub total_items: AtomicUsize,
    pub done_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    /// Bytes removed so far from the current item (deletes report as they go).
    pub item_bytes: AtomicU64,
    pub batch_trashed: AtomicU64,
    pub batch_freed: AtomicU64,
    pub errors: Mutex<Vec<String>>,
    /// Session totals.
    pub freed: AtomicU64,
    pub trashed: AtomicU64,
}

impl Job {
    /// Paths waiting in the queue (not including the current one).
    pub fn queued(&self) -> HashSet<PathBuf> {
        self.queue.lock().unwrap().iter().map(|t| t.path.clone()).collect()
    }

    pub fn current_path(&self) -> Option<PathBuf> {
        self.current.lock().unwrap().as_ref().map(|(p, _, _)| p.clone())
    }

    /// Overall progress of the batch, counting partly deleted folders.
    pub fn fraction(&self) -> f64 {
        let total = self.total_bytes.load(Relaxed).max(1);
        let done = self.done_bytes.load(Relaxed) + self.item_bytes.load(Relaxed);
        (done as f64 / total as f64).min(1.0)
    }
}

/// Queue `tasks` for removal; starts the background worker if it isn't running.
pub fn enqueue(job: &Arc<Job>, sh: &Arc<Shared>, tasks: Vec<Task>, home: PathBuf) {
    let mut q = job.queue.lock().unwrap();
    if !job.running.load(Relaxed) {
        job.done_items.store(0, Relaxed);
        job.total_items.store(0, Relaxed);
        for c in [&job.done_bytes, &job.total_bytes, &job.item_bytes, &job.batch_trashed, &job.batch_freed] {
            c.store(0, Relaxed);
        }
        job.errors.lock().unwrap().clear();
        job.touched.lock().unwrap().clear();
    }
    // Within what's added now: containers before the images they use, etc.
    let mut tasks = tasks;
    tasks.sort_by_key(|t| t.order);
    job.total_items.fetch_add(tasks.len(), Relaxed);
    job.total_bytes.fetch_add(tasks.iter().map(|t| t.size).sum(), Relaxed);
    q.extend(tasks);
    if job.running.swap(true, Relaxed) {
        return; // the worker will pick them up
    }
    drop(q);
    let (job, sh) = (job.clone(), sh.clone());
    std::thread::spawn(move || worker(&job, &sh, &home));
}

fn worker(job: &Job, sh: &Shared, home: &Path) {
    loop {
        let task = {
            // Decide "queue empty → stop" under the lock, so enqueue can't miss us.
            let mut q = job.queue.lock().unwrap();
            match q.pop_front() {
                Some(t) => t,
                None => {
                    *job.current.lock().unwrap() = None;
                    job.running.store(false, Relaxed);
                    sh.bump();
                    return;
                }
            }
        };
        *job.current.lock().unwrap() = Some((task.path.clone(), task.mode, task.label.clone()));
        job.item_bytes.store(0, Relaxed);
        let res = match (&task.cmd, task.mode) {
            (Some(cmd), _) => crate::providers::run(cmd, std::time::Duration::from_secs(900))
                .map(|_| ())
                .map_err(io::Error::other),
            (None, Mode::Trash) => trash(&task.path, home),
            (None, Mode::Delete) => delete_tree(&task.path, &job.item_bytes),
        };
        if let Some(p) = task.provider {
            job.touched.lock().unwrap().insert(p);
        }
        match res {
            Ok(()) => {
                if let Some(p) = task.provider {
                    crate::providers::forget(sh, p, &task.path);
                }
                sh.apply_removal(&task.path, task.size);
                let (batch, session) = match (task.cmd.is_some(), task.mode) {
                    (true, _) => (&job.batch_freed, &job.freed),
                    (false, Mode::Trash) => (&job.batch_trashed, &job.trashed),
                    (false, Mode::Delete) => (&job.batch_freed, &job.freed),
                };
                batch.fetch_add(task.size, Relaxed);
                session.fetch_add(task.size, Relaxed);
            }
            Err(e) => {
                // A partial delete still freed something: re-measure what's left.
                if task.cmd.is_none() && task.mode == Mode::Delete && task.path.exists() {
                    sh.refresh(&task.path);
                }
                job.errors.lock().unwrap().push(format!("{}: {e}", task.label));
            }
        }
        job.item_bytes.store(0, Relaxed);
        job.done_bytes.fetch_add(task.size, Relaxed);
        job.done_items.fetch_add(1, Relaxed);
    }
}

/// Delete a file or folder tree in parallel, adding each removed file's size to
/// `progress`, so the UI shows a moving bar even for one huge folder.
fn delete_tree(path: &Path, progress: &AtomicU64) -> io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if !md.is_dir() {
        std::fs::remove_file(path)?;
        progress.fetch_add(md.blocks() * 512, Relaxed);
        return Ok(());
    }
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(path)?.collect::<io::Result<_>>()?;
    let errors: Vec<io::Error> = entries
        .into_par_iter()
        .filter_map(|e| {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => delete_tree(&p, progress).err(),
                _ => {
                    let size = e.metadata().map_or(0, |m| m.blocks() * 512);
                    match std::fs::remove_file(&p) {
                        Ok(()) => {
                            progress.fetch_add(size, Relaxed);
                            None
                        }
                        Err(err) => Some(err),
                    }
                }
            }
        })
        .collect();
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => std::fs::remove_dir(path),
    }
}

/// Move to the Trash. A rename into ~/.Trash is instant for anything on the same
/// volume; otherwise ask Finder (which knows about per-volume trash folders).
fn trash(path: &Path, home: &Path) -> io::Result<()> {
    let trash_dir = home.join(".Trash");
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    let mut dest = trash_dir.join(name);
    if dest.symlink_metadata().is_ok() {
        let stamp = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        dest = trash_dir.join(format!("{} {stamp}", name.to_string_lossy()));
    }
    match std::fs::rename(path, &dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            let p = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!("tell application \"Finder\" to delete (POSIX file \"{p}\" as alias)");
            let out = std::process::Command::new("osascript").args(["-e", &script]).output()?;
            if out.status.success() {
                Ok(())
            } else {
                Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
            }
        }
    }
}

/// When each app was last launched: the newest access time of the executables in
/// `Contents/MacOS` (the scan itself never reads file contents, so it doesn't disturb
/// them). For folders like /Applications/Unity, the newest of the .app bundles inside.
pub fn app_launch_times(home: &Path) -> HashMap<PathBuf, i64> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in [
        PathBuf::from("/Applications"),
        home.join("Applications"),
        PathBuf::from("/Applications/Unity/Hub/Editor"),
        PathBuf::from("/Users/Shared/Epic Games"),
    ] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            candidates.extend(rd.flatten().map(|e| e.path()));
        }
    }
    let mut out = HashMap::new();
    for p in candidates {
        let t = if p.extension().is_some_and(|e| e == "app") {
            launched(&p)
        } else {
            std::fs::read_dir(&p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|c| c.extension().is_some_and(|e| e == "app"))
                .filter_map(|c| launched(&c))
                .max()
        };
        if let Some(t) = t {
            out.insert(p, t);
        }
    }
    out
}

fn launched(app: &Path) -> Option<i64> {
    std::fs::read_dir(app.join("Contents/MacOS"))
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().ok()?.accessed().ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .max()
}
