# Performance

yapi is built to start fast, use little memory and install small. `cargo xtask bench` measures it against Pi on the same machine.

## Results

Linux, measured on an x86_64 virtual machine (Intel Xeon at 2.1 GHz, 4 hardware threads) against Pi `1.0.0` on Node.js 22.22.0. Each value is the median of the samples, with their range in parentheses.

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| `--version` | 2.6 ms (2.3 to 3.9) | 307.7 ms (271.6 to 362.2) | 120× |
| Print mode, start to first request byte | 17.2 ms (14.8 to 23.1) | 451.1 ms (392.2 to 521.0) | 26× |
| Interactive first paint | 12.0 ms (10.1 to 16.2) | 410.9 ms (362.6 to 473.7) | 34× |
| Keystroke to paint, p50, 10,000-line session | 1.5 ms | 4.5 ms | 2.9× |
| Keystroke to paint, p99, 10,000-line session | 4.7 ms | 11.7 ms | 2.5× |
| Memory, idle | 18.9 MB (18.8 to 19.0) | 112.5 MB (111.8 to 112.8) | 6.0× |
| Memory, 10,000-line session open | 33.0 MB (32.7 to 33.0) | 147.2 MB (146.4 to 147.6) | 4.5× |
| Memory after 20 turns with tool calls | 37.9 MB (37.4 to 38.1) | 198.2 MB (195.2 to 200.1) | 5.2× |
| Memory with 10 small JS extensions | 31.4 MB (31.3 to 31.6) | 115.7 MB (115.1 to 116.5) | 3.7× |
| Memory with 57 of Pi's example extensions | 38.3 MB (38.1 to 38.5) | 116.7 MB (116.2 to 117.6) | 3.1× |
| Install size | 32.3 MB | 245.2 MB | 7.6× |

On the same machine, starting the system's `true` takes 1.2 ms and starting Node.js with an empty script takes 29.5 ms. Those are the floors for each program's startup.

## GitHub's hosted runners

The Bench workflow runs the same benchmark on GitHub's hosted runners, which are shared virtual machines.

Linux, on `ubuntu-latest` (AMD EPYC 9V45, 4 hardware threads), against Pi `1.0.0` on Node.js 22.23.3:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| `--version` | 1.3 ms (1.0 to 1.4) | 138.9 ms (132.4 to 176.9) | 106× |
| Print mode, start to first request byte | 9.5 ms (9.2 to 10.0) | 204.6 ms (197.4 to 263.8) | 22× |
| Interactive first paint | 6.1 ms (5.7 to 6.3) | 191.0 ms (182.2 to 226.0) | 32× |
| Keystroke to paint, p50, 10,000-line session | 0.4 ms | 1.0 ms | 2.4× |
| Keystroke to paint, p99, 10,000-line session | 0.8 ms | 2.6 ms | 3.1× |
| Idle memory | 17.4 MB (17.4 to 17.7) | 115.5 MB (114.2 to 116.5) | 6.6× |
| Idle memory with 10 JS extensions | 35.4 MB (35.3 to 35.9) | 119.3 MB (118.5 to 120.3) | 3.4× |
| Install size | 32.3 MB | 246.6 MB | 7.6× |

macOS, on `macos-latest` (Apple M1 virtual machine, 3 hardware threads), against Pi `1.0.0` on Node.js 22.23.2:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| `--version` | 6.7 ms (6.1 to 9.0) | 182.2 ms (167.6 to 269.8) | 27× |
| Print mode, start to first request byte | 12.1 ms (9.5 to 81.4) | 259.4 ms (219.9 to 337.3) | 21× |
| Interactive first paint | 15.8 ms (11.9 to 462.9) | 271.1 ms (223.4 to 373.2) | 17× |
| Keystroke to paint, p50, 10,000-line session | 3.0 ms | 4.6 ms | 1.5× |
| Keystroke to paint, p99, 10,000-line session | 10.6 ms | 10.4 ms | 1.0× |
| Idle memory | 16.3 MB (16.3 to 16.3) | 125.9 MB (124.2 to 127.6) | 7.7× |
| Idle memory with 10 JS extensions | 26.8 MB (26.8 to 26.9) | 129.8 MB (129.1 to 129.9) | 4.8× |
| Install size | 26.3 MB | 233.9 MB | 8.9× |

