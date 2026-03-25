use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use humansize::{format_size, BINARY};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Terminal,
};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(name = "fdu", about = "Fast disk usage analyzer with a streaming TUI")]
struct Cli {
    /// Path to scan (default: current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Number of top entries to display
    #[arg(short = 'n', long = "top", default_value = "20")]
    top: usize,

    /// Minimum file size to display (e.g. 100MB, 1GB)
    #[arg(long = "min-size")]
    min_size: Option<String>,

    /// Only show files, skip directory aggregation
    #[arg(long = "files-only")]
    files_only: bool,

    /// Print results to stdout without TUI (for benchmarks/scripting)
    #[arg(long = "no-tui")]
    no_tui: bool,
}

#[derive(Clone)]
struct SizeEntry {
    path: String,
    size: u64,
}

struct ScanLists {
    top_files: Vec<SizeEntry>,
    top_dirs: Vec<SizeEntry>,
    current_path: String,
}

struct ScanState {
    lists: Mutex<ScanLists>,
    total_bytes: AtomicU64,
    file_count: AtomicU64,
    dir_count: AtomicU64,
    error_count: AtomicU64,
    done: AtomicBool,
}

impl ScanState {
    fn new() -> Self {
        Self {
            lists: Mutex::new(ScanLists {
                top_files: Vec::new(),
                top_dirs: Vec::new(),
                current_path: String::new(),
            }),
            total_bytes: AtomicU64::new(0),
            file_count: AtomicU64::new(0),
            dir_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            done: AtomicBool::new(false),
        }
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_uppercase();
    let (num_str, multiplier) = if s.ends_with("TB") {
        (&s[..s.len() - 2], 1u64 << 40)
    } else if s.ends_with("GB") {
        (&s[..s.len() - 2], 1u64 << 30)
    } else if s.ends_with("MB") {
        (&s[..s.len() - 2], 1u64 << 20)
    } else if s.ends_with("KB") {
        (&s[..s.len() - 2], 1u64 << 10)
    } else if s.ends_with('B') {
        (&s[..s.len() - 1], 1u64)
    } else {
        (s.as_str(), 1u64)
    };
    let num: f64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("Invalid size: {}", s))?;
    Ok((num * multiplier as f64) as u64)
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok()
}

