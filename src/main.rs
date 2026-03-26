mod scanner;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use humansize::{format_size, BINARY};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
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

    /// Maximum directory depth to recurse
    #[arg(long = "max-depth")]
    max_depth: Option<usize>,

    /// Exclude entries matching these names (can be repeated)
    #[arg(long = "exclude")]
    exclude: Vec<String>,

    /// Stay on the same filesystem (don't cross mount points)
    #[arg(long = "one-file-system")]
    one_file_system: bool,
}

#[derive(Clone)]
pub(crate) struct SizeEntry {
    pub path: String,
    pub size: u64,
}

pub(crate) struct ScanLists {
    pub top_files: Vec<SizeEntry>,
    pub top_dirs: Vec<SizeEntry>,
    pub dir_files: HashMap<String, Vec<SizeEntry>>,
    pub current_path: String,
}

pub(crate) struct ScanState {
    pub lists: Mutex<ScanLists>,
    pub total_bytes: AtomicU64,
    pub file_count: AtomicU64,
    pub dir_count: AtomicU64,
    pub error_count: AtomicU64,
    pub done: AtomicBool,
}

impl ScanState {
    fn new() -> Self {
        Self {
            lists: Mutex::new(ScanLists {
                top_files: Vec::new(),
                top_dirs: Vec::new(),
                dir_files: HashMap::new(),
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

pub(crate) struct ScanOptions {
    pub max_depth: Option<usize>,
    pub exclude: Vec<String>,
    pub one_file_system: bool,
    pub root_dev: u64,
}

pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
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

pub(crate) fn shorten_path(path: &str) -> String {
    if let Some(home) = home_dir() {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

pub(crate) fn insert_top_n(list: &mut Vec<SizeEntry>, entry: SizeEntry, n: usize) {
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
    filter_dir: Option<String>,
    confirm_delete: Option<(String, bool)>, // (path, is_dir)
    delete_error: Option<String>,
}

impl App {
    fn new(files_only: bool) -> Self {
        let mut dirs_state = TableState::default();
        dirs_state.select(Some(0));
        Self {
            active: if files_only { ActiveTable::Files } else { ActiveTable::Dirs },
            files_state: TableState::default(),
            dirs_state,
            files_only,
            filter_dir: None,
            confirm_delete: None,
            delete_error: None,
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
                if self.files_state.selected().is_none() {
                    self.files_state.select(Some(0));
                }
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

    fn selected_path(&self, data: &FrameData) -> Option<(String, bool)> {
        match self.active {
            ActiveTable::Files => {
                let files = if let Some(ref filter) = self.filter_dir {
                    data.dir_files.get(filter).cloned().unwrap_or_default()
                } else {
                    data.top_files.clone()
                };
                self.files_state
                    .selected()
                    .and_then(|i| files.get(i).map(|e| (e.path.clone(), false)))
            }
            ActiveTable::Dirs => self
                .dirs_state
                .selected()
                .and_then(|i| data.top_dirs.get(i).map(|e| (e.path.clone(), true))),
        }
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
    pinned_path: Option<&str>,
    selected: Option<usize>,
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
            let is_pinned = pinned_path.is_some_and(|p| p == entry.path);
            let is_highlighted = is_active && selected == Some(i);
            let rank_color = if is_pinned {
                Color::Cyan
            } else if is_highlighted {
                Color::Gray
            } else {
                Color::DarkGray
            };
            let row = Row::new(vec![
                Cell::from(rank).style(Style::default().fg(rank_color)),
                Cell::from(format!("{:>10}", size_str))
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Cell::from(path).style(if is_pinned {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
            ]);
            if is_pinned {
                row.style(Style::default().bg(Color::Indexed(236)))
            } else {
                row
            }
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
    .row_highlight_style(if is_active {
        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    });

    (table, count)
}

struct FrameData {
    top_files: Vec<SizeEntry>,
    top_dirs: Vec<SizeEntry>,
    dir_files: HashMap<String, Vec<SizeEntry>>,
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
        dir_files: lists.dir_files.clone(),
        total_bytes: state.total_bytes.load(Ordering::Relaxed),
        file_count: state.file_count.load(Ordering::Relaxed),
        dir_count: state.dir_count.load(Ordering::Relaxed),
        error_count: state.error_count.load(Ordering::Relaxed),
        done: state.done.load(Ordering::Relaxed),
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
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
            None,
            app.files_state.selected(),
        );
        if count > 0 && app.files_state.selected().is_none() {
            app.files_state.select(Some(0));
        } else if let Some(sel) = app.files_state.selected() {
            if sel >= count && count > 0 {
                app.files_state.select(Some(count - 1));
            }
        }
        f.render_stateful_widget(table, chunks[1], &mut app.files_state);
    } else {
        let table_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);

        let dirs_active = matches!(app.active, ActiveTable::Dirs);

        // Dirs on top
        let (dt, dc) = render_size_table(
            &data.top_dirs,
            " Largest Directories ",
            dirs_active,
            min_size,
            table_chunks[0].width,
            app.filter_dir.as_deref(),
            app.dirs_state.selected(),
        );
        if dc > 0 && app.dirs_state.selected().is_none() {
            app.dirs_state.select(Some(0));
        } else if let Some(sel) = app.dirs_state.selected() {
            if sel >= dc && dc > 0 {
                app.dirs_state.select(Some(dc - 1));
            }
        }
        f.render_stateful_widget(dt, table_chunks[0], &mut app.dirs_state);

        // Files on bottom — show per-dir files if filtered, else global top
        let (files_to_show, files_title) = if let Some(ref filter) = app.filter_dir {
            (
                data.dir_files.get(filter).cloned().unwrap_or_default(),
                format!(" Files in {} ", shorten_path(filter)),
            )
        } else {
            (data.top_files.clone(), " Largest Files ".to_string())
        };

        let (ft, fc) = render_size_table(
            &files_to_show,
            &files_title,
            !dirs_active,
            min_size,
            table_chunks[1].width,
            None,
            app.files_state.selected(),
        );
        if fc > 0 && app.files_state.selected().is_none() {
            app.files_state.select(Some(0));
        } else if let Some(sel) = app.files_state.selected() {
            if sel >= fc && fc > 0 {
                app.files_state.select(Some(fc - 1));
            }
        }
        f.render_stateful_widget(ft, table_chunks[1], &mut app.files_state);
    }

    let footer_text = if app.files_only {
        " q: quit | d: delete | ↑↓/jk: navigate"
    } else if app.filter_dir.is_some() {
        " esc/bksp: clear filter | d: delete | q: quit | tab: table | ↑↓/jk: navigate"
    } else {
        " enter: filter by dir | d: delete | q: quit | tab: table | ↑↓/jk: navigate"
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);

    // Delete confirmation modal
    if let Some((ref delete_path, _is_dir)) = app.confirm_delete {
        let modal_width = 50u16.min(f.area().width.saturating_sub(4));
        let modal_height = 7u16;
        let area = centered_rect(modal_width, modal_height, f.area());

        f.render_widget(Clear, area);

        let max_path_len = (modal_width as usize).saturating_sub(14);
        let display_path = truncate_path(delete_path, max_path_len);

        let text = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("Delete "),
                Span::styled(
                    display_path,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("?"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Are you sure? (Y/n)",
                Style::default().fg(Color::White),
            )),
            Line::from(""),
        ];

        let modal = Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            );
        f.render_widget(modal, area);
    }

    // Delete error modal
    if let Some(ref error) = app.delete_error {
        let modal_width = 50u16.min(f.area().width.saturating_sub(4));
        let modal_height = 5u16;
        let area = centered_rect(modal_width, modal_height, f.area());

        f.render_widget(Clear, area);

        let text = vec![
            Line::from(""),
            Line::from(Span::styled(
                error.as_str(),
                Style::default().fg(Color::Red),
            )),
            Line::from(Span::styled(
                "Press any key to dismiss",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let modal = Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            );
        f.render_widget(modal, area);
    }
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
    let no_tui = cli.no_tui;

    #[cfg(unix)]
    let root_dev = if cli.one_file_system {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&path).map(|m| m.dev()).unwrap_or(0)
    } else {
        0
    };
    #[cfg(not(unix))]
    let root_dev: u64 = 0;

    let options = ScanOptions {
        max_depth: cli.max_depth,
        exclude: cli.exclude,
        one_file_system: cli.one_file_system,
        root_dev,
    };

    let scanner = thread::spawn(move || {
        scanner::scan(scan_state, scan_path_clone, top_n, files_only, scan_stop, options);
    });

    if no_tui {
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
        let mut final_elapsed = None;

        loop {
            let data = snapshot(&state);
            let elapsed = if data.done {
                *final_elapsed.get_or_insert(start.elapsed())
            } else {
                start.elapsed()
            };
            let files_len = if let Some(ref filter) = app.filter_dir {
                data.dir_files.get(filter).map(|v| v.len()).unwrap_or(0)
            } else {
                data.top_files.len()
            };
            let dirs_len = data.top_dirs.len();
            terminal.draw(|f| {
                draw_ui(f, &data, &mut app, elapsed, min_size, &scan_path);
            })?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if app.delete_error.is_some() {
                            // Any key dismisses the error modal
                            app.delete_error = None;
                        } else if app.confirm_delete.is_some() {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                    let (path, is_dir) =
                                        app.confirm_delete.take().unwrap();
                                    let result = if is_dir {
                                        std::fs::remove_dir_all(&path)
                                    } else {
                                        std::fs::remove_file(&path)
                                    };
                                    match result {
                                        Ok(()) => {
                                            let mut lists =
                                                state.lists.lock().unwrap();
                                            let prefix = format!("{}/", path);
                                            if is_dir {
                                                lists.top_dirs.retain(|e| {
                                                    e.path != path
                                                        && !e.path.starts_with(&prefix)
                                                });
                                                lists.top_files.retain(|e| {
                                                    !e.path.starts_with(&prefix)
                                                });
                                                lists.dir_files.retain(|k, _| {
                                                    k != &path
                                                        && !k.starts_with(&prefix)
                                                });
                                                if app.filter_dir.as_deref()
                                                    == Some(path.as_str())
                                                {
                                                    app.filter_dir = None;
                                                    app.active = ActiveTable::Dirs;
                                                }
                                            } else {
                                                lists
                                                    .top_files
                                                    .retain(|e| e.path != path);
                                                for files in
                                                    lists.dir_files.values_mut()
                                                {
                                                    files
                                                        .retain(|e| e.path != path);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            app.delete_error =
                                                Some(format!("Delete failed: {}", e));
                                        }
                                    }
                                }
                                KeyCode::Char('n')
                                | KeyCode::Char('N')
                                | KeyCode::Esc => {
                                    app.confirm_delete = None;
                                }
                                _ => {}
                            }
                        } else {
                            match key.code {
                                KeyCode::Char('q') => {
                                    stop.store(true, Ordering::Relaxed);
                                    break;
                                }
                                KeyCode::Esc => {
                                    if app.filter_dir.is_some() {
                                        app.filter_dir = None;
                                        app.active = ActiveTable::Dirs;
                                    } else {
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                                KeyCode::Backspace => {
                                    if app.filter_dir.is_some() {
                                        app.filter_dir = None;
                                        app.active = ActiveTable::Dirs;
                                    }
                                }
                                KeyCode::Enter => {
                                    if matches!(app.active, ActiveTable::Dirs) {
                                        if let Some(sel) = app.dirs_state.selected()
                                        {
                                            if sel < data.top_dirs.len() {
                                                app.filter_dir = Some(
                                                    data.top_dirs[sel].path.clone(),
                                                );
                                                app.active = ActiveTable::Files;
                                                app.files_state.select(Some(0));
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('d') => {
                                    if let Some(target) =
                                        app.selected_path(&data)
                                    {
                                        app.confirm_delete = Some(target);
                                    }
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
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return,
        };
        let input = format!("{}/Documents/file.txt", home);
        assert_eq!(shorten_path(&input), "~/Documents/file.txt");
    }

    #[test]
    fn shorten_path_not_under_home() {
        let path = "/tmp/some/other/path";
        assert_eq!(shorten_path(path), path);
    }

    // ── glob_match tests ─────────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("node_modules", "node_modules"));
        assert!(!glob_match("node_modules", "node_module"));
    }

    #[test]
    fn glob_star_pattern() {
        assert!(glob_match("*.log", "error.log"));
        assert!(glob_match("*.log", ".log"));
        assert!(!glob_match("*.log", "error.txt"));
        assert!(glob_match("build*", "build"));
        assert!(glob_match("build*", "build-output"));
        assert!(glob_match("*test*", "my_test_file"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
    }

    #[test]
    fn glob_combined() {
        assert!(glob_match("*.t?t", "file.txt"));
        assert!(!glob_match("*.t?t", "file.text"));
    }
}
