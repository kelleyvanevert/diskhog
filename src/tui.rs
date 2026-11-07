//! Full-screen interactive UI: live results while scanning, per-category item
//! selection (by hand or by age), and cleanup actions.

use crate::actions::{self, Job, Mode};
use crate::rules::{self, Category, Context, Item, Safety};
use crate::scan::{self, Shared};
use crate::util::{age, bar, display_path, human, thousands, truncate_middle};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

const AGE_STEPS: &[(i64, &str)] = &[(90, "3 months"), (180, "6 months"), (365, "1 year"), (730, "2 years"), (30, "1 month")];

#[derive(PartialEq, Clone, Copy)]
enum View {
    Categories,
    Explorer,
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Cats,
    Items,
}

#[derive(PartialEq, Clone, Copy)]
enum Sort {
    Size,
    /// Most recently used first: move to the first item that's too old and press b.
    Recent,
    /// Least recently used first.
    Oldest,
}

enum Dialog {
    Remove { items: Vec<RemoveItem>, total: u64 },
    /// Cleanup command(s) of a category; with several, the user picks one by number.
    Command { options: Vec<(String, String)>, refresh: Vec<PathBuf>, provider: Option<&'static str> },
    Help,
    Errors(Vec<String>),
}

#[derive(Clone)]
struct RemoveItem {
    path: PathBuf,
    size: u64,
    label: String,
    cmd: Option<Vec<String>>,
    provider: Option<&'static str>,
    order: u8,
}

struct ExRow {
    path: PathBuf,
    name: String,
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
    selectable: bool,
}

pub struct App {
    sh: Arc<Shared>,
    ctx: Context,
    roots: Vec<PathBuf>,
    expected: u64,
    on_done: Box<dyn FnMut(u64)>,
    start: Instant,
    elapsed: Option<Duration>,

    cats: Vec<Category>,
    seen_version: u64,
    last_analyze: Instant,
    /// Item path -> category title, for tags in the explorer.
    path_cat: HashMap<PathBuf, String>,

    view: View,
    focus: Focus,
    cat_state: TableState,
    cat_id: Option<String>,
    /// Until the user navigates, the cursor stays on the biggest category.
    user_moved: bool,
    item_state: TableState,
    sort: Sort,
    age_idx: usize,
    selected: BTreeMap<PathBuf, u64>,

    ex_path: Option<PathBuf>,
    ex_state: TableState,
    ex_stack: Vec<usize>,

    dialog: Option<Dialog>,
    job: Arc<Job>,
    job_was_running: bool,
    /// Summary of the last finished removal batch, shown in the panel for a moment.
    job_summary: Option<(String, bool, Instant)>,
    status: Option<(String, Instant)>,
    disk: Option<(u64, u64)>,
    disk_checked: Instant,
    quit: bool,
}

impl App {
    pub fn new(sh: Arc<Shared>, ctx: Context, roots: Vec<PathBuf>, expected: u64, on_done: Box<dyn FnMut(u64)>) -> Self {
        let mut cat_state = TableState::default();
        cat_state.select(Some(0));
        App {
            sh,
            ctx,
            roots,
            expected: expected.max(1),
            on_done,
            start: Instant::now(),
            elapsed: None,
            cats: vec![],
            seen_version: u64::MAX,
            last_analyze: Instant::now() - Duration::from_secs(10),
            path_cat: HashMap::new(),
            view: View::Categories,
            focus: Focus::Cats,
            cat_state,
            cat_id: None,
            user_moved: false,
            item_state: TableState::default(),
            sort: Sort::Size,
            age_idx: 0,
            selected: BTreeMap::new(),
            ex_path: None,
            ex_state: TableState::default().with_selected(Some(0)),
            ex_stack: vec![],
            dialog: None,
            job: Arc::new(Job::default()),
            job_was_running: false,
            job_summary: None,
            status: None,
            disk: None,
            disk_checked: Instant::now() - Duration::from_secs(10),
            quit: false,
        }
    }

    pub fn run(mut self) -> io::Result<()> {
        let mut terminal = ratatui::init();
        let res = self.main_loop(&mut terminal);
        ratatui::restore();
        res
    }

    fn main_loop(&mut self, t: &mut DefaultTerminal) -> io::Result<()> {
        while !self.quit {
            self.tick();
            t.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.on_key(k, t)?;
            }
        }
        Ok(())
    }

    // ── State updates ─────────────────────────────────────────────────────

    fn phase(&self) -> u8 {
        self.sh.phase.load(Relaxed)
    }

    fn tick(&mut self) {
        let v = self.sh.version.load(Relaxed);
        let done = self.phase() == scan::PHASE_DONE;
        if v != self.seen_version && (done || self.last_analyze.elapsed() > Duration::from_millis(400)) {
            self.seen_version = v;
            self.last_analyze = Instant::now();
            self.cats = {
                let d = self.sh.data.lock().unwrap();
                rules::analyze(&d, &self.ctx)
            };
            self.path_cat = self
                .cats
                .iter()
                .flat_map(|c| c.items.iter().map(move |i| (i.path.clone(), c.title.clone())))
                .collect();
            self.restore_cat_selection();
        }
        if done && self.elapsed.is_none() {
            self.elapsed = Some(self.start.elapsed());
            (self.on_done)(self.sh.bytes.load(Relaxed));
        }
        if self.disk_checked.elapsed() > Duration::from_secs(2) {
            self.disk = crate::disk_usage(&self.ctx.home);
            self.disk_checked = Instant::now();
        }
        let running = self.job.running.load(Relaxed);
        if self.job_was_running && !running {
            let errors = self.job.errors.lock().unwrap().clone();
            let n = self.job.done_items.load(Relaxed) - errors.len();
            let freed = self.job.batch_freed.load(Relaxed);
            let trashed = self.job.batch_trashed.load(Relaxed);
            let mut parts = Vec::new();
            if freed > 0 {
                parts.push(format!("Freed {}", human(freed)));
            }
            if trashed > 0 {
                parts.push(format!("moved {} to the Trash (empty it to free the space)", human(trashed)));
            }
            let mut msg = format!("✓ {} · {n} item{}", parts.join(", "), if n == 1 { "" } else { "s" });
            if n == 0 {
                msg = "Nothing could be removed".into();
            }
            if !errors.is_empty() {
                msg.push_str(&format!(" · {} failed", errors.len()));
                self.dialog = Some(Dialog::Errors(errors));
            }
            self.job_summary = Some((msg, n > 0, Instant::now()));
            let touched: Vec<&'static str> = self.job.touched.lock().unwrap().drain().collect();
            for p in touched {
                crate::providers::refresh(&self.sh, p);
                if let Some(c) = self.cats.iter().find(|c| c.provider == Some(p)) {
                    let (sh, paths) = (self.sh.clone(), c.refresh.clone());
                    std::thread::spawn(move || paths.iter().for_each(|p| sh.refresh(p)));
                }
            }
        }
        self.job_was_running = running;
    }

