# Security policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's [private vulnerability reporting](https://github.com/SkymanOne/yapi/security/advisories/new) for this repository. Do not open a public issue.

Include what is affected, how to reproduce it, the output of `yapi --version` and your operating system.

## Security model

yapi is a coding agent. It reads and edits files and runs commands as your user, so give it the same trust as any program you run in your shell.

- Extensions run in WebAssembly instances with memory and compute limits. In v0.1 every package gets Pi's default grants, which allow file, process, network and environment access, so an extension can do what it could do in Pi. Restricting a package's grants is planned.
- MCP servers started over stdio run as normal processes with your permissions.
- Codemode scripts run in a fresh instance with no file, process or network access. They act only through the tools they call.
- Project trust gates project-local resources. yapi loads nothing from a project's `.yapi` folder or `.agents/skills`, such as settings, extensions, skills and MCP servers, until you trust the project. `AGENTS.md` and `CLAUDE.md` load whether or not the project is trusted.
- Files and tool output that the model reads can steer it. Work in repositories you trust, or run yapi in a container or virtual machine.
