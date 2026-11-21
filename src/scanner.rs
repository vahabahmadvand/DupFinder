use crate::models::{
    DuplicateGroup, FileEntry, ScanConfig, ScanProgress, ScanResult, ScanStage, ScanSummary,
};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

const PARTIAL_CHUNK_SIZE: usize = 4 * 1024; // 4 KB head + 4 KB tail
const SMALL_FILE_THRESHOLD: u64 = 16 * 1024; // <= 16 KB: hash fully in Stage 3 (single open!)
const FULL_HASH_BUF_SIZE: usize = 256 * 1024; // 256 KB sequential read buffer

#[derive(Debug, Clone)]
struct WalkCandidate {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    volume_serial: u32,
    file_index: u64,
}

#[derive(Debug)]
struct PartialResult {
    candidate: WalkCandidate,
    identity: Option<FileIdentity>,
    hash: [u8; 32],
    is_full_hash: bool,
}

const SYSTEM_DIR_NAMES: &[&str] = &[
    "$recycle.bin",
    "system volume information",
    "$windows.~bt",
    "$windows.~ws",
    "recovery",
    "config.msi",
    "msocache",
    "perflogs",
    "winsxs",
];

const DEV_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".cargo",
    ".gradle",
    "obj",
    "bin",
];

fn should_skip_dir(name: &str, skip_system: bool, skip_dev: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if skip_system && SYSTEM_DIR_NAMES.iter().any(|&s| s == lower) {
        return true;
    }
    if skip_dev && DEV_DIR_NAMES.iter().any(|&s| s == lower) {
        return true;
    }
    false
}

/// Remove overlapping child roots so a directory isn't scanned twice.
fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical: Vec<PathBuf> = roots
        .iter()
        .filter(|p| p.exists())
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();

    // Sort shortest first so parent directories come before children
    canonical.sort_by_key(|p| p.as_os_str().len());

    let mut result: Vec<PathBuf> = Vec::with_capacity(canonical.len());
    for path in canonical {
        let is_subpath = result.iter().any(|existing| path.starts_with(existing));
        if !is_subpath {
            result.push(path);
        }
    }
    result
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn get_file_identity(file: &File) -> Option<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) };
    if ok != 0 {
        let info = unsafe { info.assume_init() };
        let file_index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
        Some(FileIdentity {
            volume_serial: info.dwVolumeSerialNumber,
            file_index,
        })
    } else {
        None
    }
}

#[cfg(not(windows))]
fn get_file_identity(_file: &File) -> Option<FileIdentity> {
    None
}

