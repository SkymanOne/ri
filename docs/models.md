# Models and sign-in

ri supports the same providers and models as pi, from the same built-in catalog.

## API keys

ri reads the same environment variables as pi. Common ones:

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

`ri --help` lists every provider, including Azure OpenAI, Amazon Bedrock, Cloudflare and Vercel AI Gateway.

To store a key instead, run `/login` inside ri and choose "Sign in with an API key". ri saves it to `~/.ri/agent/auth.json` in pi's format.

## Subscriptions

Run `/login` and choose "Sign in with an account" to use a Claude Pro or Max, ChatGPT Plus or Pro, or GitHub Copilot subscription. ri opens the browser sign-in and refreshes the token when it expires.

`/logout` removes a stored credential.

## Choosing a model

```sh
ri --list-models sonnet            # search the catalog
ri --model anthropic/claude-sonnet-4-5
ri --model sonnet:high             # a pattern with a thinking level
ri --models "sonnet,gpt-5*"        # the models Ctrl+P cycles through
```

Inside ri, `/model` or Ctrl+L opens the model selector, Ctrl+P cycles through models, and Shift+Tab cycles the thinking level.

## Custom providers and models

Add providers, models and overrides to `~/.ri/agent/models.json`. The format is pi's, described in pi's [models](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/models.md) and [custom provider](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/custom-provider.md) documentation.

## Credentials for other tools

`ri auth` gives scripts a ready credential without starting a session:

```sh
ri auth check --provider anthropic --json
ri auth print-api-key --provider openai
ri auth print-bearer-token --provider openai-codex --min-expiry 30m
```
