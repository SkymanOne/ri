# Performance

yapi is built to start fast, use little memory and install small. `cargo xtask bench` measures it against Pi on the same machine.

## Results

The [Bench workflow](https://github.com/SkymanOne/yapi/actions/runs/37601516053) measured these on GitHub's hosted runners, which are shared virtual machines, at commit `ecd633a`, against Pi `1.0.0`. Each value is the median of the samples, with their range in parentheses.

Linux, on `ubuntu-latest` (AMD EPYC 9V74, 4 hardware threads), with Pi on Node.js 22.23.3:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| Startup, interactive | 10.1 ms (9.8 to 10.8) | 323.9 ms (310.5 to 367.9) | 31.9× |
| Startup, print mode (to first request byte) | 14.1 ms (13.9 to 15.3) | 352.3 ms (342.4 to 373.0) | 24.9× |
| Keystroke to paint, p50, 10,000-line session | 0.7 ms | 1.9 ms | 2.8× |
| Keystroke to paint, p99, 10,000-line session | 0.9 ms | 3.8 ms | 4.4× |
| Memory, idle | 18.8 MB (18.7 to 19.1) | 117.4 MB (116.8 to 119.2) | 6.2× |
| Memory, 10,000-line session open | 32.8 MB (32.7 to 33.0) | 151.7 MB (149.7 to 154.5) | 4.6× |
| Memory after 20 turns with tool calls | 37.0 MB (36.8 to 37.6) | 202.6 MB (199.9 to 203.7) | 5.5× |
| Memory with 10 small JS extensions | 40.0 MB (39.6 to 40.1) | 121.1 MB (120.5 to 121.7) | 3.0× |
| Memory with 57 of Pi's example extensions | 43.1 MB (42.8 to 43.3) | 122.3 MB (122.0 to 123.1) | 2.8× |
| Install size | 35.0 MB | 246.6 MB | 7.0× |

macOS, on `macos-latest` (Apple M1 virtual machine, 3 hardware threads), with Pi on Node.js 22.23.2:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| Startup, interactive | 24.6 ms (15.2 to 127.6) | 343.3 ms (271.8 to 443.9) | 14.0× |
| Startup, print mode (to first request byte) | 14.0 ms (10.8 to 30.6) | 287.9 ms (252.1 to 321.9) | 20.6× |
| Keystroke to paint, p50, 10,000-line session | 1.7 ms | 3.7 ms | 2.1× |
| Keystroke to paint, p99, 10,000-line session | 3.8 ms | 8.3 ms | 2.2× |
| Memory, idle | 17.1 MB (16.9 to 17.1) | 126.0 MB (125.4 to 127.7) | 7.4× |
| Memory, 10,000-line session open | 32.5 MB (32.3 to 32.6) | 152.1 MB (151.6 to 152.6) | 4.7× |
| Memory after 20 turns with tool calls | 38.7 MB (38.4 to 39.3) | 209.1 MB (204.4 to 209.9) | 5.4× |
| Memory with 10 small JS extensions | 31.2 MB (31.1 to 31.3) | 129.5 MB (128.7 to 129.9) | 4.2× |
| Memory with 57 of Pi's example extensions | 35.0 MB (35.0 to 35.1) | 131.1 MB (130.2 to 131.6) | 3.7× |
| Install size | 28.7 MB | 233.9 MB | 8.1× |

Single samples on the macOS runner reach 128 ms for yapi's interactive startup and 444 ms for Pi's, which is why the tables report medians.

## What each measure means

- **Startup, interactive**: time from starting the interactive mode in a 100×40 pseudo-terminal until the footer shows the model.
- **Startup, print mode (to first request byte)**: wall time from starting `-p "hi"` until a local listener standing in for the provider receives the first byte of the request. It covers loading settings, credentials, the model catalog and the session, and building the request.
- **Keystroke to paint**: time from writing a key into the pseudo-terminal until the character appears on screen, over 200 keystrokes 20 ms apart, in a session whose transcript renders about 10,000 lines.
- **Memory**: resident set size (RSS) of the program and its child processes, as `ps` reports it, 2 s after the first paint or, for the session of turns, 2 s after the last answer.
- **Memory, idle**: an interactive start with no session history and no extensions.
- **Memory, 10,000-line session open**: the session of the keystroke measure, opened with `--session`.
- **Memory after 20 turns with tool calls**: 20 prompts typed into the editor. A local mock provider answers each with a tool call, alternately `read` on a 1,000-line file and `bash` running `seq 1 3000`, and then with a Markdown answer holding a list and a code block. The mock checks that both programs make the same 40 requests.
- **Memory with 10 small JS extensions**: ten TypeScript extensions in the agent directory, each registering a tool, a command and an event handler.
- **Memory with Pi's example extensions**: 57 of the 70 single-file example extensions that ship in Pi's npm package, loaded together from the agent directory. The rest are left out: five replace the footer, header or editor, where the benchmark looks for the first paint, three start commands, timers or file watchers when a session starts, one commits to the working directory's repository on exit, and four override a built-in tool that another example also overrides, which Pi refuses to load together.
- For every measure with extensions, a first start fills the compile caches and is not measured.
- **Install size**: yapi's release executable, which the release profile strips. For Pi, the published npm package and its dependencies installed with `--ignore-scripts` (121.8 MB, including prebuilt native modules for every platform), plus the Node.js executable it needs (124.8 MB on Linux).

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

The report is Markdown, and its header shows the commit measured. Leave out `--pi` to measure yapi alone, and `--pi-install` to skip the install size and the example extensions.

The Bench workflow runs the same steps on GitHub's hosted Linux and macOS runners. Start it by hand from the repository's Actions tab. Each run uploads its report as an artifact.
