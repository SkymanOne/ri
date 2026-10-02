# Provider cassettes

HTTP exchanges with model providers, replayed by `ri-mock`. Each wire API has a directory and each scenario a file, for example `anthropic-messages/text.json`. The format is defined in `crates/ri-mock/src/cassette.rs`.

Cassettes are hand-written or recorded. To record, run the proxy and point a client at it instead of the provider:

```sh
cargo xtask mock-sse --record https://opencode.ai/zen/go --out tests/fixtures/cassettes/anthropic-messages/<scenario>.json
```

The proxy forwards each request with its credentials and stores the response chunks as they arrive. Recorded requests and cassettes never contain credentials: authorization headers and `key` query parameters are redacted, and cookies are dropped.

## Serve a cassette to pi

Start the server. It prints its base URL:

```sh
cargo xtask mock-sse --cassette tests/fixtures/cassettes/anthropic-messages/text.json --requests /tmp/requests.json
```

In a second shell, point pi's provider at that URL through `models.json` in a scratch agent directory, then run pi in print mode:

```sh
mkdir -p /tmp/pi-agent
echo '{"providers":{"anthropic":{"baseUrl":"http://127.0.0.1:PORT"}}}' > /tmp/pi-agent/models.json
PI_CODING_AGENT_DIR=/tmp/pi-agent PI_OFFLINE=1 ANTHROPIC_API_KEY=mock \
  pi -p --no-session --model anthropic/claude-sonnet-4-5 "Say hello" < /dev/null
```

- pi prints `Hello from the mock.`
- Stop the server with Ctrl-C. It writes the requests pi sent and reports any mismatch.
- `pi` must be version 1.0.0. After installing the fixture generator, it is at `tests/fixtures/pi/generator/node_modules/.bin/pi`.
- In print mode, pi reads piped stdin into the prompt. When stdin is not a terminal, redirect it from `/dev/null`.
