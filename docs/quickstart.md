# Quickstart

## Connect a model

Set an API key for your provider:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
```

You can also start yapi and run `/login` to sign in with a Claude, ChatGPT or GitHub Copilot subscription, or to save an API key. [Models and sign-in](models.md) lists every provider.

## Start a session

Start yapi in the folder you want it to work in:

```sh
cd ~/code/my-project
yapi
```

Type a task and press Enter. yapi works with four tools by default: `read`, `bash`, `edit` and `write`. Each tool call appears in the transcript as it runs. Press Escape to interrupt the agent and Ctrl+C twice to quit.

## First tasks

```text
Explain how requests flow through src/server.rs
Find why `cargo test` fails and fix it
@Cargo.toml Which dependencies are outdated?
!git status
```

`@path` attaches a file to the message. `!command` runs a shell command and shows its output to the model, and `!!command` runs one without sending the output.

## Next steps

- [Interactive mode](interactive.md) covers keys and slash commands.
- [Sessions](sessions.md) explains how to continue and branch work.
- [Configuration](configuration.md) covers settings and project instructions in `AGENTS.md`.
