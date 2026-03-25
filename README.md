# fdu - fast disk usage analyzer

4.8x faster than `du` on macOS, with a streaming TUI that shows results as it scans.

## How it works

On macOS, `fdu` uses the `getattrlistbulk` syscall to fetch metadata for all entries in a directory in a single call, instead of issuing one `stat` per file. Directory walking is parallelized with rayon's work-stealing thread pool. On other platforms, it falls back to jwalk for parallel traversal.

Sizes are reported as allocated (on-disk) bytes, not logical file size. This correctly accounts for sparse files commonly created by container runtimes.

## Benchmark

Tested on `~/go` (554k files, 18 GiB):

| Tool | Mean | vs fdu |
|------|------|--------|
| **fdu** | **4.1s** | **1.0x** |
| ncdu | 17.6s | 4.3x slower |
| du (macOS) | 18.7s | 4.6x slower |

## Install

```
cargo install fdu
```

Homebrew (`brew install fdu`) coming soon.

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