    fn restore_cat_selection(&mut self) {
        let idx = self
            .cat_id
            .as_ref()
            .filter(|_| self.user_moved)
            .and_then(|id| self.cats.iter().position(|c| &c.id == id))
            .unwrap_or_else(|| self.cat_state.selected().unwrap_or(0).min(self.cats.len().saturating_sub(1)));
        self.cat_state.select(Some(idx));
        self.cat_id = self.cats.get(idx).map(|c| c.id.clone());
        let n = self.sorted_items().len();
        match self.item_state.selected() {
            Some(i) if i >= n => self.item_state.select(n.checked_sub(1)),
            None if n > 0 => self.item_state.select(Some(0)),
            _ => {}
        }
    }

    fn flash(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    fn cur_cat(&self) -> Option<&Category> {
        self.cat_state.selected().and_then(|i| self.cats.get(i))
    }

    fn sorted_items(&self) -> Vec<Item> {
        let Some(c) = self.cur_cat() else { return vec![] };
        let mut items = c.items.clone();
        // Items without a date ("never used") count as oldest; unremovable rows go last.
        match self.sort {
            Sort::Size => items.sort_by_key(|i| !i.selectable),
            Sort::Recent => items.sort_by_key(|i| (!i.selectable, std::cmp::Reverse(i.mtime.unwrap_or(i64::MIN)))),
            Sort::Oldest => items.sort_by_key(|i| (!i.selectable, i.mtime.unwrap_or(i64::MIN))),
        }
        items
    }

    fn cat_selected_size(&self, c: &Category) -> u64 {
        c.items.iter().filter_map(|i| self.selected.get(&i.path)).sum()
    }

    fn toggle(&mut self, path: &Path, size: u64) {
        if self.selected.remove(path).is_none() {
            self.selected.insert(path.to_path_buf(), size);
        }
    }

    /// What `d` would remove: the selection, or else the row under the cursor.
    fn removal_set(&self) -> Vec<(PathBuf, u64)> {
        let mut items: Vec<(PathBuf, u64)> = if !self.selected.is_empty() {
            self.selected.iter().map(|(p, s)| (p.clone(), *s)).collect()
        } else {
            match (self.view, self.focus) {
                (View::Categories, Focus::Items) => self
                    .item_state
                    .selected()
                    .and_then(|i| self.sorted_items().get(i).cloned())
                    .filter(|i| i.selectable)
                    .map(|i| vec![(i.path, i.size)])
                    .unwrap_or_default(),
                (View::Explorer, _) => self
                    .ex_state
                    .selected()
                    .and_then(|i| self.explorer_rows().1.into_iter().nth(i))
                    .filter(|r| r.selectable)
                    .map(|r| vec![(r.path, r.size)])
                    .unwrap_or_default(),
                _ => vec![],
            }
        };
        // Skip what's already queued or being removed.
        let busy: Vec<PathBuf> = self.job.queued().into_iter().chain(self.job.current_path()).collect();
        items.retain(|(p, _)| !busy.iter().any(|b| p.starts_with(b)));
        // Drop items inside another selected item.
        let all: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
        items.retain(|(p, _)| !all.iter().any(|o| o != p && p.starts_with(o)));
        items.sort_by(|a, b| b.1.cmp(&a.1));
        items
    }

    /// The removal set with what's needed to remove each item (label, command).
    fn removal_items(&self) -> Vec<RemoveItem> {
        let known: HashMap<&Path, (&Item, Option<&'static str>)> =
            self.cats.iter().flat_map(|c| c.items.iter().map(move |i| (i.path.as_path(), (i, c.provider)))).collect();
        self.removal_set()
            .into_iter()
            .map(|(path, size)| match known.get(path.as_path()) {
                Some((item, provider)) => RemoveItem {
                    label: item.label.clone().unwrap_or_else(|| display_path(&path, &self.ctx.home)),
                    cmd: item.cmd.clone(),
                    provider: if item.cmd.is_some() { *provider } else { None },
                    order: item.order,
                    path,
                    size,
                },
                None => RemoveItem { label: display_path(&path, &self.ctx.home), cmd: None, provider: None, order: 0, path, size },
            })
            // A synthetic item (docker://…) that vanished from the list can't be removed anymore.
            .filter(|i| i.cmd.is_some() || i.path.is_absolute())
            .collect()
    }

    /// Rows of the explorer for the current folder (or the scan roots at the top).
    fn explorer_rows(&self) -> (u64, Vec<ExRow>) {
        let d = self.sh.data.lock().unwrap();
        let Some(cur) = &self.ex_path else {
            let mut rows: Vec<ExRow> = self
                .roots
                .iter()
                .map(|r| {
                    let info = d.dirs.get(r).copied().unwrap_or_default();
                    let finished = d.roots.iter().any(|(p, _)| p == r);
                    ExRow {
                        path: r.clone(),
                        name: display_path(r, &self.ctx.home) + if finished { "" } else { "  (scanning…)" },
                        size: info.size,
                        mtime: Some(info.mtime).filter(|&m| m > 0),
                        is_dir: true,
                        selectable: false,
                    }
                })
                .collect();
            rows.sort_by(|a, b| b.size.cmp(&a.size));
            return (rows.iter().map(|r| r.size).sum(), rows);
        };
        let total = d.dirs.get(cur).map_or(0, |i| i.size);
        let mut rows: Vec<ExRow> = d
            .dirs
            .iter()
            .filter(|(p, _)| p.parent() == Some(cur.as_path()))
            .map(|(p, i)| ExRow {
                path: p.clone(),
                name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                size: i.size,
                mtime: Some(i.mtime).filter(|&m| m > 0),
                is_dir: true,
                selectable: true,
            })
            .collect();
        rows.extend(d.large.iter().chain(d.installers.iter()).filter(|f| f.path.parent() == Some(cur.as_path())).map(|f| {
            ExRow {
                path: f.path.clone(),
                name: f.path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                size: f.size,
                mtime: Some(f.mtime),
                is_dir: false,
                selectable: true,
            }
        }));
        rows.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        let listed: u64 = rows.iter().map(|r| r.size).sum();
        if total > listed {
            rows.push(ExRow {
                path: cur.clone(),
                name: "(files and folders under 10 MB)".into(),
                size: total - listed,
                mtime: None,
                is_dir: false,
                selectable: false,
            });
        }
        (total, rows)
    }

    // ── Input ─────────────────────────────────────────────────────────────

    fn on_key(&mut self, k: KeyEvent, t: &mut DefaultTerminal) -> io::Result<()> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return Ok(());
        }
        if let Some(dialog) = self.dialog.take() {
            return self.on_dialog_key(k, dialog, t);
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = if self.view == View::Categories { View::Explorer } else { View::Categories }
            }
            KeyCode::Char('1') => self.view = View::Categories,
            KeyCode::Char('2') => self.view = View::Explorer,
            KeyCode::Char('C') => {
                self.selected.clear();
                self.flash("Selection cleared".into());
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Char('D') => {
                let items = self.removal_items();
                if items.is_empty() {
                    self.flash("Nothing selected (space selects; some categories can only be cleaned with x)".into());
                } else {
                    let total = items.iter().map(|i| i.size).sum();
                    self.dialog = Some(Dialog::Remove { items, total });
                }
            }
            KeyCode::Char('r') => {
                if let Some(p) = self.cursor_path().filter(|p| p.exists()) {
                    let _ = std::process::Command::new("open").arg("-R").arg(&p).spawn();
                    self.flash(format!("Revealed in Finder: {}", display_path(&p, &self.ctx.home)));
                }
            }
            _ => match self.view {
                View::Categories => self.on_categories_key(k),
                View::Explorer => self.on_explorer_key(k),
            },
        }
        Ok(())
    }

