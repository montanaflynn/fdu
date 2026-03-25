# fdu - fast disk usage analyzer

4.8x faster than `du` on macOS, with a streaming TUI that shows results as it scans. Platform-optimized for macOS, Linux, and Windows.

## How it works

Each platform uses native APIs for maximum speed, combined with rayon's work-stealing thread pool for parallel directory walking:

- **macOS**: `getattrlistbulk` fetches metadata for all entries in a directory in a single syscall, instead of one `stat` per file
- **Linux**: Parallel `readdir` + `fstatat` across all cores (uses `d_type` for zero-cost type detection)
- **Windows**: `FindFirstFileExW` with `FindExInfoBasic` and `FIND_FIRST_EX_LARGE_FETCH` returns file sizes inline with enumeration -- no separate stat calls needed

Sizes are reported as allocated (on-disk) bytes, not logical file size. This correctly accounts for sparse files commonly created by container runtimes.

## Benchmarks

### macOS (~/go, 554k files, 18 GiB)

| Tool | Mean | vs fdu |
|------|------|--------|
| **fdu** | **4.1s** | **1.0x** |
| ncdu | 17.6s | 4.3x slower |
| du (macOS) | 18.7s | 4.6x slower |

### Cross-platform (CI, 10k files, 264 MiB)

| Platform | Time |
|----------|------|
| Linux | 6ms |
| macOS | 16ms |
| Windows | 36ms |

## Install

```
cargo install fdu
```

Homebrew (macOS):

```
brew install montanaflynn/tap/fdu
```

Pre-built binaries for macOS (ARM/x86), Linux, and Windows are available on the [releases page](https://github.com/montanaflynn/fdu/releases).

## Usage

```
Usage: fdu [OPTIONS] [PATH]

Arguments:
  [PATH]  Path to scan (default: current directory) [default: .]

Options:
  -n, --top <TOP>            Number of top entries to display [default: 20]
      --min-size <MIN_SIZE>  Minimum file size to display (e.g. 100MB, 1GB)
      --files-only           Only show files, skip directory aggregation
      --no-tui               Print results to stdout without TUI
  -h, --help                 Print help
```

## Controls

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `Tab` | Switch between files and directories tables |
| Up / `k` | Move selection up |
| Down / `j` | Move selection down |

## License

MIT
