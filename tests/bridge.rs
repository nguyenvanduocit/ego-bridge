//! End-to-end tests: a real `ego-bridge serve` app and the real client binary,
//! with a fake `ego-browser` that runs scripts in plain Node. Node 22.15+ must
//! be on PATH; the preamble's path mapping runs for real.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use ego_bridge::server::{ServeOptions, app};
use ego_bridge::tokens::TokenStore;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const FAKE_EGO_BROWSER: &str = r#"#!/bin/sh
case "$1" in --ego-server-name=*) echo "server=${1#--ego-server-name=}" >&2; shift;; esac
case "$1" in
  --version) echo "ego-browser 0.0.0-fake" >&2 ;;
  skill) echo "fake guide" ;;
  help) echo "help:${2:-}" >&2 ;;
  nodejs) shift; exec node --input-type=module - ;;
  *) echo "fake: unexpected $*" >&2; exit 64 ;;
esac
"#;

struct Bridge {
    url: String,
    token: String,
    cancel: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn write_fake_ego_browser(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("ego-browser");
    std::fs::write(&path, FAKE_EGO_BROWSER)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

async fn start_bridge(default_timeout: Duration) -> Result<Bridge, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("host");
    let ego_browser = write_fake_ego_browser(dir.path())?;
    let token = TokenStore::new(home.join("tokens.json")).add("test-client")?;
    let options = ServeOptions {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        home,
        ego_browser,
        allowed_hosts: Vec::new(),
        max_concurrent_runs: 2,
        default_timeout,
        max_timeout: default_timeout,
        max_request_bytes: 32 * 1024 * 1024,
        max_output_file_bytes: 16 * 1024 * 1024,
        max_stream_bytes: 1024 * 1024,
    };
    let cancel = CancellationToken::new();
    let router = app(&options, cancel.clone())?;
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    let addr = listener.local_addr()?;
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    Ok(Bridge {
        url: format!("http://{addr}/mcp"),
        token,
        cancel,
        _dir: dir,
    })
}

struct ClientRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the real binary as the `ego-browser` client.
async fn client(
    bridge: &Bridge,
    token: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<ClientRun, Box<dyn std::error::Error>> {
    let client_home = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ego-bridge"))
        .arg("client")
        .args(args)
        .env("EGO_BRIDGE_HOME", client_home.path())
        .env("EGO_BRIDGE_URL", &bridge.url)
        .env("EGO_BRIDGE_TOKEN", token)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(mut pipe), Some(text)) = (child.stdin.take(), stdin) {
        pipe.write_all(text.as_bytes()).await?;
        pipe.shutdown().await?;
    }
    let output = child.wait_with_output().await?;
    Ok(ClientRun {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

#[tokio::test]
async fn mcp_endpoint_requires_a_valid_token() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    let http = reqwest::Client::new();
    let health = http
        .get(bridge.url.replace("/mcp", "/healthz"))
        .send()
        .await?;
    assert_eq!(health.status(), 200);

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    for auth in [None, Some("Bearer egb_wrong"), Some("Basic abc")] {
        let mut request = http
            .post(&bridge.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(init);
        if let Some(value) = auth {
            request = request.header("authorization", value);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), 401, "auth {auth:?}");
        assert!(response.headers().contains_key("www-authenticate"));
    }

    let run = client(&bridge, "egb_wrong", &["--version"], None).await?;
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("rejected the token"), "{}", run.stderr);
    Ok(())
}

#[tokio::test]
async fn simple_commands_keep_ego_browser_streams() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    let version = client(&bridge, &bridge.token, &["--version"], None).await?;
    assert_eq!(version.code, Some(0));
    assert_eq!(version.stdout, "");
    assert_eq!(version.stderr, "ego-browser 0.0.0-fake\n");

    let skill = client(
        &bridge,
        &bridge.token,
        &["--ego-server-name=app-a", "skill"],
        None,
    )
    .await?;
    assert_eq!(skill.stdout, "fake guide\n");
    assert_eq!(skill.stderr, "server=app-a\n");

    let help = client(&bridge, &bridge.token, &["help", "nodejs"], None).await?;
    assert_eq!(help.stderr, "help:nodejs\n");
    Ok(())
}

#[tokio::test]
async fn host_only_commands_are_refused_without_contacting_the_host() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    for command in ["upgrade", "import"] {
        let run = client(&bridge, &bridge.token, &[command], None).await?;
        assert_eq!(run.code, Some(1));
        assert!(
            run.stderr.contains("not available through ego-bridge"),
            "{}",
            run.stderr
        );
    }
    Ok(())
}