    fn cursor_path(&self) -> Option<PathBuf> {
        match (self.view, self.focus) {
            (View::Categories, Focus::Items) => {
                self.item_state.selected().and_then(|i| self.sorted_items().get(i).map(|i| i.path.clone()))
            }
            (View::Categories, Focus::Cats) => self.cur_cat().and_then(|c| c.items.first().map(|i| i.path.clone())),
            (View::Explorer, _) => self.ex_state.selected().and_then(|i| self.explorer_rows().1.into_iter().nth(i)).map(|r| r.path),
        }
    }

    fn on_categories_key(&mut self, k: KeyEvent) {
        let page = 10;
        match (self.focus, k.code) {
            (_, KeyCode::Char('s')) => {
                self.sort = match self.sort {
                    Sort::Size => Sort::Recent,
                    Sort::Recent => Sort::Oldest,
                    Sort::Oldest => Sort::Size,
                };
                self.item_state.select(Some(0));
                *self.item_state.offset_mut() = 0;
            }
            (_, KeyCode::Char('a')) => {
                let items: Vec<Item> = self.sorted_items().into_iter().filter(|i| i.selectable).collect();
                let size: u64 = items.iter().map(|i| i.size).sum();
                for i in &items {
                    self.selected.insert(i.path.clone(), i.size);
                }
                self.flash(format!("Selected all {} items · {}", items.len(), human(size)));
            }
            (_, KeyCode::Char('n')) => {
                for i in self.sorted_items() {
                    self.selected.remove(&i.path);
                }
            }
            (_, KeyCode::Char('o')) => self.select_older(),
            (Focus::Items, KeyCode::Char('b')) => self.select_from_cursor(),
            (_, KeyCode::Char('x')) => match self.cur_cat() {
                Some(c) if !c.cmds.is_empty() => {
                    self.dialog =
                        Some(Dialog::Command { options: c.cmds.clone(), refresh: c.refresh.clone(), provider: c.provider })
                }
                _ => self.flash("This category has no cleanup command; select items and press d".into()),
            },
            (Focus::Cats, KeyCode::Up | KeyCode::Char('k')) => self.move_cat(-1),
            (Focus::Cats, KeyCode::Down | KeyCode::Char('j')) => self.move_cat(1),
            (Focus::Cats, KeyCode::PageUp) => self.move_cat(-page),
            (Focus::Cats, KeyCode::PageDown) => self.move_cat(page),
            (Focus::Cats, KeyCode::Home | KeyCode::Char('g')) => self.move_cat(-10_000),
            (Focus::Cats, KeyCode::End | KeyCode::Char('G')) => self.move_cat(10_000),
            (Focus::Cats, KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')) => {
                self.focus = Focus::Items;
                self.user_moved = true;
                if self.item_state.selected().is_none() {
                    self.item_state.select(Some(0));
                }
            }
            (Focus::Cats, KeyCode::Char(' ')) => {
                // Toggle the whole category.
                let items: Vec<Item> = self.sorted_items().into_iter().filter(|i| i.selectable).collect();
                let all = items.iter().all(|i| self.selected.contains_key(&i.path));
                for i in items {
                    if all {
                        self.selected.remove(&i.path);
                    } else {
                        self.selected.insert(i.path, i.size);
                    }
                }
            }
            (Focus::Items, KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc) => self.focus = Focus::Cats,
            (Focus::Items, KeyCode::Up | KeyCode::Char('k')) => self.move_item(-1),
            (Focus::Items, KeyCode::Down | KeyCode::Char('j')) => self.move_item(1),
            (Focus::Items, KeyCode::PageUp) => self.move_item(-page),
            (Focus::Items, KeyCode::PageDown) => self.move_item(page),
            (Focus::Items, KeyCode::Home | KeyCode::Char('g')) => self.move_item(-100_000),
            (Focus::Items, KeyCode::End | KeyCode::Char('G')) => self.move_item(100_000),
            (Focus::Items, KeyCode::Char(' ')) => {
                if let Some(item) = self.item_state.selected().and_then(|i| self.sorted_items().get(i).cloned()) {
                    if item.selectable {
                        self.toggle(&item.path, item.size);
                    } else {
                        self.flash(self.cur_cat().map_or(String::new(), |c| format!("Not deletable from here. {}", c.how)));
                    }
                    self.move_item(1);
                }
            }
            (Focus::Items, KeyCode::Enter) => {
                // Jump into the explorer at this item.
                if let Some(item) = self.item_state.selected().and_then(|i| self.sorted_items().get(i).cloned())
                    && item.path.is_dir()
                {
                    self.ex_path = Some(item.path);
                    self.ex_stack.clear();
                    self.ex_state.select(Some(0));
                    self.view = View::Explorer;
                }
            }
            _ => {}
        }
    }

