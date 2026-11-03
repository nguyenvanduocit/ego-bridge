# Changelog

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
