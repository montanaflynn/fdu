use std::{
    collections::HashMap,
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileExW, FindNextFileW, FindExInfoBasic, FindExSearchNameMatch,
    FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT,
};

use crate::{glob_match, insert_top_n, shorten_path, ScanOptions, ScanState, SizeEntry};
use super::{flush_dir_sizes, flush_top_dirs, insert_dir_file};

pub fn scan(
    state: Arc<ScanState>,
    root: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
    options: ScanOptions,
) {
    let dir_sizes: Mutex<HashMap<PathBuf, u64>> = Mutex::new(HashMap::new());
    let per_dir_files: Mutex<HashMap<String, Vec<SizeEntry>>> = Mutex::new(HashMap::new());
    let min_top_size = AtomicU64::new(0);
    let dirs_processed = AtomicU64::new(0);

    rayon::scope(|scope| {
        walk_dir(
            scope, root.clone(), &state, &dir_sizes, &per_dir_files, &min_top_size,
            &dirs_processed, &root, top_n, files_only, &stop, &options, 0,
        );
    });

    if !files_only {
        flush_top_dirs(&state, &dir_sizes, &per_dir_files, top_n);
    }

    state.done.store(true, Ordering::Relaxed);
}

fn is_dot_dir(name: &[u16]) -> bool {
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    (len == 1 && name[0] == b'.' as u16)
        || (len == 2 && name[0] == b'.' as u16 && name[1] == b'.' as u16)
}

fn walk_dir<'s>(
    scope: &rayon::Scope<'s>,
    dir: PathBuf,
    state: &'s ScanState,
    dir_sizes: &'s Mutex<HashMap<PathBuf, u64>>,
    per_dir_files: &'s Mutex<HashMap<String, Vec<SizeEntry>>>,
    min_top_size: &'s AtomicU64,
    dirs_processed: &'s AtomicU64,
    root: &'s PathBuf,
    top_n: usize,
    files_only: bool,
    stop: &'s AtomicBool,
    options: &'s ScanOptions,
    depth: usize,
) {
    if stop.load(Ordering::Relaxed) {
        return;
    }

    let pattern: Vec<u16> = dir
        .join("*")
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut find_data = WIN32_FIND_DATAW::default();

    let handle = unsafe {
        FindFirstFileExW(
            windows::core::PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut find_data as *mut _ as *mut _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };

    let handle = match handle {
        Ok(h) => h,
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
    let dir_str = dir.to_string_lossy().to_string();

    let mut has_entry = true;
    while has_entry {
        let attrs = find_data.dwFileAttributes;

        if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
            if !is_dot_dir(&find_data.cFileName) {
                let name_len = find_data.cFileName.iter().position(|&c| c == 0)
                    .unwrap_or(find_data.cFileName.len());
                let name = String::from_utf16_lossy(&find_data.cFileName[..name_len]);

                if !options.exclude.is_empty()
                    && options.exclude.iter().any(|e| glob_match(e, &name))
                {
                    has_entry = unsafe { FindNextFileW(handle, &mut find_data).is_ok() };
                    continue;
                }

                if attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                    subdirs.push(dir.join(&name));
                } else {
                    let size = (find_data.nFileSizeHigh as u64) << 32
                        | find_data.nFileSizeLow as u64;
                    local_bytes += size;
                    local_file_count += 1;

                    let path_str = dir.join(&name).to_string_lossy().to_string();
                    let file_entry = SizeEntry { path: path_str, size };

                    if !files_only {
                        insert_dir_file(per_dir_files, &dir_str, file_entry.clone(), top_n);
                    }

                    let current_min = min_top_size.load(Ordering::Relaxed);
                    if size > current_min
                        || state.file_count.load(Ordering::Relaxed) + local_file_count
                            <= top_n as u64
                    {
                        top_candidates.push(file_entry);
                    }
                }
            }
        }

        has_entry = unsafe { FindNextFileW(handle, &mut find_data).is_ok() };
    }

    unsafe {
        let _ = FindClose(handle);
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
        flush_dir_sizes(&dir, local_bytes, state, dir_sizes, per_dir_files, dirs_processed, root, top_n);
    }

    if options.max_depth.map_or(true, |md| depth < md) {
        for subdir in subdirs {
            scope.spawn(move |s| {
                walk_dir(s, subdir, state, dir_sizes, per_dir_files, min_top_size, dirs_processed, root, top_n, files_only, stop, options, depth + 1);
            });
        }
    }
}
