# Performance

ri has a budget for each measure below. `cargo xtask bench --pi <path>` runs ri and pi on the same machine and reports both.

| Measure | Budget | ri, Linux | pi, Linux | ri, macOS | pi, macOS |
|---|---|---|---|---|---|
| `ri --version` | < 5 ms | 2.0 ms | 246 ms | 5.8 ms | 204 ms |
| Print mode, start to first request byte | < 25 ms | 13.1 ms | 336 ms | 10.8 ms | 323 ms |
| Interactive first paint | < 40 ms | 10.0 ms | 330 ms | 22.7 ms | 270 ms |
| Keystroke to paint, p99, 10k-line session | < 16 ms | 1.9 ms | 6.1 ms | 6.7 ms | 9.7 ms |
| Idle memory | < 30 MB | 15.5 MiB | 106 MiB | 14.4 MiB | 119 MiB |
| Idle memory with 10 JS extensions | < 70 MB | 28.6 MiB | 110 MiB | 26.2 MiB | 124 MiB |
| Stripped binary | < 35 MB | 33.6 MB | | 26.5 MB | |

Linux results come from an x86_64 container. macOS results come from GitHub's hosted arm64 runner, which is noisier than a desktop machine.

On macOS, `ri --version` is over budget. The system loads the Security and CoreFoundation frameworks before ri starts, which costs about 2 ms. ri links them only to check certificates against the system trust store, and dropping that link is planned.
