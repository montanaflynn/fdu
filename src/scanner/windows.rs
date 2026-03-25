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

/// Returns true if the filename is "." or ".."
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
    min_top_size: &'s AtomicU64,
    dirs_processed: &'s AtomicU64,
    root: &'s PathBuf,
    top_n: usize,
    files_only: bool,
    stop: &'s AtomicBool,
) {
    if stop.load(Ordering::Relaxed) {
        return;
    }

    // Build the search pattern: dir\*  (null-terminated UTF-16)
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

    // Process entries starting with the first one already in find_data
    let mut has_entry = true;
    while has_entry {
        let attrs = find_data.dwFileAttributes;

        // Skip symlinks and junctions (reparse points)
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
            // Skip . and ..
            if !is_dot_dir(&find_data.cFileName) {
                if attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                    // Directory
                    let name_len = find_data
                        .cFileName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(find_data.cFileName.len());
                    let name = String::from_utf16_lossy(&find_data.cFileName[..name_len]);
                    subdirs.push(dir.join(&name));
                } else {
                    // File — size is inline in the find data
                    let size = (find_data.nFileSizeHigh as u64) << 32
                        | find_data.nFileSizeLow as u64;
                    local_bytes += size;
                    local_file_count += 1;

                    let current_min = min_top_size.load(Ordering::Relaxed);
                    if size > current_min
                        || state.file_count.load(Ordering::Relaxed) + local_file_count
                            <= top_n as u64
                    {
                        let name_len = find_data
                            .cFileName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(find_data.cFileName.len());
                        let name = String::from_utf16_lossy(&find_data.cFileName[..name_len]);
                        let path_str = dir.join(&name).to_string_lossy().to_string();
                        top_candidates.push(SizeEntry {
                            path: path_str,
                            size,
                        });
                    }
                }
            }
        }

        // Advance to next entry
        has_entry = unsafe { FindNextFileW(handle, &mut find_data).is_ok() };
    }

    // Always close the find handle
    unsafe {
        let _ = FindClose(handle);
    }

    state.total_bytes.fetch_add(local_bytes, Ordering::Relaxed);
    state
        .file_count
        .fetch_add(local_file_count, Ordering::Relaxed);

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
        flush_dir_sizes(
            &dir,
            local_bytes,
            state,
            dir_sizes,
            dirs_processed,
            root,
            top_n,
        );
    }

    for subdir in subdirs {
        scope.spawn(move |s| {
            walk_dir(
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
