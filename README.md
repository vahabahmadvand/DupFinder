# DupFinder

An ultra-fast, lightweight native Windows duplicate file finder written in Rust. Compiles to a single ~5.5 MB executable with zero runtime dependencies.

## Features

- **4-Stage Progressive Pipeline** — avoids reading unique files from disk:
  1. **Parallel Directory Walk (`jwalk`)** — scans across CPU cores, prunes skipped folders early, and ignores symlinks/reparse points.
  2. **Exact File-Size Grouping** — eliminates 80–95% of files immediately with zero file reads.
  3. **8 KB Partial Hash + NTFS Hardlink Deduplication** — hashes only the first 4 KB and last 4 KB of same-size files (files ≤ 16 KB are fully hashed in a single open) and uses Windows `GetFileInformationByHandle` to skip hardlinks pointing to the same physical file.
  4. **Parallel SIMD BLAKE3 Verification** — full-hashes only true candidates using reusable 256 KB thread-local buffers.
- **Lightweight Native GUI (`eframe` / `egui`)**:
  - Quick-select drives (`C:\`, `D:\`, …), native folder picker, and drag-and-drop folder support.
  - Tunable **SSD / NVMe** vs **HDD (Low-thrash)** I/O concurrency profiles.
  - Live progress with stage indicator, files scanned, throughput (MB/s), elapsed time, and instant cancel.
  - Virtualized result list for smooth 60 FPS scrolling even with tens of thousands of duplicates.
- **Safe Cleanup**:
  - **Auto-Select rules**: *Keep Oldest*, *Keep Newest*, *Keep Shortest Path*, and *Keep Longest Path*.
  - **Safety guard**: warns if every copy in a duplicate group is selected.
  - **Windows Recycle Bin**: moves selected files to the Recycle Bin via native Windows `IFileOperation` (with per-file fallback if a file is locked), plus one-click **Reveal in Explorer**.

## Building from Source

### Prerequisites
- [Rust](https://rustup.rs/) (edition 2024 / Rust 1.85+)
- Windows MSVC build tools

### Run in Release Mode
```powershell
cargo run --release
```

### Build Standalone Executable
```powershell
cargo build --release
```
The standalone executable will be generated at `target/release/dupfinder.exe`.

### Run Tests
```powershell
cargo test
```

## Project Structure

```text
DupFinder/
├── assets/
│   ├── icon.png         # Application icon (embedded in window/taskbar)
│   └── icon.ico         # Multi-resolution Windows executable icon
├── build.rs             # Embeds icon.ico into Windows .exe resources
├── Cargo.toml
└── src/
    ├── main.rs          # Entry point & native window configuration
    ├── models.rs        # Core data structures & lock-free scan progress
    ├── scanner.rs       # 4-stage parallel BLAKE3 duplicate detection engine
    ├── actions.rs       # Auto-select rules, Recycle Bin deletion & Explorer reveal
    └── ui.rs            # Virtualized egui interface
```

## License

MIT