    /// Make the cursor row the cutoff: select it and everything below, unselect above.
    fn select_from_cursor(&mut self) {
        let cur = self.item_state.selected().unwrap_or(0);
        let (mut n, mut size) = (0, 0);
        for (idx, i) in self.sorted_items().into_iter().enumerate().filter(|(_, i)| i.selectable) {
            if idx >= cur {
                n += 1;
                size += i.size;
                self.selected.insert(i.path, i.size);
            } else {
                self.selected.remove(&i.path);
            }
        }
        let hint = match self.sort {
            Sort::Recent => "  (everything used less recently than the cursor row)",
            _ => "",
        };
        self.flash(format!("Selected {n} items from here down · {}{hint}", human(size)));
    }

    fn select_older(&mut self) {
        let (days, label) = AGE_STEPS[self.age_idx];
        self.age_idx = (self.age_idx + 1) % AGE_STEPS.len();
        let cutoff = self.ctx.now - days * 86_400;
        let items = self.sorted_items();
        let mut n = 0;
        let mut size = 0;
        for i in items.iter().filter(|i| i.selectable) {
            self.selected.remove(&i.path);
            if i.mtime.is_some_and(|m| m < cutoff) {
                self.selected.insert(i.path.clone(), i.size);
                n += 1;
                size += i.size;
            }
        }
        let next = AGE_STEPS[self.age_idx].1;
        self.flash(format!("Selected {n} items unused for {label}+ · {}   (o again: {next}+)", human(size)));
    }

    fn move_cat(&mut self, delta: i64) {
        let n = self.cats.len() as i64;
        if n == 0 {
            return;
        }
        let i = (self.cat_state.selected().unwrap_or(0) as i64 + delta).clamp(0, n - 1) as usize;
        self.user_moved = true;
        self.cat_state.select(Some(i));
        self.cat_id = Some(self.cats[i].id.clone());
        self.item_state.select(Some(0));
        *self.item_state.offset_mut() = 0;
    }

    fn move_item(&mut self, delta: i64) {
        let n = self.sorted_items().len() as i64;
        if n == 0 {
            return;
        }
        let i = (self.item_state.selected().unwrap_or(0) as i64 + delta).clamp(0, n - 1) as usize;
        self.item_state.select(Some(i));
    }

    fn on_explorer_key(&mut self, k: KeyEvent) {
        let rows = self.explorer_rows().1;
        let n = rows.len() as i64;
        let cur = self.ex_state.selected().unwrap_or(0);
        let mv = |s: &mut Self, d: i64| {
            if n > 0 {
                s.ex_state.select(Some((cur as i64 + d).clamp(0, n - 1) as usize));
            }
        };
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => mv(self, -1),
            KeyCode::Down | KeyCode::Char('j') => mv(self, 1),
            KeyCode::PageUp => mv(self, -10),
            KeyCode::PageDown => mv(self, 10),
            KeyCode::Home | KeyCode::Char('g') => mv(self, -100_000),
            KeyCode::End | KeyCode::Char('G') => mv(self, 100_000),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(r) = rows.get(cur).filter(|r| r.is_dir) {
                    self.ex_stack.push(cur);
                    self.ex_path = Some(r.path.clone());
                    self.ex_state.select(Some(0));
                    *self.ex_state.offset_mut() = 0;
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Esc => {
                if let Some(p) = self.ex_path.take() {
                    self.ex_path = if self.roots.contains(&p) { None } else { p.parent().map(Path::to_path_buf) };
                    self.ex_state.select(Some(self.ex_stack.pop().unwrap_or(0)));
                }
            }
            KeyCode::Char(' ') => {
                if let Some(r) = rows.get(cur) {
                    if r.selectable {
                        let (p, s) = (r.path.clone(), r.size);
                        self.toggle(&p, s);
                    }
                    mv(self, 1);
                }
            }
            _ => {}
        }
    }

    fn on_dialog_key(&mut self, k: KeyEvent, dialog: Dialog, t: &mut DefaultTerminal) -> io::Result<()> {
        match dialog {
            Dialog::Remove { items, total } => {
                let has_files = items.iter().any(|i| i.cmd.is_none());
                let mode = match k.code {
                    KeyCode::Char('t') if has_files => Some(Mode::Trash),
                    KeyCode::Char('D') => Some(Mode::Delete),
                    KeyCode::Enter if !has_files => Some(Mode::Delete),
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => None,
                    _ => {
                        self.dialog = Some(Dialog::Remove { items, total });
                        return Ok(());
                    }
                };
                if let Some(mode) = mode {
                    for i in &items {
                        self.selected.remove(&i.path);
                    }
                    let tasks = items
                        .into_iter()
                        .map(|i| actions::Task {
                            path: i.path,
                            size: i.size,
                            mode,
                            cmd: i.cmd,
                            provider: i.provider,
                            order: i.order,
                            label: i.label,
                        })
                        .collect();
                    actions::enqueue(&self.job, &self.sh, tasks, self.ctx.home.clone());
                    self.job_was_running = true;
                    self.job_summary = None;
                }
            }
            Dialog::Command { options, refresh, provider } => {
                let pick = match k.code {
                    KeyCode::Char('y') | KeyCode::Enter if options.len() == 1 => Some(0),
                    KeyCode::Char(c @ '1'..='9') => Some(c as usize - '1' as usize).filter(|&i| i < options.len()),
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => return Ok(()),
                    _ => None,
                };
                match pick {
                    Some(i) => {
                        let cmd = options[i].1.clone();
                        self.run_command(t, &cmd, refresh, provider)?
                    }
                    None => self.dialog = Some(Dialog::Command { options, refresh, provider }),
                }
            }
            Dialog::Help | Dialog::Errors(_) => {}
        }
        Ok(())
    }

    /// Leave the TUI, run a cleanup command in the user's shell, come back and re-measure.
    fn run_command(
        &mut self,
        t: &mut DefaultTerminal,
        cmd: &str,
        refresh: Vec<PathBuf>,
        provider: Option<&'static str>,
    ) -> io::Result<()> {
        ratatui::restore();
        println!("\n\x1b[1m$ {cmd}\x1b[0m\n");
        let status = std::process::Command::new("sh").arg("-c").arg(cmd).status();
        match status {
            Ok(s) if s.success() => println!("\n\x1b[32m✓ done\x1b[0m"),
            Ok(s) => println!("\n\x1b[31m✗ exited with {s}\x1b[0m"),
            Err(e) => println!("\n\x1b[31m✗ could not run: {e}\x1b[0m"),
        }
        println!("Press Enter to return to diskhog…");
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        *t = ratatui::init();
        t.clear()?;
        let sh = self.sh.clone();
        if let Some(p) = provider {
            crate::providers::refresh(&sh, p);
        }
        std::thread::spawn(move || {
            for p in refresh {
                sh.refresh(&p);
            }
        });
        self.flash("Re-measuring after the command…".into());
        Ok(())
    }