fn shorten_path(path: &str) -> String {
    if let Some(home) = home_dir() {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

fn insert_top_n(list: &mut Vec<SizeEntry>, entry: SizeEntry, n: usize) {
    let pos = list
        .binary_search_by(|e| entry.size.cmp(&e.size))
        .unwrap_or_else(|p| p);

    if pos < n {
        list.insert(pos, entry);
        if list.len() > n {
            list.pop();
        }
    }
}

// ─── macOS: getattrlistbulk + rayon parallel walker ───────────────────────────

#[cfg(target_os = "macos")]
mod bulk {
    use std::os::fd::RawFd;

    pub const ATTR_BIT_MAP_COUNT: u16 = 5;
    pub const ATTR_CMN_RETURNED_ATTRS: u32 = 0x80000000;
    pub const ATTR_CMN_NAME: u32 = 0x00000001;
    pub const ATTR_CMN_OBJTYPE: u32 = 0x00000008;
    pub const ATTR_FILE_DATAALLOCSIZE: u32 = 0x00000400;
    pub const VREG: u32 = 1;
    pub const VDIR: u32 = 2;

    #[repr(C)]
    pub struct AttrList {
        pub bitmapcount: u16,
        pub reserved: u16,
        pub commonattr: u32,
        pub volattr: u32,
        pub dirattr: u32,
        pub fileattr: u32,
        pub forkattr: u32,
    }

    unsafe extern "C" {
        pub fn getattrlistbulk(
            dirfd: i32,
            alist: *const AttrList,
            attribute_buffer: *mut u8,
            buffer_size: usize,
            options: u64,
        ) -> i32;
    }

    pub struct BulkEntry {
        pub name: String,
        pub obj_type: u32,
        pub size: u64,
    }

    fn read_u32(buf: &[u8], off: usize) -> u32 {
        u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
    }

    fn read_i32(buf: &[u8], off: usize) -> i32 {
        i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
    }

    pub fn read_dir_bulk(fd: RawFd) -> Vec<BulkEntry> {
        let attrs = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_DATAALLOCSIZE,
            forkattr: 0,
        };

        const BUF_SIZE: usize = 256 * 1024;
        let mut buf = vec![0u8; BUF_SIZE];
        let mut entries = Vec::new();

        loop {
            let count = unsafe {
                getattrlistbulk(fd, &attrs, buf.as_mut_ptr(), BUF_SIZE, 0)
            };
            if count <= 0 {
                break;
            }

            let mut offset = 0usize;
            for _ in 0..count as usize {
                if offset + 28 > BUF_SIZE {
                    break;
                }

                let entry_len = read_u32(&buf, offset) as usize;
                let entry_start = offset;
                offset += 4;

                // returned_attrs: attribute_set_t (5 × u32 = 20 bytes)
                let ret_common = read_u32(&buf, offset);
                offset += 4;
                offset += 4; // volattr
                offset += 4; // dirattr
                let ret_file = read_u32(&buf, offset);
                offset += 4;
                offset += 4; // forkattr

                // Parse common attrs
                let mut name = String::new();
                let mut obj_type: u32 = 0;
                let mut size: u64 = 0;

                if ret_common & ATTR_CMN_NAME != 0 {
                    let name_ref_offset = offset;
                    let name_data_offset = read_i32(&buf, offset);
                    let _name_length = read_u32(&buf, offset + 4);
                    offset += 8;

                    let name_start = (name_ref_offset as i64 + name_data_offset as i64) as usize;
                    if name_start < entry_start + entry_len {
                        // Read null-terminated string
                        let end = buf[name_start..entry_start + entry_len]
                            .iter()
                            .position(|&b| b == 0)
                            .map(|p| name_start + p)
                            .unwrap_or(entry_start + entry_len);
                        name = String::from_utf8_lossy(&buf[name_start..end]).to_string();
                    }
                }

                if ret_common & ATTR_CMN_OBJTYPE != 0 {
                    obj_type = read_u32(&buf, offset);
                    offset += 4;
                }

                // Parse file attrs (only present for regular files)
                if ret_file & ATTR_FILE_DATAALLOCSIZE != 0 {
                    if offset + 8 <= entry_start + entry_len {
                        size = u64::from_ne_bytes(
                            buf[offset..offset + 8].try_into().unwrap(),
                        );
                    }
                }

                entries.push(BulkEntry {
                    name,
                    obj_type,
                    size,
                });

                offset = entry_start + entry_len;
            }
        }

        entries
    }
}

#[cfg(target_os = "macos")]
fn scan(
    state: Arc<ScanState>,
    root: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
) {
    let dir_sizes: Mutex<HashMap<PathBuf, u64>> = Mutex::new(HashMap::new());
    let min_top_size = AtomicU64::new(0);
    let dirs_processed = AtomicU64::new(0);

    rayon::scope(|scope| {
        walk_dir_bulk(
            scope,
            root.clone(),
            &state,
            &dir_sizes,
            &min_top_size,
            &dirs_processed,
            &root,
            top_n,
            files_only,
            &stop,
        );
    });

    if !files_only {
        flush_top_dirs(&state, &dir_sizes, top_n);
    }

    state.done.store(true, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn walk_dir_bulk<'s>(
    scope: &rayon::Scope<'s>,
    dir: PathBuf,
    state: &'s ScanState,
    dir_sizes: &'s Mutex<HashMap<PathBuf, u64>>,
    min_top_size: &'s AtomicU64,
    dirs_processed: &'s AtomicU64,
    root: &'s PathBuf,
    top_n: usize,
    files_only: bool,
    stop: &'s AtomicBool,
) {
    use std::os::fd::AsRawFd;

    if stop.load(Ordering::Relaxed) {
        return;
    }

    // Open dir, bulk-read all entries, close fd before recursing
    let entries = {
        let dir_file = match std::fs::File::open(&dir) {
            Ok(f) => f,
            Err(_) => {
                state.error_count.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        bulk::read_dir_bulk(dir_file.as_raw_fd())
        // fd closed here
    };

    state.dir_count.fetch_add(1, Ordering::Relaxed);

    let mut local_bytes: u64 = 0;
    let mut local_file_count: u64 = 0;
    let mut top_candidates: Vec<SizeEntry> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();

    for entry in &entries {
        if entry.obj_type == bulk::VDIR {
            subdirs.push(dir.join(&entry.name));
        } else if entry.obj_type == bulk::VREG {
            let size = entry.size;
            local_bytes += size;
            local_file_count += 1;

            let current_min = min_top_size.load(Ordering::Relaxed);
            if size > current_min
                || state.file_count.load(Ordering::Relaxed) + local_file_count
                    <= top_n as u64
            {
                let path_str = dir.join(&entry.name).to_string_lossy().to_string();
                top_candidates.push(SizeEntry {
                    path: path_str,
                    size,
                });
            }
        }
    }

    // Flush counters (lock-free)
    state
        .total_bytes
        .fetch_add(local_bytes, Ordering::Relaxed);
    state
        .file_count
        .fetch_add(local_file_count, Ordering::Relaxed);

    // Flush top-N file candidates
    if !top_candidates.is_empty() {
        let mut lists = state.lists.lock().unwrap();
        if let Some(last) = top_candidates.last() {
            lists.current_path = shorten_path(&last.path);
        }
        for entry in top_candidates {
            insert_top_n(&mut lists.top_files, entry, top_n);
        }
        min_top_size.store(
            lists.top_files.last().map(|e| e.size).unwrap_or(0),
            Ordering::Relaxed,
        );
    }

    // Flush dir size accumulation (batched per-directory)
    if !files_only && local_bytes > 0 {
        // Pre-compute ancestor chain once for this directory
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut anc = dir.clone();
        loop {
            ancestors.push(anc.clone());
            if anc == *root {
                break;
            }
            if !anc.pop() || !anc.starts_with(root) {
                break;
            }
        }

        let mut map = dir_sizes.lock().unwrap();
        for anc in &ancestors {
            if let Some(v) = map.get_mut(anc) {
                *v += local_bytes;
            } else {
                map.insert(anc.clone(), local_bytes);
            }
        }
        drop(map);

        let count = dirs_processed.fetch_add(1, Ordering::Relaxed);
        if count % 500 == 0 && count > 0 {
            flush_top_dirs(state, dir_sizes, top_n);
        }
    }

    // Spawn child directories into rayon's work-stealing pool
    for subdir in subdirs {
        scope.spawn(move |s| {
            walk_dir_bulk(
                s,
                subdir,
                state,
                dir_sizes,
                min_top_size,
                dirs_processed,
                root,
                top_n,
                files_only,
                stop,
            );
        });
    }
}

#[cfg(target_os = "macos")]
fn flush_top_dirs(
    state: &ScanState,
    dir_sizes: &Mutex<HashMap<PathBuf, u64>>,
    top_n: usize,
) {
    let map = dir_sizes.lock().unwrap();
    let mut dirs: Vec<SizeEntry> = map
        .iter()
        .map(|(p, &s)| SizeEntry {
            path: p.to_string_lossy().to_string(),
            size: s,
        })
        .collect();
    drop(map);
    dirs.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    dirs.truncate(top_n);
    state.lists.lock().unwrap().top_dirs = dirs;
}

// ─── Fallback: jwalk-based scanner for non-macOS ─────────────────────────────

#[cfg(not(target_os = "macos"))]
fn scan(
    state: Arc<ScanState>,
    path: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
) {
    use jwalk::WalkDir;

    let mut dir_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut batch: Vec<SizeEntry> = Vec::with_capacity(1024);
    let mut batch_bytes: u64 = 0;
    let mut batch_files: u64 = 0;
    let mut batch_dirs: u64 = 0;
    let mut batch_errors: u64 = 0;
    let mut last_path = String::new();
    let mut local_file_count: u64 = 0;
    let mut last_dir_update: u64 = 0;
    let mut min_top_size: u64 = 0;

    let walker = WalkDir::new(&path)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false);

    for entry in walker {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                batch_errors += 1;
                continue;
            }
        };

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                batch_errors += 1;
                continue;
            }
        };

        if metadata.is_file() {
            #[cfg(unix)]
            let size = {
                use std::os::unix::fs::MetadataExt;
                metadata.blocks() * 512
            };
            #[cfg(not(unix))]
            let size = metadata.len();
            batch_bytes += size;
            batch_files += 1;
            local_file_count += 1;

            let entry_path = entry.path();

            if !files_only {
                if let Some(parent) = entry_path.parent() {
                    let mut dir = parent.to_path_buf();
                    loop {
                        *dir_sizes.entry(dir.clone()).or_insert(0) += size;
                        if !dir.pop() || !dir.starts_with(&path) {
                            break;
                        }
                    }
                }
            }

            if size > min_top_size || batch_files <= top_n as u64 {
                let path_str = entry_path.to_string_lossy().to_string();
                last_path.clone_from(&path_str);
                batch.push(SizeEntry {
                    path: path_str,
                    size,
                });
            } else {
                last_path = entry_path.to_string_lossy().to_string();
            }
        } else if metadata.is_dir() {
            batch_dirs += 1;
        }

        if batch_files >= 1024 || batch_dirs >= 1024 {
            state
                .total_bytes
                .fetch_add(batch_bytes, Ordering::Relaxed);
            state
                .file_count
                .fetch_add(batch_files, Ordering::Relaxed);
            state.dir_count.fetch_add(batch_dirs, Ordering::Relaxed);
            if batch_errors > 0 {
                state
                    .error_count
                    .fetch_add(batch_errors, Ordering::Relaxed);
            }

            if !batch.is_empty() {
                let mut lists = state.lists.lock().unwrap();
                lists.current_path = shorten_path(&last_path);
                for entry in batch.drain(..) {
                    insert_top_n(&mut lists.top_files, entry, top_n);
                }
                min_top_size = lists.top_files.last().map(|e| e.size).unwrap_or(0);
            }

            batch_bytes = 0;
            batch_files = 0;
            batch_dirs = 0;
            batch_errors = 0;

            if !files_only && local_file_count - last_dir_update >= 10_000 {
                last_dir_update = local_file_count;
                let mut dirs: Vec<SizeEntry> = dir_sizes
                    .iter()
                    .map(|(p, &s)| SizeEntry {
                        path: p.to_string_lossy().to_string(),
                        size: s,
                    })
                    .collect();
                dirs.sort_unstable_by(|a, b| b.size.cmp(&a.size));
                dirs.truncate(top_n);
                state.lists.lock().unwrap().top_dirs = dirs;
            }
        }
    }

    state
        .total_bytes
        .fetch_add(batch_bytes, Ordering::Relaxed);
    state
        .file_count
        .fetch_add(batch_files, Ordering::Relaxed);
    state.dir_count.fetch_add(batch_dirs, Ordering::Relaxed);
    state
        .error_count
        .fetch_add(batch_errors, Ordering::Relaxed);

    if !batch.is_empty() {
        let mut lists = state.lists.lock().unwrap();
        for entry in batch.drain(..) {
            insert_top_n(&mut lists.top_files, entry, top_n);
        }
    }

    if !files_only {
        let mut dirs: Vec<SizeEntry> = dir_sizes
            .iter()
            .map(|(p, &s)| SizeEntry {
                path: p.to_string_lossy().to_string(),
                size: s,
            })
            .collect();
        dirs.sort_unstable_by(|a, b| b.size.cmp(&a.size));
        dirs.truncate(top_n);
        state.lists.lock().unwrap().top_dirs = dirs;
    }

    state.done.store(true, Ordering::Relaxed);
}

