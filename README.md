# ego-bridge

Use one machine's ego lite browser (Citro Labs) from agents on other machines.

ego-bridge is an independent project, not affiliated with or endorsed by Citro Labs.

ego lite runs on one Mac. Its `ego-browser` command talks to the app over macOS
Mach IPC, which only works on that machine. `ego-bridge` puts an authenticated
MCP server (Streamable HTTP) in front of it, and ships a drop-in `ego-browser`
client for the other machines. Agents there keep typing the same commands and
writing the same scripts, file paths included.

```
 other machine (any network)                          host Mac (ego lite)
┌───────────────────────────┐                    ┌────────────────────────────────┐
│ agent                     │   HTTPS + token    │ ego-bridge serve (LaunchAgent) │
│  └ ego-browser  (client) ─┼──── MCP /mcp ─────▶│  └ ego-browser ── Mach IPC ──▶ │ ego lite
│  └ or any MCP client      │                    │     (per-run file mirror)      │
└───────────────────────────┘                    └────────────────────────────────┘
```

## What stays the same

| You run on the client | What happens |
|---|---|
| `ego-browser skill` | The host's guide, matching the host's ego lite version |
| `ego-browser nodejs -e '…'`, `nodejs < file.js`, heredoc, positional script | Runs on the host; stdout, stderr and the exit code come back unchanged |
| `nodejs --print`, `--strip-types`, `--input-type=…` | Passed through |
| `ego-browser help [topic]`, `--version`, `--ego-server-name=<name>` | Passed through |
| Absolute paths in scripts (`node:fs`, `page.screenshot({ path })`, `setInputFiles`, `FileChooser.setFiles`, `download.saveAs`, `page.fetch(…, { saveAs })`) | Refer to **the client's** filesystem (see below) |

What is different:

- `ego-browser import …` and `ego-browser upgrade` are refused. They change the
  host's browser, so run them on the host.
- The interactive REPL and `nodejs --sdk-path` are not available.
- Output arrives when the script ends, not while it runs.
- `localhost` in a script means the host, not the client.
- A script with static `import`/`export` declarations runs as a module file on
  the host, so stack traces name that file instead of `[eval]` (line numbers
  are unchanged).
- Processes a script leaves running in the background are stopped when the
  script ends. Stopping the client (Ctrl-C) stops the run on the host.

### How file paths work

Each run gets a fresh mirror folder on the host that stands in for the client's
filesystem. The client scans the script for absolute path literals:

- files that exist locally are sent along;
- directories that exist locally are created empty in the mirror (so writes into
  them succeed);
- the parent directory of a path that does not exist yet is created too.

On the host, a small preamble maps every absolute path the script uses through
`node:fs`, `node:fs/promises` and the ego APIs above into the mirror, and maps
paths reported back (`realpath`, `mkdtemp`, error messages) to client paths.
After the run, every file the script created or changed comes back and is
written to the same path on the client. The mirror is then deleted.

Limits: a file the script *reads* through a path assembled at run time (not
written out in the script) is not sent. Neither are directory contents
(`readdir` of a client folder sees only what was sent). Writes always come
back, however the path was built.

## Install

Download a release archive for your platform from
[Releases](../../releases) (macOS arm64/x86_64, Linux x86_64/arm64 static), or
build from source:

```sh
cargo install --git https://github.com/nguyenvanduocit/ego-bridge --locked
```

The same `ego-bridge` binary is the server and the client.

## Host setup (the Mac with ego lite)

```sh
# 1. One token per client machine. It is printed once; only its hash is stored.
ego-bridge token add pod-a

# 2. Run the server as a LaunchAgent (starts at login, restarts if it dies).
#    It must run inside your GUI session to reach ego lite, which the agent does.
ego-bridge service install --allowed-host mac.your-tailnet.ts.net
```

`serve` listens on `127.0.0.1:8787` only. Expose it in one of these ways:

| Clients can join your tailnet? | Expose with | MCP URL |
|---|---|---|
| Yes (recommended: nothing is public) | `tailscale serve --bg 8787` | `https://<host>.<tailnet>.ts.net/mcp` |
| No | `tailscale funnel --bg 8787` | same URL, reachable from the internet |
| No, prefer Cloudflare | a Cloudflare Tunnel to `http://127.0.0.1:8787` (add Cloudflare Access for a second factor) | `https://<your-hostname>/mcp` |

Pass the public hostname with `--allowed-host` (repeatable, or
`EGO_BRIDGE_ALLOWED_HOSTS=a,b`). Requests whose `Host` header is not
`localhost`, `127.0.0.1`, `::1` or an allowed host are rejected, which blocks
DNS-rebinding attacks.