    // ── Drawing ───────────────────────────────────────────────────────────

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] =
            Layout::vertical([Constraint::Length(3), Constraint::Fill(1), Constraint::Length(4)]).areas(f.area());
        self.draw_header(f, header);
        let show_job = self.job.running.load(Relaxed)
            || self.job_summary.as_ref().is_some_and(|(_, _, at)| at.elapsed() < Duration::from_secs(5));
        let body = if show_job {
            let [main, panel] = Layout::vertical([Constraint::Fill(1), Constraint::Length(5)]).areas(body);
            self.draw_job(f, panel);
            main
        } else {
            body
        };
        match self.view {
            View::Categories => self.draw_categories(f, body),
            View::Explorer => self.draw_explorer(f, body),
        }
        self.draw_footer(f, footer);
        if self.dialog.is_some() {
            self.draw_dialog(f);
        }
    }

    fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.start.elapsed().as_millis() / 80) as usize % FRAMES.len()]
    }

    /// The removal panel: spinner, progress bar and current item while removing,
    /// then a short summary when the batch is done.
    fn draw_job(&self, f: &mut Frame, area: Rect) {
        let job = &self.job;
        if !job.running.load(Relaxed) {
            let Some((msg, ok, _)) = &self.job_summary else { return };
            let color = if *ok { Color::Green } else { Color::Red };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(color))
                .title(Span::styled(" Removal finished ", Style::new().bold()));
            let text = vec![Line::raw(""), Line::styled(format!(" {msg}"), Style::new().fg(color).bold())];
            f.render_widget(Paragraph::new(text).block(block), area);
            return;
        }
        let (path, mode, label) =
            job.current.lock().unwrap().clone().unwrap_or((PathBuf::new(), Mode::Delete, String::new()));
        let (verb, color) = match mode {
            _ if !path.is_absolute() => ("Removing with docker/simctl", Color::Red),
            Mode::Trash => ("Moving to Trash", Color::Yellow),
            Mode::Delete => ("Deleting permanently", Color::Red),
        };
        let done_items = job.done_items.load(Relaxed);
        let total_items = job.total_items.load(Relaxed);
        let bytes_done = job.done_bytes.load(Relaxed) + job.item_bytes.load(Relaxed);
        let total_bytes = job.total_bytes.load(Relaxed);
        let frac = job.fraction();

        let line1 = Line::from(vec![
            Span::styled(format!(" {} ", self.spinner()), Style::new().fg(color).bold()),
            Span::styled(verb, Style::new().fg(color).bold()),
            Span::raw(format!(
                "   item {} of {}   ·   {} of {}",
                (done_items + 1).min(total_items),
                total_items,
                human(bytes_done.min(total_bytes)),
                human(total_bytes)
            )),
        ]);
        let inner_w = area.width.saturating_sub(2) as usize;
        let bar_w = inner_w.saturating_sub(9);
        let filled = ((frac * bar_w as f64).round() as usize).min(bar_w);
        let line2 = Line::from(vec![
            Span::raw(" "),
            Span::styled("━".repeat(filled), Style::new().fg(color)),
            Span::styled("─".repeat(bar_w - filled), Style::new().dark_gray()),
            Span::styled(format!(" {:>3.0}%", frac * 100.0), Style::new().bold()),
        ]);
        let line3 = Line::from(vec![
            Span::styled(" ↳ ", Style::new().dark_gray()),
            Span::raw(truncate_middle(&label, inner_w.saturating_sub(4))),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(Span::styled(" Removing ", Style::new().bold()));
        f.render_widget(Paragraph::new(vec![line1, line2, line3]).block(block), area);
    }

    /// Checkbox cell for a row, showing removal state when relevant.
    fn check_cell(&self, path: &Path, selectable: bool, busy: &(Option<PathBuf>, std::collections::HashSet<PathBuf>)) -> (Span<'static>, Option<Style>) {
        if busy.0.as_deref() == Some(path) {
            return (Span::styled(format!(" {} ", self.spinner()), Style::new().yellow().bold()), Some(Style::new().yellow()));
        }
        if busy.1.contains(path) {
            return (Span::styled(" … ", Style::new().yellow()), Some(Style::new().dark_gray()));
        }
        let span = match (selectable, self.selected.contains_key(path)) {
            (false, _) => Span::raw(" · ").dark_gray(),
            (true, true) => Span::styled("[✓]", Style::new().green().bold()),
            (true, false) => Span::raw("[ ]"),
        };
        (span, None)
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let tab = |label: &str, active: bool| {
            if active {
                Span::styled(format!(" {label} "), Style::new().bg(Color::Blue).fg(Color::White).bold())
            } else {
                Span::styled(format!(" {label} "), Style::new().fg(Color::Gray))
            }
        };
        let mut l1 = vec![
            Span::styled(" diskhog ", Style::new().bg(Color::Magenta).fg(Color::White).bold()),
            Span::raw("  "),
            tab("1 Categories", self.view == View::Categories),
            tab("2 Explorer", self.view == View::Explorer),
        ];
        if let Some((total, avail)) = self.disk {
            let style = if avail < total / 10 { Style::new().fg(Color::Red).bold() } else { Style::new().fg(Color::Green) };
            l1.push(Span::raw("   Disk: "));
            l1.push(Span::styled(format!("{} free", human(avail)), style));
            l1.push(Span::styled(format!(" of {}", human(total)), Style::new().dark_gray()));
        }

        let phase = self.phase();
        let bytes = self.sh.bytes.load(Relaxed);
        let done = phase == scan::PHASE_DONE;
        let pct = if done { 100 } else { (bytes * 100 / self.expected).min(99) };
        let label = match phase {
            scan::PHASE_HOTSPOTS | scan::PHASE_FULL => "Scanning…",
            scan::PHASE_DONE => "Scan complete",
            _ => "Starting…",
        };
        let elapsed = self.elapsed.unwrap_or_else(|| self.start.elapsed());
        let width = 30usize;
        let filled = (pct as usize * width) / 100;
        let l2 = Line::from(vec![
            Span::styled(format!(" {label:<31}"), Style::new().bold()),
            Span::styled("━".repeat(filled), Style::new().fg(if done { Color::Green } else { Color::Cyan })),
            Span::styled("─".repeat(width - filled), Style::new().dark_gray()),
            Span::styled(format!(" {pct:>3}%"), Style::new().bold()),
            Span::styled(
                format!(
                    "   {} files · {} · {:.0}s",
                    thousands(self.sh.files.load(Relaxed)),
                    human(bytes),
                    elapsed.as_secs_f64()
                ),
                Style::new().dark_gray(),
            ),
        ]);

        let sel: u64 = self.removal_total();
        let mut l3 = vec![Span::raw(" ")];
        if !self.selected.is_empty() {
            l3.push(Span::styled(
                format!("✓ {} selected · {}", self.selected.len(), human(sel)),
                Style::new().fg(Color::Black).bg(Color::Green).bold(),
            ));
            l3.push(Span::raw("  "));
        }
        let freed = self.job.freed.load(Relaxed);
        let trashed = self.job.trashed.load(Relaxed);
        if freed > 0 {
            l3.push(Span::styled(format!("Freed {}  ", human(freed)), Style::new().green().bold()));
        }
        if trashed > 0 {
            l3.push(Span::styled(format!("In Trash {} (empty it to free)  ", human(trashed)), Style::new().yellow()));
        }
        f.render_widget(Paragraph::new(vec![Line::from(l1), l2, Line::from(l3)]), area);
    }

    fn removal_total(&self) -> u64 {
        if self.selected.is_empty() {
            return 0;
        }
        let all: Vec<&PathBuf> = self.selected.keys().collect();
        self.selected.iter().filter(|(p, _)| !all.iter().any(|o| *o != *p && p.starts_with(o))).map(|(_, s)| s).sum()
    }

    fn draw_categories(&mut self, f: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)]).areas(area);
        let focus_style = |on: bool| if on { Style::new().fg(Color::Cyan) } else { Style::new().dark_gray() };

        // Left: categories.
        let rows: Vec<Row> = self
            .cats
            .iter()
            .map(|c| {
                let sel = self.cat_selected_size(c);
                let mut title = vec![Span::raw(c.title.clone())];
                if sel > 0 {
                    title.push(Span::styled(format!("  ✓{}", human(sel)), Style::new().green().bold()));
                }
                Row::new(vec![
                    Cell::from(size_span(c.size)),
                    Cell::from(safety_span(c.safety)),
                    Cell::from(Line::from(title)),
                ])
            })
            .collect();
        let total: u64 = self.cats.iter().filter(|c| c.safety != Safety::Info).map(|c| c.size).sum();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(focus_style(self.focus == Focus::Cats))
            .title(Line::from(vec![
                Span::styled(" Cleanup candidates ", Style::new().bold()),
                Span::styled(format!("{} ", human(total)), Style::new().dark_gray()),
            ]));
        let hl = if self.focus == Focus::Cats {
            Style::new().bg(Color::Blue).fg(Color::White)
        } else {
            Style::new().bg(Color::DarkGray)
        };
        let table = Table::new(rows, [Constraint::Length(9), Constraint::Length(6), Constraint::Fill(1)])
            .block(block)
            .row_highlight_style(hl);
        f.render_stateful_widget(table, left, &mut self.cat_state);

        // Right: items of the current category.
        let items = self.sorted_items();
        let Some(cat) = self.cur_cat() else {
            let msg = if self.phase() == scan::PHASE_DONE { "Nothing found." } else { "Looking…" };
            f.render_widget(Paragraph::new(msg).block(Block::bordered().border_type(BorderType::Rounded)), right);
            return;
        };
        let opened = cat.age_label != "Modified";
        let path_w = right.width.saturating_sub(3 + 9 + 11 + 6) as usize;
        let busy = (self.job.current_path(), self.job.queued());
        let rows: Vec<Row> = items
            .iter()
            .map(|i| {
                let (check, busy_style) = self.check_cell(&i.path, i.selectable, &busy);
                let when = match i.mtime {
                    Some(m) => {
                        let s = age(m, self.ctx.now);
                        if m < self.ctx.now - self.ctx.stale_days * 86_400 {
                            Span::styled(s, Style::new().yellow())
                        } else {
                            Span::raw(s)
                        }
                    }
                    None if i.opened || (cat.provider.is_some() && i.selectable) => Span::raw("never").dark_gray(),
                    None => Span::raw(""),
                };
                let path = i.label.clone().unwrap_or_else(|| display_path(&i.path, &self.ctx.home));
                let label = match &i.note {
                    Some(n) => {
                        // Shorten the note before the name: the name is what identifies the item.
                        let name_w = path.chars().count().min(path_w.saturating_sub(12)).max(path_w.saturating_sub(n.chars().count() + 2));
                        let note_w = path_w.saturating_sub(name_w + 2);
                        Line::from(vec![
                            Span::raw(truncate_middle(&path, name_w)),
                            Span::styled(format!("  {}", truncate_middle(n, note_w)), Style::new().dark_gray().italic()),
                        ])
                    }
                    None => Line::from(truncate_middle(&path, path_w)),
                };
                let mut row = Row::new(vec![Cell::from(check), Cell::from(size_span(i.size)), Cell::from(when), Cell::from(label)]);
                if let Some(st) = busy_style {
                    row = row.style(st);
                } else if !i.selectable {
                    row = row.style(Style::new().dark_gray());
                }
                row
            })
            .collect();
        let sort_label = match self.sort {
            Sort::Size => "by size",
            Sort::Recent if opened => "most recently used first",
            Sort::Recent => "newest first",
            Sort::Oldest if opened => "least recently used first",
            Sort::Oldest => "oldest first",
        };
        let header = Row::new(vec!["", "Size", cat.age_label, if cat.provider.is_some() { "Item" } else { "Path" }])
            .style(Style::new().dark_gray().add_modifier(Modifier::UNDERLINED));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(focus_style(self.focus == Focus::Items))
            .title(Line::from(vec![
                Span::styled(format!(" {} ", cat.title), Style::new().bold()),
                Span::styled(format!("{} · {} ", human(cat.size), sort_label), Style::new().dark_gray()),
            ]));
        let hl = if self.focus == Focus::Items {
            Style::new().bg(Color::Blue).fg(Color::White)
        } else {
            Style::new().bg(Color::DarkGray)
        };
        let table = Table::new(rows, [Constraint::Length(3), Constraint::Length(9), Constraint::Length(9), Constraint::Fill(1)])
            .header(header)
            .block(block)
            .column_spacing(2)
            .row_highlight_style(hl);
        f.render_stateful_widget(table, right, &mut self.item_state);
    }

    fn draw_explorer(&mut self, f: &mut Frame, area: Rect) {
        let (total, rows) = self.explorer_rows();
        let title = match &self.ex_path {
            Some(p) => display_path(p, &self.ctx.home),
            None => "Scanned locations".into(),
        };
        let name_w = area.width.saturating_sub(3 + 9 + 12 + 9 + 12) as usize;
        let busy = (self.job.current_path(), self.job.queued());
        let trows: Vec<Row> = rows
            .iter()
            .map(|r| {
                let (check, busy_style) = if r.selectable {
                    self.check_cell(&r.path, true, &busy)
                } else {
                    (Span::raw("   "), None)
                };
                let frac = if total > 0 { r.size as f64 / total as f64 } else { 0.0 };
                let mut name = vec![Span::styled(
                    truncate_middle(&format!("{}{}", r.name, if r.is_dir { "/" } else { "" }), name_w.saturating_sub(2)),
                    if r.is_dir { Style::new().bold() } else { Style::new() },
                )];
                if let Some(cat) = self.path_cat.get(&r.path).filter(|_| r.selectable) {
                    name.push(Span::styled(format!("  ◆ {cat}"), Style::new().magenta()));
                }
                let mut row = Row::new(vec![
                    Cell::from(check),
                    Cell::from(size_span(r.size)),
                    Cell::from(Span::styled(bar(frac, 10), Style::new().cyan())),
                    Cell::from(r.mtime.map(|m| age(m, self.ctx.now)).unwrap_or_default()),
                    Cell::from(Line::from(name)),
                ]);
                if let Some(st) = busy_style {
                    row = row.style(st);
                } else if !r.selectable && self.ex_path.is_some() {
                    row = row.style(Style::new().dark_gray().italic());
                }
                row
            })
            .collect();
        let header = Row::new(vec!["", "Size", "", "Modified", "Name"])
            .style(Style::new().dark_gray().add_modifier(Modifier::UNDERLINED));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().cyan())
            .title(Line::from(vec![
                Span::styled(format!(" {title} "), Style::new().bold()),
                Span::styled(format!("{} ", human(total)), Style::new().dark_gray()),
            ]));
        let table = Table::new(
            trows,
            [Constraint::Length(3), Constraint::Length(9), Constraint::Length(10), Constraint::Length(9), Constraint::Fill(1)],
        )
        .header(header)
        .block(block)
        .column_spacing(2)
        .row_highlight_style(Style::new().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, area, &mut self.ex_state);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let key = |k: &str| Span::styled(format!(" {k} "), Style::new().bg(Color::DarkGray).fg(Color::White));
        let desc = |d: &str| Span::styled(format!(" {d}  "), Style::new().gray());
        let mut lines: Vec<Line> = Vec::new();

        if self.view == View::Categories
            && let Some(c) = self.cur_cat()
        {
            let mut l = vec![Span::styled(format!(" {}", c.how), Style::new().cyan())];
            match c.cmds.as_slice() {
                [] => {}
                [(_, cmd)] => {
                    l.push(Span::raw("   "));
                    l.push(key("x"));
                    l.push(Span::styled(format!(" $ {cmd}"), Style::new().cyan().bold()));
                }
                many => {
                    l.push(Span::raw("   "));
                    l.push(key("x"));
                    l.push(Span::styled(format!(" {} cleanup options", many.len()), Style::new().cyan().bold()));
                }
            }
            lines.push(Line::from(l));
            match (&self.status, &c.note) {
                (Some((msg, at)), _) if at.elapsed() < Duration::from_secs(6) => {
                    lines.push(Line::from(Span::styled(format!(" {msg}"), Style::new().yellow().bold())))
                }
                (_, Some(note)) => lines.push(Line::from(Span::styled(format!(" {note}"), Style::new().yellow()))),
                _ => lines.push(Line::raw("")),
            }
        } else {
            lines.push(Line::raw(""));
            match &self.status {
                Some((msg, at)) if at.elapsed() < Duration::from_secs(6) => {
                    lines.push(Line::from(Span::styled(format!(" {msg}"), Style::new().yellow().bold())))
                }
                _ => lines.push(Line::raw("")),
            }
        }

        let next_age = AGE_STEPS[self.age_idx].1;
        let hints: Vec<Span> = match self.view {
            View::Categories => vec![
                key("↑↓"), desc("move"), key("←→"), desc("pane"), key("space"), desc("select"),
                key("o"), desc(&format!("unused {next_age}+")), key("a"), desc("all"), key("n"), desc("none"),
                key("b"), desc("this & below"), key("s"), desc("sort"), key("d"), desc("remove"), key("x"), desc("command"),
                key("r"), desc("reveal"), key("tab"), desc("explorer"), key("?"), desc("help"), key("q"), desc("quit"),
            ],
            View::Explorer => vec![
                key("↑↓"), desc("move"), key("→/enter"), desc("open"), key("←"), desc("up"), key("space"), desc("select"),
                key("d"), desc("remove"), key("r"), desc("reveal"), key("tab"), desc("categories"), key("?"), desc("help"),
                key("q"), desc("quit"),
            ],
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(hints));
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_dialog(&self, f: &mut Frame) {
        let (title, body, width): (String, Text, u16) = match self.dialog.as_ref().unwrap() {
            Dialog::Remove { items, total } => {
                let mut lines: Vec<Line> = vec![Line::raw("")];
                for i in items.iter().take(10) {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        size_span(i.size),
                        Span::raw("  "),
                        Span::raw(truncate_middle(&i.label, 70)),
                    ]));
                }
                if items.len() > 10 {
                    let rest: u64 = items[10..].iter().map(|i| i.size).sum();
                    lines.push(Line::styled(format!("  {:>9}  … and {} more", human(rest), items.len() - 10), Style::new().dark_gray()));
                }
                lines.push(Line::raw(""));
                let has_files = items.iter().any(|i| i.cmd.is_none());
                let has_cmds = items.iter().any(|i| i.cmd.is_some());
                if has_files {
                    lines.push(Line::from(vec![
                        Span::styled("  t ", Style::new().bold().yellow()),
                        Span::raw(" Move to Trash"),
                        Span::styled("   (space comes back once you empty the Trash)", Style::new().dark_gray()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("  D ", Style::new().bold().red()),
                        Span::raw(" Delete permanently"),
                        Span::styled("   (frees space now · cannot be undone)", Style::new().dark_gray()),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("  D/Enter ", Style::new().bold().red()),
                        Span::raw(" Remove"),
                        Span::styled("   (cannot be undone)", Style::new().dark_gray()),
                    ]));
                }
                if has_cmds {
                    // "docker rmi", "docker volume rm", "xcrun simctl delete", ... without the ids.
                    let mut verbs: Vec<String> = items
                        .iter()
                        .filter_map(|i| i.cmd.as_ref())
                        .map(|c| {
                            let n = if ["volume", "builder", "container", "simctl"].contains(&c[1].as_str()) { 3 } else { 2 };
                            c.iter().take(n).cloned().collect::<Vec<_>>().join(" ")
                        })
                        .collect();
                    verbs.sort();
                    verbs.dedup();
                    let n = items.iter().filter(|i| i.cmd.is_some()).count();
                    lines.push(Line::styled(format!("      {n} removed with: {}", verbs.join(" · ")), Style::new().dark_gray()));
                    let volumes: Vec<&RemoveItem> = items.iter().filter(|i| i.path.starts_with("docker://volume")).collect();
                    if !volumes.is_empty() {
                        let size: u64 = volumes.iter().map(|i| i.size).sum();
                        lines.push(Line::styled(
                            format!("  ⚠ Includes {} Docker volume(s), {}: their data (e.g. databases) is gone for good.", volumes.len(), human(size)),
                            Style::new().yellow().bold(),
                        ));
                    }
                }
                lines.push(Line::from(vec![Span::styled("  Esc", Style::new().bold()), Span::raw(" Cancel")]));
                if self.job.running.load(Relaxed) {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled("  A removal is running; these will be queued after it.", Style::new().yellow()));
                }
                (format!(" Remove {} item{} · {} ", items.len(), if items.len() == 1 { "" } else { "s" }, human(*total)), Text::from(lines), 90)
            }
            Dialog::Command { options, .. } if options.len() == 1 => (
                " Run cleanup command ".into(),
                Text::from(vec![
                    Line::raw(""),
                    Line::raw("  diskhog will pause and run this in your shell:"),
                    Line::raw(""),
                    Line::styled(format!("    $ {}", options[0].1), Style::new().cyan().bold()),
                    Line::raw(""),
                    Line::raw("  Afterwards the affected folders are measured again."),
                    Line::raw(""),
                    Line::from(vec![
                        Span::styled("  y/Enter", Style::new().bold().green()),
                        Span::raw(" Run    "),
                        Span::styled("Esc", Style::new().bold()),
                        Span::raw(" Cancel"),
                    ]),
                ]),
                80,
            ),
            Dialog::Command { options, .. } => {
                let mut lines = vec![Line::raw(""), Line::raw("  Pick one; diskhog pauses and runs it in your shell, then re-measures:"), Line::raw("")];
                for (i, (desc, cmd)) in options.iter().enumerate() {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", i + 1), Style::new().bold().green()),
                        Span::styled(desc.clone(), Style::new().bold()),
                    ]));
                    lines.push(Line::styled(format!("      $ {cmd}"), Style::new().cyan()));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![Span::styled("  Esc", Style::new().bold()), Span::raw(" Cancel")]));
                (" Cleanup options ".into(), Text::from(lines), 100)
            }
            Dialog::Errors(errs) => {
                let mut lines = vec![Line::raw("")];
                for e in errs.iter().take(12) {
                    lines.push(Line::styled(format!("  {}", truncate_middle(e, 100)), Style::new().red()));
                }
                if errs.len() > 12 {
                    lines.push(Line::raw(format!("  … and {} more", errs.len() - 12)));
                }
                lines.push(Line::raw(""));
                lines.push(Line::raw("  Some items need admin rights or Full Disk Access. Press any key."));
                (format!(" {} item(s) could not be removed ", errs.len()), Text::from(lines), 110)
            }
            Dialog::Help => (" Help ".into(), help_text(), 100),
        };
        let height = body.lines.len() as u16 + 2;
        let area = centered(f.area(), width, height);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().yellow())
                    .title(Span::styled(title, Style::new().bold())),
            ),
            area,
        );
    }
}

