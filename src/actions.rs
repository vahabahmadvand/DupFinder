use crate::models::{AutoSelectRule, DuplicateGroup, SortBy};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn apply_auto_select(groups: &mut [DuplicateGroup], rule: AutoSelectRule) {
    for group in groups.iter_mut() {
        if group.files.len() < 2 {
            continue;
        }

        let keep_idx = match rule {
            AutoSelectRule::KeepOldest => group
                .files
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let ta = a.modified.unwrap_or(SystemTime::UNIX_EPOCH);
                    let tb = b.modified.unwrap_or(SystemTime::UNIX_EPOCH);
                    ta.cmp(&tb).then_with(|| a.path.cmp(&b.path))
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
            AutoSelectRule::KeepNewest => group
                .files
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    let ta = a.modified.unwrap_or(SystemTime::UNIX_EPOCH);
                    let tb = b.modified.unwrap_or(SystemTime::UNIX_EPOCH);
                    ta.cmp(&tb).then_with(|| b.path.cmp(&a.path))
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
            AutoSelectRule::KeepShortestPath => group
                .files
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    a.path
                        .as_os_str()
                        .len()
                        .cmp(&b.path.as_os_str().len())
                        .then_with(|| a.path.cmp(&b.path))
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
            AutoSelectRule::KeepLongestPath => group
                .files
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    a.path
                        .as_os_str()
                        .len()
                        .cmp(&b.path.as_os_str().len())
                        .then_with(|| b.path.cmp(&a.path))
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
        };

        for (i, file) in group.files.iter_mut().enumerate() {
            file.selected = i != keep_idx;
        }
    }
}

pub fn clear_selection(groups: &mut [DuplicateGroup]) {
    for group in groups.iter_mut() {
        for file in group.files.iter_mut() {
            file.selected = false;
        }
    }
}

pub fn sort_groups(groups: &mut [DuplicateGroup], sort_by: SortBy) {
    match sort_by {
        SortBy::WastedSpaceDesc => {
            groups.sort_by(|a, b| {
                b.wasted_bytes()
                    .cmp(&a.wasted_bytes())
                    .then_with(|| b.size.cmp(&a.size))
            });
        }
        SortBy::FileSizeDesc => {
            groups.sort_by(|a, b| {
                b.size
                    .cmp(&a.size)
                    .then_with(|| b.files.len().cmp(&a.files.len()))
            });
        }
        SortBy::CountDesc => {
            groups.sort_by(|a, b| {
                b.files
                    .len()
                    .cmp(&a.files.len())
                    .then_with(|| b.wasted_bytes().cmp(&a.wasted_bytes()))
            });
        }
        SortBy::NameAsc => {
            groups.sort_by(|a, b| {
                let na = a
                    .files
                    .first()
                    .and_then(|f| f.path.file_name())
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                let nb = b
                    .files
                    .first()
                    .and_then(|f| f.path.file_name())
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                na.cmp(&nb)
            });
        }
    }
}

#[derive(Debug, Default)]
pub struct DeleteReport {
    pub deleted_count: usize,
    pub freed_bytes: u64,
    pub failed: Vec<(PathBuf, String)>,
}

/// Moves all selected files to the OS Recycle Bin and updates `groups` in-place.
pub fn move_selected_to_recycle_bin(groups: &mut Vec<DuplicateGroup>) -> DeleteReport {
    let mut to_delete: Vec<(PathBuf, u64)> = Vec::new();
    for g in groups.iter() {
        for f in &g.files {
            if f.selected {
                to_delete.push((f.path.clone(), f.size));
            }
        }
    }

    if to_delete.is_empty() {
        return DeleteReport::default();
    }

    let mut report = DeleteReport::default();
    let mut succeeded_paths: rustc_hash::FxHashSet<PathBuf> = rustc_hash::FxHashSet::default();

    // Fast path: batch delete via OS Recycle Bin API (single IFileOperation on Windows)
    let path_refs: Vec<&Path> = to_delete.iter().map(|(p, _)| p.as_path()).collect();
    if trash::delete_all(&path_refs).is_ok() {
        for (p, sz) in to_delete {
            report.deleted_count += 1;
            report.freed_bytes = report.freed_bytes.saturating_add(sz);
            succeeded_paths.insert(p);
        }
    } else {
        // Fallback: delete individually so one locked/in-use file doesn't block the rest
        for (p, sz) in to_delete {
            match trash::delete(&p) {
                Ok(()) => {
                    report.deleted_count += 1;
                    report.freed_bytes = report.freed_bytes.saturating_add(sz);
                    succeeded_paths.insert(p);
                }
                Err(e) => {
                    report.failed.push((p, e.to_string()));
                }
            }
        }
    }

    // Remove successfully deleted files from groups and drop groups with < 2 files left
    for g in groups.iter_mut() {
        g.files.retain(|f| !succeeded_paths.contains(&f.path));
    }
    groups.retain(|g| g.files.len() >= 2);

    report
}

pub fn reveal_in_explorer(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

pub fn available_drives() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut drives = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if root.exists() {
                drives.push(root);
            }
        }
        drives
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let rem = secs - (mins as f64 * 60.0);
        format!("{mins}m {rem:.1}s")
    }
}

pub fn format_timestamp(time: Option<SystemTime>) -> String {
    match time {
        Some(t) => {
            let dt: DateTime<Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FileEntry;

    fn sample_group() -> DuplicateGroup {
        let t_old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let t_mid = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let t_new = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000);
        DuplicateGroup {
            hash_hex: "deadbeef".into(),
            size: 1024,
            files: vec![
                FileEntry {
                    path: PathBuf::from(r"C:\very\deep\folder\copy.bin"),
                    size: 1024,
                    modified: Some(t_old),
                    selected: false,
                },
                FileEntry {
                    path: PathBuf::from(r"C:\a.bin"),
                    size: 1024,
                    modified: Some(t_mid),
                    selected: false,
                },
                FileEntry {
                    path: PathBuf::from(r"C:\mid\file.bin"),
                    size: 1024,
                    modified: Some(t_new),
                    selected: false,
                },
            ],
        }
    }

    #[test]
    fn auto_select_rules_keep_exactly_one() {
        let mut groups = vec![sample_group()];

        apply_auto_select(&mut groups, AutoSelectRule::KeepOldest);
        assert!(!groups[0].files[0].selected);
        assert!(groups[0].files[1].selected);
        assert!(groups[0].files[2].selected);

        apply_auto_select(&mut groups, AutoSelectRule::KeepNewest);
        assert!(groups[0].files[0].selected);
        assert!(groups[0].files[1].selected);
        assert!(!groups[0].files[2].selected);

        apply_auto_select(&mut groups, AutoSelectRule::KeepShortestPath);
        assert!(groups[0].files[0].selected);
        assert!(!groups[0].files[1].selected);
        assert!(groups[0].files[2].selected);

        apply_auto_select(&mut groups, AutoSelectRule::KeepLongestPath);
        assert!(!groups[0].files[0].selected);
        assert!(groups[0].files[1].selected);
        assert!(groups[0].files[2].selected);
    }
}