// ─── TUI ──────────────────────────────────────────────────────────────────────

enum ActiveTable {
    Files,
    Dirs,
}

struct App {
    active: ActiveTable,
    files_state: TableState,
    dirs_state: TableState,
    files_only: bool,
}

impl App {
    fn new(files_only: bool) -> Self {
        let mut files_state = TableState::default();
        files_state.select(Some(0));
        Self {
            active: ActiveTable::Files,
            files_state,
            dirs_state: TableState::default(),
            files_only,
        }
    }

    fn toggle_table(&mut self) {
        if self.files_only {
            return;
        }
        match self.active {
            ActiveTable::Files => {
                self.active = ActiveTable::Dirs;
                if self.dirs_state.selected().is_none() {
                    self.dirs_state.select(Some(0));
                }
            }
            ActiveTable::Dirs => {
                self.active = ActiveTable::Files;
            }
        }
    }

    fn move_up(&mut self) {
        let st = match self.active {
            ActiveTable::Files => &mut self.files_state,
            ActiveTable::Dirs => &mut self.dirs_state,
        };
        let i = st.selected().unwrap_or(0);
        st.select(Some(i.saturating_sub(1)));
    }

    fn move_down(&mut self, max: usize) {
        let st = match self.active {
            ActiveTable::Files => &mut self.files_state,
            ActiveTable::Dirs => &mut self.dirs_state,
        };
        let i = st.selected().unwrap_or(0);
        if i + 1 < max {
            st.select(Some(i + 1));
        }
    }
}