fn help_text() -> Text<'static> {
    let row = |k: &'static str, d: &'static str| {
        Line::from(vec![Span::styled(format!("  {k:<14}"), Style::new().bold().cyan()), Span::raw(d)])
    };
    Text::from(vec![
        Line::raw(""),
        Line::styled("  Categories", Style::new().bold().underlined()),
        row("↑↓ / j k", "move · PgUp/PgDn/g/G jump"),
        row("→ / Enter", "go into a category's items; Enter on an item opens it in the explorer"),
        row("space", "select item (on a category: select/unselect all of it)"),
        row("o", "select items unused for 3 months; press again for 6 months, 1 year, …"),
        row("a / n / C", "select all in category / none in category / clear all selections"),
        row("s", "sort: size → most recently used first → least recently used first"),
        row("b", "select this item and everything below it (unselects the ones above)"),
        row("x", "run the category's own cleanup command (e.g. docker system prune)"),
        Line::raw(""),
        Line::styled("  Everywhere", Style::new().bold().underlined()),
        row("d", "remove the selection (or the item under the cursor): Trash or delete"),
        row("r", "reveal in Finder"),
        row("tab / 1 / 2", "switch between categories and the folder explorer"),
        row("q / Ctrl-C", "quit"),
        Line::raw(""),
        Line::styled("  SAFE = cache/build output that is rebuilt automatically.", Style::new().green()),
        Line::styled("  REVIEW = your data or installs: look before you delete.", Style::new().yellow()),
        Line::styled("  Dimmed items can't be removed directly; use the shown advice or command.", Style::new().dark_gray()),
        Line::raw(""),
        Line::raw("  Press any key to close."),
    ])
}

fn size_span(b: u64) -> Span<'static> {
    let s = format!("{:>9}", human(b));
    if b >= 10_000_000_000 {
        Span::styled(s, Style::new().red().bold())
    } else if b >= 1_000_000_000 {
        Span::styled(s, Style::new().yellow().bold())
    } else {
        Span::raw(s)
    }
}

fn safety_span(s: Safety) -> Span<'static> {
    match s {
        Safety::Safe => Span::styled("SAFE", Style::new().green()),
        Safety::Review => Span::styled("REVIEW", Style::new().yellow()),
        Safety::Info => Span::styled("INFO", Style::new().dark_gray()),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width.saturating_sub(4)))]).flex(Flex::Center).areas(v);
    r
}
