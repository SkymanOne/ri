# Performance

yapi is built to start fast, use little memory and install small. Each measure below has a budget in the project's [AGENTS.md](https://github.com/SkymanOne/ri/blob/main/AGENTS.md), and `cargo xtask bench` checks yapi against it with Pi on the same machine.

## Results

Linux, measured on an x86_64 virtual machine (Intel Xeon at 2.1 GHz, 4 hardware threads) against Pi `1.0.0` on Node.js 22.22.0. Each value is the median of the samples, with their range in parentheses.

| Measure | Budget | yapi | Pi | Pi / yapi |
|---|---|---|---|---|
| `--version` | < 5 ms | 2.2 ms (2.0 to 4.2) | 240.5 ms (206.6 to 425.1) | 109× |
| Print mode, start to first request byte | < 25 ms | 13.2 ms (11.6 to 16.6) | 332.5 ms (301.5 to 372.3) | 25× |
| Interactive first paint | < 40 ms | 9.1 ms (8.4 to 81.1) | 308.4 ms (263.6 to 414.8) | 34× |
| Keystroke to paint, p50, 10,000-line session | | 1.1 ms | 2.8 ms | 2.6× |
| Keystroke to paint, p99, 10,000-line session | < 16 ms | 2.3 ms | 6.5 ms | 2.8× |
| Idle memory | < 30 MB | 19.5 MB (19.2 to 19.7) | 115.0 MB (113.1 to 116.4) | 5.9× |
| Idle memory with 10 JS extensions | < 70 MB | 32.2 MB (31.8 to 32.6) | 118.3 MB (118.1 to 119.4) | 3.7× |
| Install size | < 35 MB | 32.2 MB | 245.2 MB | 7.6× |

On the same machine, starting the system's `true` takes 1.1 ms and starting Node.js with an empty script takes 22.4 ms. Those are the floors for each program's startup.

## What each measure means

- **`--version`**: wall time of `--version` until the process exits.
- **Print mode**: wall time from starting `-p "hi"` until a local listener standing in for the provider receives the first byte of the request. It covers loading settings, credentials, the model catalog and the session, and building the request.
- **Interactive first paint**: time from starting the interactive mode in a 100×40 pseudo-terminal until the footer shows the model.
- **Keystroke to paint**: time from writing a key into the pseudo-terminal until the character appears on screen, over 200 keystrokes 20 ms apart, in a session whose transcript renders about 10,000 lines.
- **Idle memory**: resident set size (RSS) of the interactive process 2 s after first paint, as `/proc` (Linux) or `ps` (macOS) reports it.
- **Idle memory with 10 JS extensions**: the same with ten small TypeScript extensions in the agent directory, each registering a tool, a command and an event handler. The first start fills the compile caches and is not measured.
- **Install size**: yapi's release executable, which the release profile strips. For Pi, the published npm package and its dependencies installed with `--ignore-scripts` (121.8 MB, including prebuilt native modules for every platform), plus the Node.js executable it needs (123.4 MB).

## Method

- Every program runs with a cleared environment, a fresh home and agent directory, `PI_OFFLINE=1`, and a placeholder Anthropic key pointed at a local listener, so no measure touches the network.
- Startup measures run 20 times per program and memory measures 5 times, alternating between yapi and Pi, so drift on the machine affects both alike. Keystrokes run 200 times per program.
- Medians are reported because single slow runs, such as the 81 ms first paint above, come from the machine rather than the program. The ranges show how much the samples vary.
- Pi runs from the same npm install whose size is measured, as its README recommends installing it.
- MB means 10^6 bytes.

Results vary with the machine. Compare yapi and Pi from the same run, not numbers from different machines.

## Run it yourself

```sh
cargo build --release -p yapi
npm install --prefix pi-install --ignore-scripts @earendil-works/pi-coding-agent@1.0.0
cargo xtask bench --pi pi-install/node_modules/.bin/pi --pi-install pi-install
```

The report is Markdown. Leave out `--pi` to measure yapi alone. The Bench workflow runs the same steps on GitHub's hosted Linux and macOS runners, by hand or for a pushed commit whose message contains `[bench]`.
