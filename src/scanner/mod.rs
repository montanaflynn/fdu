use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use crate::{ScanState, SizeEntry};

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

/// Shared parallel walker logic: accumulate dir sizes for ancestors and periodically flush.
pub(crate) fn flush_dir_sizes(
    dir: &PathBuf,
    local_bytes: u64,
    state: &ScanState,
    dir_sizes: &Mutex<HashMap<PathBuf, u64>>,
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
        flush_top_dirs(state, dir_sizes, top_n);
    }
}