To run in the foreground instead: `ego-bridge serve --allowed-host …`.

## Client setup (each other machine)

```sh
ego-bridge connect --url https://mac.your-tailnet.ts.net/mcp --token egb_…
ln -s "$(command -v ego-bridge)" ~/.local/bin/ego-browser   # any dir early on PATH
ego-browser --version    # prints the host's ego lite version
```

`connect` checks the URL and token, then saves them to
`~/.config/ego-bridge/client.json` (mode 600). `EGO_BRIDGE_URL` and
`EGO_BRIDGE_TOKEN` override the file. `ego-bridge client <args>` is the same as
running the `ego-browser` link.

Now install the regular ego-browser agent skill on that machine. It tells agents
to run `ego-browser skill`, and everything after that works unchanged.

### From an MCP client instead

Point any Streamable HTTP MCP client at the URL with an
`Authorization: Bearer <token>` header. For Claude Code:

```sh
claude mcp add --transport http ego-bridge https://mac.your-tailnet.ts.net/mcp \
  --header "Authorization: Bearer egb_…"
```

Tools: `skill`, `help`, `version`, `nodejs`. `nodejs` takes the script plus
optional `files` (`[{ path, content_base64 }]`) and `dirs`. Returned files are in
`structuredContent.files`; images are also returned as image content, so the
model can look at screenshots directly.

## Security model

**A token is a shell on the host.** `nodejs` runs arbitrary Node.js code as
your user on the host, inside your logged-in browser, with every session it
holds. The path mirror is a compatibility feature, not a sandbox. Treat tokens
like SSH keys:

- give each machine its own token, and keep tokens in a password manager;
- prefer tailnet-only exposure (`tailscale serve`) over public exposure;
- consider a dedicated ego lite profile that is not signed in to your bank or
  primary email.

What the bridge does enforce:

- bearer-token auth on `/mcp`, per client, revocable instantly
  (`ego-bridge token revoke <name>`; the token file is re-read on every request);
- only SHA-256 digests of tokens are stored (`tokens.json`, mode 600);
- `Host` header allow-list;
- callers never pass raw arguments to `ego-browser`; arguments are built from
  typed fields, so `import`/`upgrade` cannot be reached, and server names are
  validated;
- limits on request size, concurrent runs, run time, output size;
- an audit log of every call: client, tool, script, files in and out, exit
  code, duration (`audit.jsonl`, mode 600; it contains scripts, so treat it
  as sensitive).

Kill switch: `ego-bridge service uninstall` (or `tailscale serve reset` /
`tailscale funnel reset`) cuts every client off at once.

## Operations

Everything lives in `~/.config/ego-bridge` (override with `EGO_BRIDGE_HOME`):

| File | Purpose |
|---|---|
| `tokens.json` | client names and token digests (host) |
| `audit.jsonl` | one JSON line per call (host) |
| `serve.log` | LaunchAgent stdout/stderr (host) |
| `runs/` | per-run mirrors, deleted after each run and on start (host) |
| `client.json` | URL and token (client) |

```sh
ego-bridge token list
ego-bridge token revoke pod-a
tail -f ~/.config/ego-bridge/audit.jsonl
launchctl print gui/$(id -u)/io.github.nguyenvanduocit.ego-bridge   # service state
curl -s http://127.0.0.1:8787/healthz                              # liveness, no auth
```

### `serve` options

| Flag | Default | |
|---|---|---|
| `--listen` | `127.0.0.1:8787` | also `EGO_BRIDGE_LISTEN` |
| `--ego-browser` | `ego-browser` on PATH, then `~/.local/bin/ego-browser` | also `EGO_BROWSER_BIN` |
| `--allowed-host` | none | repeatable; also `EGO_BRIDGE_ALLOWED_HOSTS` |
| `--max-concurrent-runs` | 4 | |
| `--default-timeout` | 10m | runs that exceed it exit with 124 |
| `--max-timeout` | 60m | cap for the `timeout_ms` a caller asks for |
| `--max-request-mib` | 160 | script plus base64 input files |
| `--max-output-mib` | 64 | total size of returned files per run |
| `--max-stream-mib` | 16 | stdout/stderr kept per run |

The client sends at most 64 MiB per file and 96 MiB in total.

## Development

```sh
cargo test        # unit tests, plus end-to-end tests with a fake ego-browser (needs Node 22.15+)
cargo clippy --all-targets -- -D warnings
```

The end-to-end tests start a real server and drive the real client binary; the
fake `ego-browser` runs scripts in plain Node, so the JavaScript preamble's path
mapping is exercised for real. Design notes are in [docs/design.md](docs/design.md).

## License

MIT
