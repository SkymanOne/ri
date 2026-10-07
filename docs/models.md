# Models and sign-in

yapi supports the same providers and models as Pi, from the same built-in catalog.

## API keys

yapi reads the same environment variables as Pi. Common ones:

| Provider | Variable |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Groq | `GROQ_API_KEY` |
| xAI | `XAI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| OpenCode Zen and Go | `OPENCODE_API_KEY` |

`yapi --help` lists every provider, including Azure OpenAI, Amazon Bedrock, Cloudflare and Vercel AI Gateway.

To store a key instead, run `/login` inside yapi and choose "Sign in with an API key". yapi saves it to `~/.yapi/agent/auth.json` in Pi's format.

## Subscriptions and accounts

Run `/login` and choose "Sign in with an account" to use a Claude Pro or Max, ChatGPT Plus or Pro, GitHub Copilot, Kimi For Coding, Meta, SuperGrok or X Premium account, or to create an OpenRouter key. yapi opens the browser sign-in, or shows a device code, and refreshes the token when it expires.

"Sign in with Radius" at the top of `/login` signs in to Pi's Radius gateway. Its models come from the gateway's own catalog, which yapi fetches after the sign-in.

Providers that extensions register can add their own sign-ins to `/login`. See [Providers from extensions](extensions.md#providers-from-extensions).

`/logout` removes a stored credential.

## Cloud providers

Amazon Bedrock, Google Vertex AI and Cloudflare use their platform's credentials, as in Pi:

- Amazon Bedrock reads `AWS_BEARER_TOKEN_BEDROCK`, or AWS credentials from the environment, `~/.aws` profiles (including SSO and assumed roles), container credentials or instance metadata, with `AWS_REGION` or the profile's region.
- Google Vertex AI reads `GOOGLE_CLOUD_API_KEY`, or Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS` or `gcloud auth application-default login`) with `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`.
- Cloudflare Workers AI and AI Gateway read `CLOUDFLARE_API_KEY` with `CLOUDFLARE_ACCOUNT_ID`, and `CLOUDFLARE_GATEWAY_ID` for the gateway.

`/login` stores these settings in `auth.json` instead, with Pi's prompts. With `ANTHROPIC_FEDERATION_RULE_ID` and the related variables set, Anthropic requests use workload identity federation instead of a key.

## Local models with llama.cpp

yapi supports the [llama.cpp](https://github.com/ggml-org/llama.cpp) router server, as Pi does. Start `llama-server` without `--model`, for example:

```sh
llama-server --models-dir ~/models --no-models-autoload --jinja --host 127.0.0.1 --port 8080
```

Then run `/login llama.cpp` and enter the server URL (default `http://127.0.0.1:8080`) and an optional API key, or set `LLAMA_BASE_URL` and `LLAMA_API_KEY`. `/llama` loads and unloads the router's models and downloads new ones from Hugging Face. Loaded models appear in `/model`, and every chat model is also listed as a classifier. Pi's [llama.cpp guide](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/llama-cpp.md) covers the server options and model layout.

To remove the provider and `/llama`, disable `llama.cpp` in `yapi config`, or add `"-builtin:llama.cpp"` to the `extensions` setting.

## Model catalogs

The catalog is built into yapi. Between releases, Pi publishes catalog updates for its providers. yapi fetches them for the providers you have configured: in the background at startup, when the model selector opens, after `/login`, and with `yapi update --models`. Fetched catalogs are kept in `~/.yapi/agent/models-store.json`, so later sessions have them offline. `--offline` or `PI_OFFLINE=1` turns fetching off. Providers that extensions register with `refreshModels` refresh at the same times.

## Classifier and image models

Besides chat models, the catalog lists classifiers, which answer typed questions about JSON data (TypeSafe's Jev, also through OpenCode, OpenRouter, Vercel AI Gateway and Cloudflare Workers AI, and models loaded in llama.cpp), and image models on OpenRouter. They run from codemode scripts through the `models` global, and from extensions through `ctx.modelRegistry.classify()` and `generateImages()`, with the session's credentials. See [MCP servers and codemode](mcp.md#codemode).

## Choosing a model

```sh
yapi --list-models sonnet            # search the catalog
yapi --model anthropic/claude-sonnet-4-5
yapi --model sonnet:high             # a pattern with a thinking level
yapi --models "sonnet,gpt-5*"        # the models Ctrl+P cycles through
```

Inside yapi, `/model` or Ctrl+L opens the model selector, Ctrl+P cycles through models, and Shift+Tab cycles the thinking level.

## Custom providers and models

Add providers, models and overrides to `~/.yapi/agent/models.json`. The format is Pi's, described in Pi's [models](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/models.md) and [custom provider](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/custom-provider.md) documentation. Extensions can register providers too, including ones that implement their own wire API. See [Providers from extensions](extensions.md#providers-from-extensions).

## Credentials for other tools

`yapi auth` gives scripts a ready credential without starting a session:

```sh
yapi auth check --provider anthropic --json
yapi auth print-api-key --provider openai
yapi auth print-bearer-token --provider openai-codex --min-expiry 30m
```
