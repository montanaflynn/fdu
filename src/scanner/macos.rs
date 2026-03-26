use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use crate::{insert_top_n, shorten_path, ScanState, SizeEntry};
use super::{flush_dir_sizes, flush_top_dirs, insert_dir_file};

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

                let ret_common = read_u32(&buf, offset);
                offset += 4;
                offset += 4; // volattr
                offset += 4; // dirattr
                let ret_file = read_u32(&buf, offset);
                offset += 4;
                offset += 4; // forkattr

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

pub fn scan(
    state: Arc<ScanState>,
    root: PathBuf,
    top_n: usize,
    files_only: bool,
    stop: Arc<AtomicBool>,
) {
    let dir_sizes: Mutex<HashMap<PathBuf, u64>> = Mutex::new(HashMap::new());
    let per_dir_files: Mutex<HashMap<String, Vec<SizeEntry>>> = Mutex::new(HashMap::new());
    let min_top_size = AtomicU64::new(0);
    let dirs_processed = AtomicU64::new(0);

    rayon::scope(|scope| {
        walk_dir(
            scope,
            root.clone(),
            &state,
            &dir_sizes,
            &per_dir_files,
            &min_top_size,
            &dirs_processed,
            &root,
            top_n,
            files_only,
            &stop,
        );
    });

    if !files_only {
        flush_top_dirs(&state, &dir_sizes, &per_dir_files, top_n);
    }

    state.done.store(true, Ordering::Relaxed);
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
) {
    if stop.load(Ordering::Relaxed) {
        return;
    }

    let entries = {
        let dir_file = match std::fs::File::open(&dir) {
            Ok(f) => f,
            Err(_) => {
                state.error_count.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        bulk::read_dir_bulk(dir_file.as_raw_fd())
    };

    state.dir_count.fetch_add(1, Ordering::Relaxed);

    let mut local_bytes: u64 = 0;
    let mut local_file_count: u64 = 0;
    let mut top_candidates: Vec<SizeEntry> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();

    let dir_str = dir.to_string_lossy().to_string();

    for entry in &entries {
        if entry.obj_type == bulk::VDIR {
            subdirs.push(dir.join(&entry.name));
        } else if entry.obj_type == bulk::VREG {
            let size = entry.size;
            local_bytes += size;
            local_file_count += 1;

            let path_str = dir.join(&entry.name).to_string_lossy().to_string();
            let file_entry = SizeEntry { path: path_str, size };

            // Track per-directory top-N files
            if !files_only {
                insert_dir_file(per_dir_files, &dir_str, file_entry.clone(), top_n);
            }

            // Track global top-N files
            let current_min = min_top_size.load(Ordering::Relaxed);
            if size > current_min
                || state.file_count.load(Ordering::Relaxed) + local_file_count
                    <= top_n as u64
            {
                top_candidates.push(file_entry);
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
        flush_dir_sizes(&dir, local_bytes, state, dir_sizes, per_dir_files, dirs_processed, root, top_n);
    }

    for subdir in subdirs {
        scope.spawn(move |s| {
            walk_dir(s, subdir, state, dir_sizes, per_dir_files, min_top_size, dirs_processed, root, top_n, files_only, stop);
        });
    }
}