fn truncate_path(path: &str, max_width: usize) -> String {
    let shortened = shorten_path(path);
    if shortened.len() <= max_width {
        return shortened;
    }
    if max_width <= 3 {
        return "...".to_string();
    }
    format!("...{}", &shortened[shortened.len() - (max_width - 3)..])
}

fn render_size_table<'a>(
    entries: &[SizeEntry],
    title: &'a str,
    is_active: bool,
    min_size: u64,
    area_width: u16,
) -> (Table<'a>, usize) {
    let border_style = if is_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let path_width = (area_width as usize).saturating_sub(23);

    let rows: Vec<Row> = entries
        .iter()
        .filter(|e| e.size >= min_size)
        .enumerate()
        .map(|(i, entry)| {
            let rank = format!("{:>3}", i + 1);
            let size_str = format_size(entry.size, BINARY);
            let path = truncate_path(&entry.path, path_width);
            Row::new(vec![
                Cell::from(rank).style(Style::default().fg(Color::DarkGray)),
                Cell::from(format!("{:>10}", size_str))
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Cell::from(path),
            ])
        })
        .collect();

    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Min(10),
        ],
    )
    .block(
        Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_style(border_style),
    )
    .row_highlight_style(
        Style::default()
            .bg(if is_active {
                Color::DarkGray
            } else {
                Color::Reset
            })
            .add_modifier(Modifier::BOLD),
    );

    (table, count)
}

