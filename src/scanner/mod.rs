use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use crate::{insert_top_n, ScanState, SizeEntry};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod fallback;

pub fn scan(
    state: Arc<ScanState>,
    root: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
) {
    #[cfg(target_os = "macos")]
    macos::scan(state, root, top_n, files_only, stop);

    #[cfg(target_os = "linux")]
    linux::scan(state, root, top_n, files_only, stop);

    #[cfg(target_os = "windows")]
    windows::scan(state, root, top_n, files_only, stop);

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    fallback::scan(state, root, top_n, files_only, stop);
}

pub(crate) fn flush_top_dirs(
    state: &ScanState,
    dir_sizes: &Mutex<HashMap<PathBuf, u64>>,
    per_dir_files: &Mutex<HashMap<String, Vec<SizeEntry>>>,
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

    // For each top-N dir, collect top-N files from it AND all its subdirectories
    let pdf = per_dir_files.lock().unwrap();
    let mut dir_files: HashMap<String, Vec<SizeEntry>> = HashMap::new();
    for dir in &dirs {
        let prefix = format!("{}/", dir.path.trim_end_matches('/'));
        let mut merged: Vec<SizeEntry> = Vec::new();
        for (dir_key, files) in pdf.iter() {
            if *dir_key == dir.path || dir_key.starts_with(&prefix) {
                for f in files {
                    insert_top_n(&mut merged, f.clone(), top_n);
                }
            }
        }
        if !merged.is_empty() {
            dir_files.insert(dir.path.clone(), merged);
        }
    }
    drop(pdf);

    let mut lists = state.lists.lock().unwrap();
    lists.top_dirs = dirs;
    lists.dir_files = dir_files;
}

/// Insert a file into the per-directory top-N file list.
pub(crate) fn insert_dir_file(
    per_dir_files: &Mutex<HashMap<String, Vec<SizeEntry>>>,
    dir_path: &str,
    entry: SizeEntry,
    top_n: usize,
) {
    let mut map = per_dir_files.lock().unwrap();
    let list = map.entry(dir_path.to_string()).or_default();
    insert_top_n(list, entry, top_n);
}

/// Shared parallel walker logic: accumulate dir sizes for ancestors and periodically flush.
pub(crate) fn flush_dir_sizes(
    dir: &PathBuf,
    local_bytes: u64,
    state: &ScanState,
    dir_sizes: &Mutex<HashMap<PathBuf, u64>>,
    per_dir_files: &Mutex<HashMap<String, Vec<SizeEntry>>>,
    dirs_processed: &std::sync::atomic::AtomicU64,
    root: &PathBuf,
    top_n: usize,
) {
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
        flush_top_dirs(state, dir_sizes, per_dir_files, top_n);
    }
}