The start floors were 0.3 ms for `true` and 14.6 ms for Node.js on Linux, and 1.7 ms and 36.7 ms on macOS.

On macOS, `--version` takes 5.0 ms more than `true`, where Linux takes 1.0 ms more. Part of the difference is the system loading the Security and CoreFoundation frameworks before yapi's code runs. yapi links them to check certificates against the system's trust store, and a probe on the same runner type measured 2.1 ms for loading them. The rest has not been traced yet.

Keystroke latency on the macOS runner is close to Pi's at p99 (10.6 ms and 10.4 ms). Single samples there reach 463 ms for a first paint and 81 ms in print mode, which is why the tables report medians.

## What each measure means

- **`--version`**: wall time of `--version` until the process exits.
- **Print mode**: wall time from starting `-p "hi"` until a local listener standing in for the provider receives the first byte of the request. It covers loading settings, credentials, the model catalog and the session, and building the request.
- **Interactive first paint**: time from starting the interactive mode in a 100×40 pseudo-terminal until the footer shows the model.
- **Keystroke to paint**: time from writing a key into the pseudo-terminal until the character appears on screen, over 200 keystrokes 20 ms apart, in a session whose transcript renders about 10,000 lines.
- **Memory**: resident set size (RSS) of the program and its child processes, as `ps` reports it, 2 s after the first paint or, for the session of turns, 2 s after the last answer.
- **Memory, idle**: an interactive start with no session history and no extensions.
- **Memory, 10,000-line session open**: the session of the keystroke measure, opened with `--session`.
- **Memory after 20 turns with tool calls**: 20 prompts typed into the editor. A local mock provider answers each with a tool call, alternately `read` on a 1,000-line file and `bash` running `seq 1 3000`, and then with a Markdown answer holding a list and a code block. The mock checks that both programs make the same 40 requests.
- **Memory with 10 small JS extensions**: ten TypeScript extensions in the agent directory, each registering a tool, a command and an event handler.
- **Memory with Pi's example extensions**: 57 of the 70 single-file example extensions that ship in Pi's npm package, loaded together from the agent directory. The rest are left out: five replace the footer, header or editor, where the benchmark looks for the first paint, three start commands, timers or file watchers when a session starts, one commits to the working directory's repository on exit, and four override a built-in tool that another example also overrides, which Pi refuses to load together.
- For every measure with extensions, a first start fills the compile caches and is not measured.
- **Install size**: yapi's release executable, which the release profile strips. For Pi, the published npm package and its dependencies installed with `--ignore-scripts` (121.8 MB, including prebuilt native modules for every platform), plus the Node.js executable it needs (123.4 MB).

## Method

- Every program runs with a cleared environment, a fresh home and agent directory, `PI_OFFLINE=1`, and a placeholder Anthropic key pointed at a local listener or mock provider, so no measure touches the network.
- Startup measures run 20 times per program and memory measures 5 times, alternating between yapi and Pi, so drift on the machine affects both alike. Keystrokes run 200 times per program.
- Medians are reported because single slow runs come from the machine rather than the program. The ranges show how much the samples vary.
- Pi runs from the same npm install whose size is measured, as its README recommends installing it. The example extensions come from that install too.
- MB means 10^6 bytes.

Results vary with the machine. Compare yapi and Pi from the same run, not numbers from different machines.

## Run it yourself

```sh
cargo build --release -p yapi
npm install --prefix pi-install --ignore-scripts @earendil-works/pi-coding-agent@1.0.0
cargo xtask bench --pi pi-install/node_modules/.bin/pi --pi-install pi-install
```

The report is Markdown. Leave out `--pi` to measure yapi alone, and `--pi-install` to skip the install size and the example extensions. The Bench workflow runs the same steps on GitHub's hosted Linux and macOS runners, by hand or for a pushed commit whose message contains `[bench]`.