fn compute_partial_or_small_hash(
    candidate: WalkCandidate,
    buf: &mut [u8],
    progress: &ScanProgress,
) -> Option<PartialResult> {
    if progress.is_cancelled() {
        return None;
    }

    let mut file = match File::open(&candidate.path) {
        Ok(f) => f,
        Err(_) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };

    let identity = get_file_identity(&file);
    let size = candidate.size;

    if size <= SMALL_FILE_THRESHOLD {
        // Small file: read entire file in one go — avoids opening again in Stage 4!
        let mut hasher = blake3::Hasher::new();
        let mut total_read = 0u64;
        loop {
            match file.read(buf) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    total_read += n as u64;
                }
                Err(_) => {
                    progress.errors.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        }
        progress.bytes_hashed.fetch_add(total_read, Ordering::Relaxed);
        progress.candidates_done.fetch_add(1, Ordering::Relaxed);
        return Some(PartialResult {
            candidate,
            identity,
            hash: *hasher.finalize().as_bytes(),
            is_full_hash: true,
        });
    }

    // Larger file: hash first 4 KB + last 4 KB
    let mut hasher = blake3::Hasher::new();
    let head_buf = &mut buf[..PARTIAL_CHUNK_SIZE];
    let head_n = match read_up_to(&mut file, head_buf) {
        Ok(n) => n,
        Err(_) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    hasher.update(&head_buf[..head_n]);

    let mut total_read = head_n as u64;
    if size > PARTIAL_CHUNK_SIZE as u64 {
        let tail_offset = size.saturating_sub(PARTIAL_CHUNK_SIZE as u64);
        if file.seek(SeekFrom::Start(tail_offset)).is_err() {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let tail_n = match read_up_to(&mut file, head_buf) {
            Ok(n) => n,
            Err(_) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        hasher.update(&head_buf[..tail_n]);
        total_read += tail_n as u64;
    }

    progress.bytes_hashed.fetch_add(total_read, Ordering::Relaxed);
    progress.candidates_done.fetch_add(1, Ordering::Relaxed);

    Some(PartialResult {
        candidate,
        identity,
        hash: *hasher.finalize().as_bytes(),
        is_full_hash: false,
    })
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn compute_full_hash(
    candidate: WalkCandidate,
    buf: &mut [u8],
    progress: &ScanProgress,
) -> Option<(WalkCandidate, [u8; 32])> {
    if progress.is_cancelled() {
        return None;
    }

    let mut file = match File::open(&candidate.path) {
        Ok(f) => f,
        Err(_) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };

    let mut hasher = blake3::Hasher::new();
    let mut bytes_since_report = 0u64;

    loop {
        if progress.is_cancelled() {
            return None;
        }
        match file.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                bytes_since_report += n as u64;
                // Batch atomic updates every 4 MB to avoid cache-line contention across threads
                if bytes_since_report >= 4 * 1024 * 1024 {
                    progress
                        .bytes_hashed
                        .fetch_add(bytes_since_report, Ordering::Relaxed);
                    bytes_since_report = 0;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }

    if bytes_since_report > 0 {
        progress
            .bytes_hashed
            .fetch_add(bytes_since_report, Ordering::Relaxed);
    }
    progress.candidates_done.fetch_add(1, Ordering::Relaxed);

    Some((candidate, *hasher.finalize().as_bytes()))
}

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = path.to_str() {
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
    }
    path
}

pub fn run_scan(config: &ScanConfig, progress: Arc<ScanProgress>) -> ScanResult {
    let start = Instant::now();
    progress.set_stage(ScanStage::Walking);

    let roots = normalize_roots(&config.roots);
    let min_size = config.min_size;
    let skip_system = config.skip_system_dirs;
    let skip_dev = config.skip_dev_dirs;

    // Stage 1: Parallel directory walk -> group by exact file size
    let mut size_groups: FxHashMap<u64, Vec<WalkCandidate>> = FxHashMap::default();
    let mut total_files_seen = 0u64;

    for root in &roots {
        if progress.is_cancelled() {
            break;
        }

        let walk = jwalk::WalkDir::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(move |_depth, _path, _read_dir_state, children| {
                children.retain(|entry_res| {
                    let Ok(entry) = entry_res else {
                        return false;
                    };
                    if entry.file_type().is_symlink() {
                        return false;
                    }
                    if entry.file_type().is_dir() {
                        if let Some(name) = entry.file_name().to_str() {
                            if should_skip_dir(name, skip_system, skip_dev) {
                                return false;
                            }
                        }
                    }
                    true
                });
            });

        for entry_res in walk {
            if progress.is_cancelled() {
                break;
            }

            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => {
                    progress.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    progress.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            if is_reparse_point(&meta) {
                continue;
            }

            total_files_seen += 1;
            if total_files_seen & 0xFF == 0 {
                progress
                    .files_scanned
                    .store(total_files_seen, Ordering::Relaxed);
            }

            let size = meta.len();
            if size < min_size {
                continue;
            }

            let modified = meta.modified().ok();
            let path = strip_verbatim_prefix(entry.path());
            size_groups
                .entry(size)
                .or_default()
                .push(WalkCandidate {
                    path,
                    size,
                    modified,
                });
        }
    }

    progress
        .files_scanned
        .store(total_files_seen, Ordering::Relaxed);

    if progress.is_cancelled() {
        progress.set_stage(ScanStage::Cancelled);
        return ScanResult {
            groups: Vec::new(),
            summary: ScanSummary {
                total_files_seen,
                total_bytes_hashed: 0,
                skipped_hardlinks: 0,
                skipped_errors: progress.errors.load(Ordering::Relaxed),
                elapsed: start.elapsed(),
            },
            cancelled: true,
        };
    }

    // Stage 2: Filter out unique sizes (typically eliminates 80-95% of all files!)
    progress.set_stage(ScanStage::GroupingBySize);
    let size_candidates: Vec<WalkCandidate> = size_groups
        .into_values()
        .filter(|v| v.len() >= 2)
        .flatten()
        .collect();

    if size_candidates.is_empty() {
        progress.set_stage(ScanStage::Done);
        return ScanResult {
            groups: Vec::new(),
            summary: ScanSummary {
                total_files_seen,
                total_bytes_hashed: 0,
                skipped_hardlinks: 0,
                skipped_errors: progress.errors.load(Ordering::Relaxed),
                elapsed: start.elapsed(),
            },
            cancelled: false,
        };
    }

    // Build a bounded Rayon pool tuned for SSD or HDD
    let thread_count = config.io_profile.thread_count();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    // Stage 3: Partial hash (first 4 KB + last 4 KB) + NTFS hardlink deduplication
    progress.set_stage(ScanStage::PartialHashing);
    progress
        .candidates_total
        .store(size_candidates.len() as u64, Ordering::Relaxed);
    progress.candidates_done.store(0, Ordering::Relaxed);

    let partial_results: Vec<PartialResult> = pool.install(|| {
        size_candidates
            .into_par_iter()
            .map_init(
                || vec![0u8; SMALL_FILE_THRESHOLD as usize],
                |buf, candidate| compute_partial_or_small_hash(candidate, buf, &progress),
            )
            .flatten()
            .collect()
    });

    if progress.is_cancelled() {
        progress.set_stage(ScanStage::Cancelled);
        return ScanResult {
            groups: Vec::new(),
            summary: ScanSummary {
                total_files_seen,
                total_bytes_hashed: progress.bytes_hashed.load(Ordering::Relaxed),
                skipped_hardlinks: 0,
                skipped_errors: progress.errors.load(Ordering::Relaxed),
                elapsed: start.elapsed(),
            },
            cancelled: true,
        };
    }

    // Deduplicate NTFS hardlinks and bucket by (size, partial_hash)
    let mut seen_identities: FxHashSet<FileIdentity> = FxHashSet::default();
    let mut skipped_hardlinks = 0u64;

    let mut confirmed_small: FxHashMap<(u64, [u8; 32]), Vec<WalkCandidate>> = FxHashMap::default();
    let mut partial_buckets: FxHashMap<(u64, [u8; 32]), Vec<WalkCandidate>> = FxHashMap::default();

    for res in partial_results {
        if let Some(id) = res.identity {
            if !seen_identities.insert(id) {
                skipped_hardlinks += 1;
                continue;
            }
        }
        let key = (res.candidate.size, res.hash);
        if res.is_full_hash {
            confirmed_small.entry(key).or_default().push(res.candidate);
        } else {
            partial_buckets.entry(key).or_default().push(res.candidate);
        }
    }

    // Only large files that collided on (size, partial_hash) need Stage 4 full hashing!
    let full_candidates: Vec<WalkCandidate> = partial_buckets
        .into_values()
        .filter(|v| v.len() >= 2)
        .flatten()
        .collect();

    // Stage 4: Full-file BLAKE3 hash
    progress.set_stage(ScanStage::FullHashing);
    progress
        .candidates_total
        .store(full_candidates.len() as u64, Ordering::Relaxed);
    progress.candidates_done.store(0, Ordering::Relaxed);

    let full_results: Vec<(WalkCandidate, [u8; 32])> = pool.install(|| {
        full_candidates
            .into_par_iter()
            .map_init(
                || vec![0u8; FULL_HASH_BUF_SIZE],
                |buf, candidate| compute_full_hash(candidate, buf, &progress),
            )
            .flatten()
            .collect()
    });

    if progress.is_cancelled() {
        progress.set_stage(ScanStage::Cancelled);
        return ScanResult {
            groups: Vec::new(),
            summary: ScanSummary {
                total_files_seen,
                total_bytes_hashed: progress.bytes_hashed.load(Ordering::Relaxed),
                skipped_hardlinks,
                skipped_errors: progress.errors.load(Ordering::Relaxed),
                elapsed: start.elapsed(),
            },
            cancelled: true,
        };
    }

    let mut full_buckets: FxHashMap<(u64, [u8; 32]), Vec<WalkCandidate>> = confirmed_small;
    for (candidate, hash) in full_results {
        full_buckets
            .entry((candidate.size, hash))
            .or_default()
            .push(candidate);
    }

    // Build final duplicate groups
    let mut groups: Vec<DuplicateGroup> = full_buckets
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|((size, hash), mut files)| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let entries = files
                .into_iter()
                .map(|c| FileEntry {
                    path: c.path,
                    size: c.size,
                    modified: c.modified,
                    selected: false,
                })
                .collect();
            DuplicateGroup {
                hash_hex: bytes_to_hex_prefix(&hash),
                size,
                files: entries,
            }
        })
        .collect();

    // Sort by wasted space descending by default
    groups.sort_by(|a, b| b.wasted_bytes().cmp(&a.wasted_bytes()));

    progress.set_stage(ScanStage::Done);

    ScanResult {
        groups,
        summary: ScanSummary {
            total_files_seen,
            total_bytes_hashed: progress.bytes_hashed.load(Ordering::Relaxed),
            skipped_hardlinks,
            skipped_errors: progress.errors.load(Ordering::Relaxed),
            elapsed: start.elapsed(),
        },
        cancelled: false,
    }
}

fn bytes_to_hex_prefix(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(16);
    for b in &bytes[..8] {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Convenience helper to check if a path is a valid directory or drive root.
pub fn is_scannable_dir(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dupfinder_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_small_and_large_duplicates_and_rejects_partial_collisions() {
        let dir = temp_test_dir("basic");

        // 1. Small duplicates (<= 16 KB)
        fs::write(dir.join("small_a.txt"), b"hello duplicate world").unwrap();
        fs::write(dir.join("small_b.txt"), b"hello duplicate world").unwrap();

        // 2. Unique file
        fs::write(dir.join("unique.txt"), b"totally unique content here").unwrap();

        // 3. Large duplicates (> 16 KB)
        let mut large_data = vec![0xABu8; 40 * 1024];
        large_data[20_000] = 0x11;
        fs::write(dir.join("large_1.bin"), &large_data).unwrap();
        fs::write(dir.join("large_2.bin"), &large_data).unwrap();

        // 4. Same size & same head+tail as large_1, but different middle byte (must NOT be flagged!)
        let mut tricky = large_data.clone();
        tricky[20_000] = 0x99;
        fs::write(dir.join("large_tricky.bin"), &tricky).unwrap();

        // 5. Empty files (skipped when min_size = 1)
        fs::File::create(dir.join("empty_1.txt")).unwrap().flush().unwrap();
        fs::File::create(dir.join("empty_2.txt")).unwrap().flush().unwrap();

        let cfg = ScanConfig {
            roots: vec![dir.clone()],
            min_size: 1,
            skip_system_dirs: true,
            skip_dev_dirs: false,
            io_profile: crate::models::IoProfile::AutoSsd,
        };
        let progress = Arc::new(ScanProgress::default());
        let result = run_scan(&cfg, progress);

        assert!(!result.cancelled);
        assert_eq!(result.groups.len(), 2, "Expected 2 duplicate groups (small + large)");

        // Largest wasted space first -> large_1 & large_2
        assert_eq!(result.groups[0].size, 40 * 1024);
        assert_eq!(result.groups[0].files.len(), 2);

        // Second group -> small_a & small_b
        assert_eq!(result.groups[1].size, 21);
        assert_eq!(result.groups[1].files.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_ntfs_hardlinks() {
        let dir = temp_test_dir("hardlink");
        let orig = dir.join("orig.txt");
        let link = dir.join("hardlink.txt");
        fs::write(&orig, b"shared physical file content").unwrap();
        if fs::hard_link(&orig, &link).is_ok() {
            let cfg = ScanConfig {
                roots: vec![dir.clone()],
                ..Default::default()
            };
            let result = run_scan(&cfg, Arc::new(ScanProgress::default()));
            assert_eq!(result.groups.len(), 0, "Hardlinks must not be reported as duplicates");
            assert_eq!(result.summary.skipped_hardlinks, 1);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
