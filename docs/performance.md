# Performance

yapi is built to start fast, use little memory and install small. `cargo xtask bench` measures it against Pi on the same machine.

## Results

The [Bench workflow](https://github.com/SkymanOne/yapi/actions/runs/37471240557) measured these on GitHub's hosted runners, which are shared virtual machines, at commit `ccf76a6`, against Pi `1.0.0`. Each value is the median of the samples, with their range in parentheses.

Linux, on `ubuntu-latest` (AMD EPYC 9V74, 4 hardware threads), with Pi on Node.js 22.23.3:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| Startup, interactive | 10.6 ms (10.4 to 11.7) | 316.1 ms (299.3 to 349.3) | 29.8× |
| Startup, print mode (to first request byte) | 14.9 ms (14.6 to 15.5) | 340.5 ms (332.0 to 364.2) | 22.8× |
| Keystroke to paint, p50, 10,000-line session | 0.6 ms | 2.3 ms | 3.9× |
| Keystroke to paint, p99, 10,000-line session | 0.7 ms | 4.1 ms | 6.1× |
| Memory, idle | 18.1 MB (17.8 to 18.2) | 116.2 MB (114.0 to 116.9) | 6.4× |
| Memory, 10,000-line session open | 32.0 MB (31.8 to 32.1) | 149.9 MB (147.1 to 150.8) | 4.7× |
| Memory after 20 turns with tool calls | 35.4 MB (34.8 to 36.3) | 198.3 MB (197.7 to 201.9) | 5.6× |
| Memory with 10 small JS extensions | 35.8 MB (35.8 to 36.3) | 119.6 MB (118.2 to 121.4) | 3.3× |
| Memory with 57 of Pi's example extensions | 42.1 MB (41.9 to 42.4) | 121.9 MB (121.3 to 122.5) | 2.9× |
| Install size | 32.5 MB | 246.6 MB | 7.6× |

macOS, on `macos-latest` (Apple M1 virtual machine, 3 hardware threads), with Pi on Node.js 22.23.2:

| Measure | yapi | Pi | Pi / yapi |
|---|---|---|---|
| Startup, interactive | 23.5 ms (16.3 to 156.9) | 363.5 ms (286.2 to 526.7) | 15.5× |
| Startup, print mode (to first request byte) | 14.3 ms (11.8 to 36.7) | 293.9 ms (267.2 to 371.1) | 20.5× |
| Keystroke to paint, p50, 10,000-line session | 0.9 ms | 3.3 ms | 3.6× |
| Keystroke to paint, p99, 10,000-line session | 2.1 ms | 9.7 ms | 4.5× |
| Memory, idle | 16.2 MB (16.2 to 16.3) | 125.2 MB (123.8 to 125.5) | 7.7× |
| Memory, 10,000-line session open | 32.0 MB (31.9 to 32.2) | 151.7 MB (150.2 to 152.1) | 4.7× |
| Memory after 20 turns with tool calls | 37.6 MB (37.5 to 37.7) | 205.7 MB (204.7 to 207.6) | 5.5× |
| Memory with 10 small JS extensions | 26.9 MB (26.9 to 27.0) | 129.0 MB (127.2 to 130.1) | 4.8× |
| Memory with 57 of Pi's example extensions | 34.1 MB (33.9 to 34.1) | 130.5 MB (129.6 to 130.8) | 3.8× |
| Install size | 26.6 MB | 233.9 MB | 8.8× |

Single samples on the macOS runner reach 157 ms for yapi's interactive startup and 527 ms for Pi's, which is why the tables report medians.

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
