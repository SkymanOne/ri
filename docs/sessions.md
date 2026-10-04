# Sessions

ri saves every conversation as a JSONL file under `~/.ri/agent/sessions/`, grouped by project. The format is pi's session format, so pi and ri open each other's sessions.

## Continuing work

```sh
ri -c                      # continue the last session in this project
ri -r                      # pick a session to resume
ri --session 3f2a          # open a session by file or id prefix
ri --fork 3f2a             # copy an existing session into a new one
ri --no-session            # do not save this run
```

Inside ri, `/resume` opens the session picker and `/new` starts a fresh session.

## Branches

A session is a tree. Going back to an earlier message keeps the abandoned branch, so no work is lost.

- `/tree` shows every branch and jumps to any earlier point. Escape twice on an empty editor opens it too.
- `/fork` starts a new session from an earlier message.
- `/clone` duplicates the current session.

## Compaction

Long sessions fill the model's context window. ri summarizes older messages automatically before that happens, as pi does. `/compact` runs it on demand, and `/settings` turns automatic compaction off.

## Export

`/export` writes the session to a standalone HTML file, and `ri --export <session>` does the same from the command line.
