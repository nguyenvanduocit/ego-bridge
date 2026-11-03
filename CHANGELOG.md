# Changelog

## v0.2.0

- Scripts with static `import`/`export` now get mapped file paths too:
  `import { writeFileSync } from "node:fs"` previously reached the host's real
  filesystem.
- A hashbang line and a leading `"use strict"` directive keep working.
- Stopping the client (Ctrl-C, lost connection) cancels the run on the host.
  The MCP endpoint is now stateless.
- Every process a run started is killed when it ends (with `killpg`, no
  external `kill`), so a background process can no longer hang a run.
- Output files are size-checked before being read; a run slot is taken before
  input files are written; `timeout_ms: 0` is rejected; time-limit messages
  show the exact limit.
- `service install` pins `EGO_BRIDGE_HOME` in the LaunchAgent, so the service
  reads the same tokens as the CLI that installed it.

## v0.1.0

First release.

- `ego-bridge serve`: MCP server over Streamable HTTP with per-client bearer
  tokens, a Host allow-list, an audit log, concurrency, time and size limits.
- Tools mirror the ego-browser CLI: `skill`, `help`, `version`, `nodejs`.
- Drop-in `ego-browser` client (link the binary as `ego-browser`) with the same
  command line, streams and exit codes.
- Transparent file mirroring: absolute paths in scripts refer to the client's
  filesystem for `node:fs`, screenshots, uploads, file choosers, downloads and
  `fetch` `saveAs`.
- `ego-bridge service install` runs the server as a macOS LaunchAgent in the
  GUI session.
