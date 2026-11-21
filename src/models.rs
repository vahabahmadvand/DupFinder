use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanStage {
    Idle = 0,
    Walking = 1,
    GroupingBySize = 2,
    PartialHashing = 3,
    FullHashing = 4,
    Done = 5,
    Cancelled = 6,
}

impl ScanStage {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Walking,
            2 => Self::GroupingBySize,
            3 => Self::PartialHashing,
            4 => Self::FullHashing,
            5 => Self::Done,
            6 => Self::Cancelled,
            _ => Self::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Walking => "Stage 1/4: Scanning directories...",
            Self::GroupingBySize => "Stage 2/4: Filtering unique sizes...",
            Self::PartialHashing => "Stage 3/4: Quick partial hashing (head + tail)...",
            Self::FullHashing => "Stage 4/4: Full BLAKE3 verification...",
            Self::Done => "Scan complete",
            Self::Cancelled => "Scan cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoProfile {
    AutoSsd,
    HddGentle,
}

impl IoProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::AutoSsd => "SSD / NVMe (Fast)",
            Self::HddGentle => "HDD (Low-thrash)",
        }
    }

    pub fn thread_count(self) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        match self {
            Self::AutoSsd => cores.clamp(2, 16),
            Self::HddGentle => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub min_size: u64,
    pub skip_system_dirs: bool,
    pub skip_dev_dirs: bool,
    pub io_profile: IoProfile,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            min_size: 1, // Skip 0-byte files by default
            skip_system_dirs: true,
            skip_dev_dirs: false,
            io_profile: IoProfile::AutoSsd,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub hash_hex: String,
    pub size: u64,
    pub files: Vec<FileEntry>,
}

impl DuplicateGroup {
    pub fn wasted_bytes(&self) -> u64 {
        if self.files.len() <= 1 {
            0
        } else {
            self.size.saturating_mul((self.files.len() - 1) as u64)
        }
    }

    pub fn selected_count(&self) -> usize {
        self.files.iter().filter(|f| f.selected).count()
    }

    pub fn all_selected(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(|f| f.selected)
    }
}

#[derive(Debug, Clone)]
pub struct ScanSummary {
    pub total_files_seen: u64,
    pub total_bytes_hashed: u64,
    pub skipped_hardlinks: u64,
    pub skipped_errors: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub groups: Vec<DuplicateGroup>,
    pub summary: ScanSummary,
    pub cancelled: bool,
}

#[derive(Debug, Default)]
pub struct ScanProgress {
    pub stage: AtomicU8,
    pub files_scanned: AtomicU64,
    pub candidates_total: AtomicU64,
    pub candidates_done: AtomicU64,
    pub bytes_hashed: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
}

impl ScanProgress {
    pub fn set_stage(&self, stage: ScanStage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    pub fn get_stage(&self) -> ScanStage {
        ScanStage::from_u8(self.stage.load(Ordering::Relaxed))
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    WastedSpaceDesc,
    FileSizeDesc,
    CountDesc,
    NameAsc,
}

impl SortBy {
    pub fn label(self) -> &'static str {
        match self {
            Self::WastedSpaceDesc => "Wasted Space (Largest)",
            Self::FileSizeDesc => "File Size (Largest)",
            Self::CountDesc => "Duplicate Count",
            Self::NameAsc => "File Name (A-Z)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSelectRule {
    KeepOldest,
    KeepNewest,
    KeepShortestPath,
    KeepLongestPath,
}

impl AutoSelectRule {
    pub fn label(self) -> &'static str {
        match self {
            Self::KeepOldest => "Keep Oldest",
            Self::KeepNewest => "Keep Newest",
            Self::KeepShortestPath => "Keep Shortest Path",
            Self::KeepLongestPath => "Keep Longest Path",
        }
    }
}