struct FrameData {
    top_files: Vec<SizeEntry>,
    top_dirs: Vec<SizeEntry>,
    total_bytes: u64,
    file_count: u64,
    dir_count: u64,
    error_count: u64,
    done: bool,
}

fn snapshot(state: &ScanState) -> FrameData {
    let lists = state.lists.lock().unwrap();
    FrameData {
        top_files: lists.top_files.clone(),
        top_dirs: lists.top_dirs.clone(),
        total_bytes: state.total_bytes.load(Ordering::Relaxed),
        file_count: state.file_count.load(Ordering::Relaxed),
        dir_count: state.dir_count.load(Ordering::Relaxed),
        error_count: state.error_count.load(Ordering::Relaxed),
        done: state.done.load(Ordering::Relaxed),
    }
}

fn draw_ui(
    f: &mut ratatui::Frame,
    data: &FrameData,
    app: &mut App,
    elapsed: Duration,
    min_size: u64,
    scan_path: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(f.area());

    let status_icon = if data.done { " ✓" } else { " ◉" };
    let status_label = if data.done { "done" } else { "scanning" };
    let header_text = format!(
        " {} {} | {} files | {} dirs | {} total | {} errors | {}s",
        status_label,
        shorten_path(scan_path),
        data.file_count,
        data.dir_count,
        format_size(data.total_bytes, BINARY),
        data.error_count,
        elapsed.as_secs(),
    );
    let status_color = if data.done { Color::Green } else { Color::Cyan };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            status_icon,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(header_text),
    ]))
    .block(
        Block::default()
            .title(" fdu ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(status_color)),
    );
    f.render_widget(header, chunks[0]);

    if app.files_only {
        let (table, count) = render_size_table(
            &data.top_files,
            " Largest Files ",
            true,
            min_size,
            chunks[1].width,
        );
        f.render_stateful_widget(table, chunks[1], &mut app.files_state);
        if let Some(sel) = app.files_state.selected() {
            if sel >= count && count > 0 {
                app.files_state.select(Some(count - 1));
            }
        }
    } else {
        let table_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);

        let files_active = matches!(app.active, ActiveTable::Files);

        let (ft, fc) = render_size_table(
            &data.top_files,
            " Largest Files ",
            files_active,
            min_size,
            table_chunks[0].width,
        );
        f.render_stateful_widget(ft, table_chunks[0], &mut app.files_state);
        if let Some(sel) = app.files_state.selected() {
            if sel >= fc && fc > 0 {
                app.files_state.select(Some(fc - 1));
            }
        }

        let (dt, dc) = render_size_table(
            &data.top_dirs,
            " Largest Directories ",
            !files_active,
            min_size,
            table_chunks[1].width,
        );
        f.render_stateful_widget(dt, table_chunks[1], &mut app.dirs_state);
        if let Some(sel) = app.dirs_state.selected() {
            if sel >= dc && dc > 0 {
                app.dirs_state.select(Some(dc - 1));
            }
        }
    }

    let footer_text = if app.files_only {
        " q: quit | ↑↓/jk: navigate"
    } else {
        " q: quit | tab: switch table | ↑↓/jk: navigate"
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

// ─── main ─────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let path = cli.path.canonicalize().unwrap_or_else(|_| {
        eprintln!("Error: cannot access path {:?}", cli.path);
        std::process::exit(1);
    });
    let scan_path = path.to_string_lossy().to_string();

    let min_size = match &cli.min_size {
        Some(s) => parse_size(s).unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }),
        None => 0,
    };

    let state = Arc::new(ScanState::new());
    let stop = Arc::new(AtomicBool::new(false));

    let scan_state = Arc::clone(&state);
    let scan_stop = Arc::clone(&stop);
    let scan_path_clone = path.clone();
    let top_n = cli.top;
    let files_only = cli.files_only;
    let scanner = thread::spawn(move || {
        scan(scan_state, scan_path_clone, top_n, files_only, scan_stop);
    });

    if cli.no_tui {
        scanner.join().unwrap();
        let data = snapshot(&state);
        println!(
            "Scanned {} files, {} dirs, {} total\n",
            data.file_count,
            data.dir_count,
            format_size(data.total_bytes, BINARY),
        );
        println!("Largest Files:");
        for (i, entry) in data
            .top_files
            .iter()
            .filter(|e| e.size >= min_size)
            .enumerate()
        {
            println!(
                "  {:>3}. {:>10}  {}",
                i + 1,
                format_size(entry.size, BINARY),
                shorten_path(&entry.path)
            );
        }
        if !files_only {
            println!("\nLargest Directories:");
            for (i, entry) in data
                .top_dirs
                .iter()
                .filter(|e| e.size >= min_size)
                .enumerate()
            {
                println!(
                    "  {:>3}. {:>10}  {}",
                    i + 1,
                    format_size(entry.size, BINARY),
                    shorten_path(&entry.path)
                );
            }
        }
    } else {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut app = App::new(files_only);
        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            let data = snapshot(&state);
            let files_len = data.top_files.len();
            let dirs_len = data.top_dirs.len();
            terminal.draw(|f| {
                draw_ui(f, &data, &mut app, elapsed, min_size, &scan_path);
            })?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                            KeyCode::Tab => app.toggle_table(),
                            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                            KeyCode::Down | KeyCode::Char('j') => {
                                let max = match app.active {
                                    ActiveTable::Files => files_len,
                                    ActiveTable::Dirs => dirs_len,
                                };
                                app.move_down(max);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        let _ = scanner.join();

        println!(
            "\nScanned {} files, {} dirs, {} total",
            state.file_count.load(Ordering::Relaxed),
            state.dir_count.load(Ordering::Relaxed),
            format_size(state.total_bytes.load(Ordering::Relaxed), BINARY),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_size tests ──────────────────────────────────────────────────

    #[test]
    fn parse_size_megabytes() {
        assert_eq!(parse_size("100MB").unwrap(), 100 * (1u64 << 20));
    }

    #[test]
    fn parse_size_gigabytes() {
        assert_eq!(parse_size("1GB").unwrap(), 1u64 << 30);
    }

    #[test]
    fn parse_size_kilobytes() {
        assert_eq!(parse_size("500KB").unwrap(), 500 * (1u64 << 10));
    }

    #[test]
    fn parse_size_terabytes() {
        assert_eq!(parse_size("2TB").unwrap(), 2 * (1u64 << 40));
    }

    #[test]
    fn parse_size_bytes_suffix() {
        assert_eq!(parse_size("1024B").unwrap(), 1024);
    }

    #[test]
    fn parse_size_no_suffix() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_size_invalid() {
        assert!(parse_size("abc").is_err());
    }

    // ── insert_top_n tests ────────────────────────────────────────────────

    #[test]
    fn insert_top_n_into_empty_list() {
        let mut list: Vec<SizeEntry> = Vec::new();
        insert_top_n(
            &mut list,
            SizeEntry {
                path: "a".into(),
                size: 100,
            },
            3,
        );
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].size, 100);
    }

    #[test]
    fn insert_top_n_maintains_descending_order() {
        let mut list: Vec<SizeEntry> = Vec::new();
        insert_top_n(
            &mut list,
            SizeEntry {
                path: "a".into(),
                size: 50,
            },
            5,
        );
        insert_top_n(
            &mut list,
            SizeEntry {
                path: "b".into(),
                size: 200,
            },
            5,
        );
        insert_top_n(
            &mut list,
            SizeEntry {
                path: "c".into(),
                size: 100,
            },
            5,
        );
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].size, 200);
        assert_eq!(list[1].size, 100);
        assert_eq!(list[2].size, 50);
    }

    #[test]
    fn insert_top_n_truncates_at_n() {
        let mut list: Vec<SizeEntry> = Vec::new();
        for size in [10, 20, 30, 40, 50] {
            insert_top_n(
                &mut list,
                SizeEntry {
                    path: format!("f{}", size),
                    size,
                },
                3,
            );
        }
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].size, 50);
        assert_eq!(list[1].size, 40);
        assert_eq!(list[2].size, 30);
    }

    // ── shorten_path tests ────────────────────────────────────────────────

    #[test]
    fn shorten_path_under_home() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let input = format!("{}/Documents/file.txt", home);
        assert_eq!(shorten_path(&input), "~/Documents/file.txt");
    }

    #[test]
    fn shorten_path_not_under_home() {
        let path = "/tmp/some/other/path";
        assert_eq!(shorten_path(path), path);
    }
}
