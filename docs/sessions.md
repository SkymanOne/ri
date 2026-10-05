# Sessions

yapi saves every conversation as a JSONL file under `~/.yapi/agent/sessions/`, grouped by project. The format is Pi's session format, so Pi and yapi open each other's sessions.

## Continuing work

```sh
yapi -c                      # continue the last session in this project
yapi -r                      # pick a session to resume
yapi --session 3f2a          # open a session by file or id prefix
yapi --fork 3f2a             # copy an existing session into a new one
yapi --no-session            # do not save this run
```

Inside yapi, `/resume` opens the session picker and `/new` starts a fresh session.

## Branches

A session is a tree. Going back to an earlier message keeps the abandoned branch, so no work is lost.

- `/tree` shows every branch and jumps to any earlier point. Escape twice on an empty editor opens it too.
- `/fork` starts a new session from an earlier message.
- `/clone` duplicates the current session.

## Compaction

Long sessions fill the model's context window. yapi summarizes older messages automatically before that happens, as Pi does. `/compact` runs it on demand, and `/settings` turns automatic compaction off.

## Export

`/export` writes the session to a standalone HTML file, and `yapi --export <session>` does the same from the command line.
