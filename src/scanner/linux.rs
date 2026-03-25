use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use crate::{insert_top_n, shorten_path, ScanState, SizeEntry};
use super::{flush_dir_sizes, flush_top_dirs};

pub fn scan(
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
        walk_dir(
            scope, root.clone(), &state, &dir_sizes, &min_top_size,
            &dirs_processed, &root, top_n, files_only, &stop,
        );
    });

    if !files_only {
        flush_top_dirs(&state, &dir_sizes, top_n);
    }

    state.done.store(true, Ordering::Relaxed);
}

fn walk_dir<'s>(
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
    use std::os::unix::fs::MetadataExt;

    if stop.load(Ordering::Relaxed) {
        return;
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => {
            state.error_count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    state.dir_count.fetch_add(1, Ordering::Relaxed);

    let mut local_bytes: u64 = 0;
    let mut local_file_count: u64 = 0;
    let mut top_candidates: Vec<SizeEntry> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                state.error_count.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                state.error_count.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        if file_type.is_dir() {
            subdirs.push(entry.path());
        } else if file_type.is_file() {
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    state.error_count.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            let size = metadata.blocks() * 512;
            local_bytes += size;
            local_file_count += 1;

            let current_min = min_top_size.load(Ordering::Relaxed);
            if size > current_min
                || state.file_count.load(Ordering::Relaxed) + local_file_count <= top_n as u64
            {
                let path_str = entry.path().to_string_lossy().to_string();
                top_candidates.push(SizeEntry { path: path_str, size });
            }
        }
    }

    state.total_bytes.fetch_add(local_bytes, Ordering::Relaxed);
    state.file_count.fetch_add(local_file_count, Ordering::Relaxed);

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

    if !files_only && local_bytes > 0 {
        flush_dir_sizes(&dir, local_bytes, state, dir_sizes, dirs_processed, root, top_n);
    }

    for subdir in subdirs {
        scope.spawn(move |s| {
            walk_dir(s, subdir, state, dir_sizes, min_top_size, dirs_processed, root, top_n, files_only, stop);
        });
    }
}
