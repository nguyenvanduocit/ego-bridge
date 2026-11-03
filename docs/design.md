# ego-bridge design

## Intent

ego lite runs on one Mac, and its `ego-browser` CLI reaches the app over Mach
IPC (`bootstrap_look_up`), which cannot leave that machine. Agents on other
machines, often on other networks, should be able to use that browser with as
little difference as possible, so they need no extra instructions.

## Goals

- One authenticated endpoint on the host that other machines reach over HTTPS.
- Agents keep using the ego-browser skill and command line unchanged.
- Absolute file paths in scripts keep meaning paths on the agent's machine.
- Each client can be cut off on its own; every call is recorded.

## Non-goals

- Isolation between clients. All tokens have the same power; a token is a
  shell on the host.
- Reaching the client's `localhost` from the host browser.
- Streaming output while a script runs.
- OAuth sign-in for web MCP clients (bearer tokens only).

## Shape

One Rust binary, `ego-bridge`:

- `serve`: axum + rmcp Streamable HTTP server at `/mcp`, bearer auth layer,
  `/healthz`. Runs as a LaunchAgent in the Aqua session (Mach IPC to ego lite
  only works from the logged-in GUI session).
- client: invoked as `ego-browser` (argv[0]) or `ego-bridge client`. Parses the
  ego-browser command line, calls the matching MCP tool, writes returned files,
  replays stdout/stderr and the exit code.
- MCP tools mirror the CLI: `skill`, `help`, `version`, `nodejs`. The server
  builds `ego-browser` arguments from typed fields only, so host-changing
  commands (`import`, `upgrade`) are unreachable.

## File mirroring

Per run, the host creates `runs/<id>/fs/` (the mirror) and maps caller path
`/a/b` to `runs/<id>/fs/a/b`.

1. Client: scan the script for absolute path literals. Send existing files,
   create existing directories, and create parents of paths that do not exist
   yet.
2. Host: prepend a loader to the script's first line:
   `process.getBuiltinModule("node:module").createRequire("/")("<preamble>");`
   - Not `--sdk-path`: that flag replaces ego's own SDK, so `taskSpace` and
     friends disappear.
   - Not `await import()`: top-level await makes Node treat the input as ESM,
     which breaks `--print`.
   - Same line, so error line numbers match the caller's script. A hashbang
     line and a leading `"use strict"` directive stay in front of it.
   - Scripts with static `import`/`export` declarations are different: Node
     resolves and links static imports before any statement runs, so the hook
     would come too late. Such a script is written to `runs/<id>/script.mjs`
     (`.mts` with `--strip-types`) and stdin becomes
     `<loader>await import("<file url>");`. The preamble treats that module URL
     as the caller's script.
3. Preamble (`src/preamble.mjs`):
   - `module.registerHooks` redirects `fs` / `fs/promises` imports made by the
     caller's script (parent URL ends in `/[eval<n>]` or `/[stdin<n>]`) to
     wrappers that map path arguments in and returned paths and errors out. ego's
     SDK keeps the real fs.
   - Patches the shared `Page`, `FileChooser` and `Download` prototypes for the
     path-taking APIs (`screenshot`, `setInputFiles`, `fetch` `saveAs`,
     `setFiles`, `saveAs`), found through the `taskSpace` /
     `claimTaskSpace` / `takeOverTaskSpace` globals.
4. Host: return every new or changed file (by SHA-256 against what was
   uploaded), within the output cap, then delete the run folder.

## Run lifecycle

- A run slot (`--max-concurrent-runs`) is taken before the mirror is written,
  so queued requests cost no disk.
- `ego-browser` runs as the leader of its own process group. When the run ends
  (exit, time limit, or cancellation) the whole group is killed with
  `killpg`, so background processes cannot outlive it or hold its pipes. Output
  is then read for at most two more seconds, for processes that left the group.
- The MCP endpoint is stateless (no sessions): rmcp cancels a request when its
  client disconnects only in that mode, and the run is cancelled with it
  (exit code 130).
- Output files are size-checked before they are read, so the output cap also
  bounds memory.

## Verified behaviour (ego lite 0.6.2.5, macOS)

- `console.log`, `--version` and `help` output goes to stderr in the real CLI,
  and is passed through on the same streams.
- With the real file hidden on the host, `setInputFiles` and
  `FileChooser.setFiles` still upload the mirror copy; screenshots, downloads,
  `fetch` `saveAs` and `fs` writes never touch the real host path and arrive
  on the client.
- Through `tailscale serve`, requests are rejected until the tailnet hostname
  is passed with `--allowed-host`.
- Static `import { writeFileSync } from "node:fs"`, a hashbang, `--strip-types`
  and a `"use strict"` CommonJS script all behave as with the local CLI.
- Killing the client mid-run stops the host run (exit 130).

## Upgrade risk

A future ego lite release may add an API that takes a file path. It works but
is not mirrored until it is added to the preamble. Re-run the live check in
the README after each ego lite upgrade.
