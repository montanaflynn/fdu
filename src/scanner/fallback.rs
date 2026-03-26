use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use jwalk::WalkDir;

use crate::{glob_match, insert_top_n, shorten_path, ScanOptions, ScanState, SizeEntry};

pub fn scan(
    state: Arc<ScanState>,
    path: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
    options: ScanOptions,
) {
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

    let mut walker = WalkDir::new(&path)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false);
    if let Some(md) = options.max_depth {
        walker = walker.max_depth(md + 1);
    }

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

        // Check exclude
        if !options.exclude.is_empty() {
            let should_exclude = entry
                .path()
                .strip_prefix(&path)
                .unwrap_or(&entry.path())
                .components()
                .any(|c| {
                    if let std::path::Component::Normal(name) = c {
                        name.to_str()
                            .map_or(false, |n| options.exclude.iter().any(|e| glob_match(e, n)))
                    } else {
                        false
                    }
                });
            if should_exclude {
                continue;
            }
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                batch_errors += 1;
                continue;
            }
        };

        // Check one-file-system
        #[cfg(unix)]
        if options.one_file_system && options.root_dev != 0 {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != options.root_dev {
                continue;
            }
        }

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
            state.total_bytes.fetch_add(batch_bytes, Ordering::Relaxed);
            state.file_count.fetch_add(batch_files, Ordering::Relaxed);
            state.dir_count.fetch_add(batch_dirs, Ordering::Relaxed);
            if batch_errors > 0 {
                state.error_count.fetch_add(batch_errors, Ordering::Relaxed);
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

    state.total_bytes.fetch_add(batch_bytes, Ordering::Relaxed);
    state.file_count.fetch_add(batch_files, Ordering::Relaxed);
    state.dir_count.fetch_add(batch_dirs, Ordering::Relaxed);
    state.error_count.fetch_add(batch_errors, Ordering::Relaxed);

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