#[tokio::test]
async fn scripts_use_caller_paths_and_files_round_trip() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    let work = tempfile::tempdir()?;
    let work = work.path().canonicalize()?;
    let input = work.join("in.txt");
    let hidden = work.join("in.txt.hidden");
    let out_dir = work.join("out");
    std::fs::write(&input, "hello from the client")?;
    std::fs::create_dir(&out_dir)?;
    let output = out_dir.join("nested/result.txt");

    // The script hides the real input with the host's own fs, so reading it
    // only works through the mirror. It also proves writes do not land on
    // the real host path before the client copies them back.
    let script = format!(
        r#"
const real = process.getBuiltinModule("node:fs");
const fs = await import("node:fs/promises");
real.renameSync("{input}", "{hidden}");
try {{
  const text = await fs.readFile("{input}", "utf8");
  await fs.mkdir("{out_dir}/nested", {{ recursive: true }});
  await fs.writeFile("{output}", text.toUpperCase());
  const realpath = await fs.realpath("{output}");
  let missing = null;
  try {{ await fs.readFile("{out_dir}/missing.txt"); }} catch (err) {{ missing = err.path; }}
  console.log(JSON.stringify({{
    text,
    realpath,
    missing,
    directOnHost: real.existsSync("{output}"),
  }}));
}} finally {{
  real.renameSync("{hidden}", "{input}");
}}
"#,
        input = input.display(),
        hidden = hidden.display(),
        out_dir = out_dir.display(),
        output = output.display(),
    );
    let run = client(&bridge, &bridge.token, &["nodejs"], Some(&script)).await?;
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    let report: serde_json::Value = serde_json::from_str(run.stdout.trim())?;
    assert_eq!(report["text"], "hello from the client");
    assert_eq!(report["realpath"], output.to_string_lossy().as_ref());
    assert_eq!(
        report["missing"],
        format!("{}/missing.txt", out_dir.display())
    );
    assert_eq!(report["directOnHost"], false);
    assert_eq!(std::fs::read_to_string(&output)?, "HELLO FROM THE CLIENT");
    Ok(())
}

#[tokio::test]
async fn static_fs_imports_use_caller_paths() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    let work = tempfile::tempdir()?;
    let work = work.path().canonicalize()?;
    let input = work.join("in.txt");
    let hidden = work.join("in.txt.hidden");
    let output = work.join("out.txt");
    std::fs::write(&input, "static")?;

    // Static imports are linked before any statement runs; they must still
    // get the mapped fs. The real input is hidden, so only the mirror has it.
    let script = format!(
        r#"import {{ readFileSync, writeFileSync, existsSync }} from "node:fs";
import {{ readFile }} from "node:fs/promises";
const real = process.getBuiltinModule("node:fs");
real.renameSync("{input}", "{hidden}");
try {{
  writeFileSync("{output}", readFileSync("{input}", "utf8") + (await readFile("{input}", "utf8")));
  console.log(JSON.stringify({{ seen: existsSync("{input}"), direct: real.existsSync("{output}") }}));
}} finally {{
  real.renameSync("{hidden}", "{input}");
}}
"#,
        input = input.display(),
        hidden = hidden.display(),
        output = output.display(),
    );
    let run = client(&bridge, &bridge.token, &["nodejs"], Some(&script)).await?;
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.stdout.trim(), r#"{"seen":true,"direct":false}"#);
    assert_eq!(std::fs::read_to_string(&output)?, "staticstatic");
    Ok(())
}

#[tokio::test]
async fn exit_codes_and_inline_scripts_pass_through() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(30)).await?;
    let run = client(
        &bridge,
        &bridge.token,
        &["nodejs", "-e", "console.log('hi'); process.exit(7)"],
        None,
    )
    .await?;
    assert_eq!(run.code, Some(7));
    assert_eq!(run.stdout, "hi\n");

    let thrown = client(
        &bridge,
        &bridge.token,
        &["nodejs", "-e", "throw new Error('boom')"],
        None,
    )
    .await?;
    assert_eq!(thrown.code, Some(1));
    assert!(thrown.stderr.contains("boom"), "{}", thrown.stderr);
    Ok(())
}

#[tokio::test]
async fn a_client_that_disconnects_cancels_its_run() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(60)).await?;
    let work = tempfile::tempdir()?;
    let pid_file = work.path().join("pid");
    let script = format!(
        r#"process.getBuiltinModule("node:fs").writeFileSync("{}", String(process.pid));
await new Promise((r) => setTimeout(r, 60_000));"#,
        pid_file.display()
    );
    let client_home = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ego-bridge"))
        .args(["client", "nodejs", "-e", &script])
        .env("EGO_BRIDGE_HOME", client_home.path())
        .env("EGO_BRIDGE_URL", &bridge.url)
        .env("EGO_BRIDGE_TOKEN", &bridge.token)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut pid = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            pid = text.trim().parse::<i32>().ok();
            if pid.is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = pid.ok_or("the script never started")?;
    child.kill().await?;

    let alive = |pid: i32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let mut stopped = false;
    for _ in 0..50 {
        if !alive(pid) {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        stopped,
        "host run {pid} kept going after the client disconnected"
    );
    Ok(())
}

#[tokio::test]
async fn runs_past_the_time_limit_are_stopped() -> TestResult {
    let bridge = start_bridge(Duration::from_secs(2)).await?;
    let run = client(
        &bridge,
        &bridge.token,
        &[
            "nodejs",
            "-e",
            "await new Promise((r) => setTimeout(r, 60_000))",
        ],
        None,
    )
    .await?;
    assert_eq!(run.code, Some(124));
    assert!(run.stderr.contains("time limit"), "{}", run.stderr);
    Ok(())
}
