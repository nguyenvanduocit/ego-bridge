//! Spawn the host's `ego-browser` with arguments built from typed tool input.
//! Callers never supply raw arguments, so `import` and `upgrade` cannot be
//! reached through the bridge.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::protocol::{HelpArgs, NodejsArgs};

/// Exit code reported when a run hits its time limit, as `timeout(1)` does.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("invalid server name {0:?}: use up to 64 ASCII letters, digits, '-', '_' or '.'")]
    InvalidServerName(String),
    #[error("start {bin}: {source}")]
    Spawn {
        bin: PathBuf,
        source: std::io::Error,
    },
    #[error("talk to ego-browser: {0}")]
    Io(#[from] std::io::Error),
}

/// Arguments and stdin for one `ego-browser` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

fn server_flag(server_name: Option<&str>) -> Result<Option<String>, RunError> {
    let Some(name) = server_name else {
        return Ok(None);
    };
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(RunError::InvalidServerName(name.to_owned()));
    }
    Ok(Some(format!("--ego-server-name={name}")))
}

fn with_server(server_name: Option<&str>, rest: &[&str]) -> Result<Vec<String>, RunError> {
    let mut args: Vec<String> = server_flag(server_name)?.into_iter().collect();
    args.extend(rest.iter().map(|arg| (*arg).to_owned()));
    Ok(args)
}

pub fn skill(server_name: Option<&str>) -> Result<Invocation, RunError> {
    Ok(Invocation {
        args: with_server(server_name, &["skill"])?,
        stdin: None,
    })
}

pub fn version(server_name: Option<&str>) -> Result<Invocation, RunError> {
    Ok(Invocation {
        args: with_server(server_name, &["--version"])?,
        stdin: None,
    })
}

pub fn help(args: &HelpArgs) -> Result<Invocation, RunError> {
    let mut rest = vec!["help"];
    if let Some(topic) = args.topic.as_deref() {
        rest.push(topic);
    }
    Ok(Invocation {
        args: with_server(args.server_name.as_deref(), &rest)?,
        stdin: None,
    })
}

/// Statement that loads the preamble.
///
/// `--sdk-path` cannot carry the preamble because it replaces ego's own SDK.
/// A synchronous `require` (Node loads ESM through it) works in module,
/// CommonJS and `--print` modes alike.
pub fn preamble_loader(preamble: &Path) -> Result<String, serde_json::Error> {
    let path = serde_json::to_string(&preamble.to_string_lossy())?;
    Ok(format!(
        "process.getBuiltinModule(\"node:module\").createRequire(\"/\")({path});"
    ))
}

/// Put `loader` in front of the script's code without moving any line: after
/// a hashbang line and after a leading `"use strict"` directive, both of which
/// must stay first to keep their meaning.
pub fn with_loader(script: &str, loader: &str) -> String {
    let (hashbang, body) = if script.starts_with("#!") {
        match script.find('\n') {
            Some(end) => script.split_at(end + 1),
            None => (script, ""),
        }
    } else {
        ("", script)
    };
    let newline = if !hashbang.is_empty() && !hashbang.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let (directive, rest) = split_use_strict(body);
    let separator = if !directive.is_empty() && !directive.ends_with(';') {
        ";"
    } else {
        ""
    };
    format!("{hashbang}{newline}{directive}{separator}{loader}{rest}")
}

fn split_use_strict(body: &str) -> (&str, &str) {
    let start = body.len() - body.trim_start().len();
    for literal in ["\"use strict\"", "'use strict'"] {
        if body[start..].starts_with(literal) {
            let mut end = start + literal.len();
            if body[end..].starts_with(';') {
                end += 1;
            }
            return body.split_at(end);
        }
    }
    ("", body)
}

/// Whether the script has static `import`/`export` declarations. Node links
/// those before any statement runs, so the preamble's import hook would come
/// too late; such scripts are loaded as a module file after the preamble.
pub fn has_static_imports(script: &str) -> bool {
    script.lines().any(|line| {
        let line = line.trim_start();
        let import = line
            .strip_prefix("import")
            .is_some_and(|rest| rest.starts_with([' ', '\t', '{', '*', '"', '\'']));
        import || line.starts_with("export ") || line.starts_with("export{")
    })
}

/// `file://` URL for an absolute path.
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// Build the `ego-browser nodejs` call. The script goes through stdin, which
/// ego-browser accepts up to 64 MiB and which avoids command-line limits.
///
/// With `module_file`, the caller has written the script there and stdin only
/// loads the preamble and then imports that file.
pub fn nodejs(
    args: &NodejsArgs,
    preamble: &Path,
    module_file: Option<&Path>,
) -> Result<Invocation, RunError> {
    let loader = preamble_loader(preamble).map_err(|err| RunError::Io(err.into()))?;
    let mut argv = with_server(args.server_name.as_deref(), &["nodejs"])?;
    if args.print {
        argv.push("--print".to_owned());
    }
    if args.strip_types {
        argv.push("--strip-types".to_owned());
    }
    if let Some(input_type) = args.input_type.as_deref() {
        argv.push(format!("--input-type={input_type}"));
    }
    let stdin = match module_file {
        Some(file) => {
            let url =
                serde_json::to_string(&file_url(file)).map_err(|err| RunError::Io(err.into()))?;
            format!("{loader}await import({url});")
        }
        None => with_loader(&args.script, &loader),
    };
    Ok(Invocation {
        args: argv,
        stdin: Some(stdin),
    })
}

/// How long to keep reading output after the run ended, for processes that
/// left the run's process group but still hold its stdout or stderr.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// Exit code reported when the caller cancelled the run, as for Ctrl-C.
pub const CANCELLED_EXIT_CODE: i32 = 130;

#[derive(Debug, Clone)]
pub struct EgoBrowser {
    bin: PathBuf,
    max_stream_bytes: usize,
}

enum Ended {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

impl EgoBrowser {
    pub fn new(bin: PathBuf, max_stream_bytes: usize) -> Self {
        Self {
            bin,
            max_stream_bytes,
        }
    }

    /// Run `invocation` until it exits, hits `timeout`, or `cancel` fires.
    /// Every process left in the run's process group is killed when it ends.
    pub async fn run(
        &self,
        invocation: &Invocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Output, RunError> {
        let mut command = Command::new(&self.bin);
        command
            .args(&invocation.args)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| RunError::Spawn {
            bin: self.bin.clone(),
            source,
        })?;
        // The group id outlives the leader, so keep it for after `wait`.
        let group = child.id();

        let stdin_task = match (child.stdin.take(), invocation.stdin.clone()) {
            (Some(mut pipe), Some(text)) => Some(tokio::spawn(async move {
                pipe.write_all(text.as_bytes()).await?;
                pipe.shutdown().await
            })),
            _ => None,
        };
        let stdout = Arc::new(Mutex::new(Capped::new(self.max_stream_bytes)));
        let stderr = Arc::new(Mutex::new(Capped::new(self.max_stream_bytes)));
        let mut readers = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            readers.push(tokio::spawn(pump(pipe, Arc::clone(&stdout))));
        }
        if let Some(pipe) = child.stderr.take() {
            readers.push(tokio::spawn(pump(pipe, Arc::clone(&stderr))));
        }

        let ended = tokio::select! {
            status = child.wait() => Ended::Exited(status?),
            () = tokio::time::sleep(timeout) => Ended::TimedOut,
            () = cancel.cancelled() => Ended::Cancelled,
        };
        kill_group(group);
        if !matches!(ended, Ended::Exited(_)) {
            // Also reaps the child.
            let _ = child.kill().await;
        }
        if let Some(task) = stdin_task {
            // A script that exits early closes stdin; that is not an error.
            task.abort();
        }
        let drained = tokio::time::timeout(PIPE_GRACE, async {
            for reader in &mut readers {
                let _ = reader.await;
            }
        })
        .await
        .is_ok();
        for reader in &readers {
            reader.abort();
        }

        let stdout = take_text(&stdout);
        let mut stderr = take_text(&stderr);
        let mut note = |line: String| {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&line);
        };
        if !drained {
            note("ego-bridge: a detached process kept the output open; later output is not included\n".to_owned());
        }
        let exit_code = match ended {
            Ended::Exited(status) => exit_code(status),
            Ended::TimedOut => {
                note(format!(
                    "ego-bridge: run stopped after its {} time limit\n",
                    humantime::format_duration(timeout)
                ));
                TIMEOUT_EXIT_CODE
            }
            Ended::Cancelled => {
                note("ego-bridge: run cancelled by the caller\n".to_owned());
                CANCELLED_EXIT_CODE
            }
        };
        Ok(Output {
            exit_code,
            stdout,
            stderr,
            timed_out: matches!(ended, Ended::TimedOut),
        })
    }
}

/// Kill every process in the group led by `leader`. Errors mean the group is
/// already gone.
#[cfg(unix)]
fn kill_group(leader: Option<u32>) {
    let pid = leader
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw);
    if let Some(pid) = pid {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_leader: Option<u32>) {}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// Output kept up to a byte cap, counting what was dropped.
#[derive(Debug)]
struct Capped {
    kept: Vec<u8>,
    dropped: usize,
    cap: usize,
}

impl Capped {
    fn new(cap: usize) -> Self {
        Self {
            kept: Vec::new(),
            dropped: 0,
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let take = self.cap.saturating_sub(self.kept.len()).min(chunk.len());
        self.kept.extend_from_slice(&chunk[..take]);
        self.dropped += chunk.len() - take;
    }

    fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.kept).into_owned();
        if self.dropped > 0 {
            text.push_str(&format!(
                "\n[ego-bridge: output truncated, {} bytes dropped]\n",
                self.dropped
            ));
        }
        text
    }
}

fn take_text(buffer: &Mutex<Capped>) -> String {
    buffer.lock().unwrap_or_else(PoisonError::into_inner).text()
}

/// Copy a stream into `buffer` until it ends.
async fn pump(
    mut reader: impl AsyncRead + Unpin,
    buffer: Arc<Mutex<Capped>>,
) -> std::io::Result<()> {
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADER: &str = "L;";

    #[test]
    fn builds_cli_arguments_like_ego_browser() -> Result<(), RunError> {
        assert_eq!(skill(None)?.args, vec!["skill"]);
        assert_eq!(
            version(Some("app-a"))?.args,
            vec!["--ego-server-name=app-a", "--version"]
        );
        let help_args = HelpArgs {
            topic: Some("nodejs".to_owned()),
            server_name: None,
        };
        assert_eq!(help(&help_args)?.args, vec!["help", "nodejs"]);

        let run = NodejsArgs {
            script: "console.log(1)".to_owned(),
            print: true,
            input_type: Some("module".to_owned()),
            ..NodejsArgs::default()
        };
        let invocation = nodejs(&run, Path::new("/runs/1/preamble.mjs"), None)?;
        assert_eq!(
            invocation.args,
            vec!["nodejs", "--print", "--input-type=module"]
        );
        // The loader shares line 1 with the script so line numbers match.
        assert_eq!(
            invocation.stdin.as_deref(),
            Some(
                "process.getBuiltinModule(\"node:module\").createRequire(\"/\")(\"/runs/1/preamble.mjs\");console.log(1)"
            )
        );
        Ok(())
    }

    #[test]
    fn module_scripts_are_imported_after_the_preamble() -> Result<(), RunError> {
        let run = NodejsArgs {
            script: "import fs from \"node:fs\";".to_owned(),
            ..NodejsArgs::default()
        };
        let invocation = nodejs(
            &run,
            Path::new("/runs/1/preamble.mjs"),
            Some(Path::new("/runs/1/my script.mjs")),
        )?;
        let stdin = invocation.stdin.unwrap_or_default();
        assert!(
            stdin.ends_with("await import(\"file:///runs/1/my%20script.mjs\");"),
            "{stdin}"
        );
        assert!(!stdin.contains("node:fs"));
        Ok(())
    }

    #[test]
    fn loader_keeps_hashbang_and_use_strict_first() {
        assert_eq!(with_loader("a()\nb()", LOADER), "L;a()\nb()");
        assert_eq!(
            with_loader("#!/usr/bin/env node\na()", LOADER),
            "#!/usr/bin/env node\nL;a()"
        );
        assert_eq!(
            with_loader("#!/usr/bin/env node", LOADER),
            "#!/usr/bin/env node\nL;"
        );
        assert_eq!(
            with_loader("\"use strict\";\na()", LOADER),
            "\"use strict\";L;\na()"
        );
        assert_eq!(
            with_loader("'use strict'\na()", LOADER),
            "'use strict';L;\na()"
        );
        assert_eq!(
            with_loader("#!/bin/node\n  'use strict';x", LOADER),
            "#!/bin/node\n  'use strict';L;x"
        );
    }

    #[test]
    fn detects_static_imports_and_exports_only() {
        assert!(has_static_imports("import fs from \"node:fs\";"));
        assert!(has_static_imports("const a = 1;\n  import { x } from 'y';"));
        assert!(has_static_imports("import * as p from \"node:path\""));
        assert!(has_static_imports("import \"side-effect\";"));
        assert!(has_static_imports("export const a = 1;"));
        assert!(!has_static_imports("const fs = await import(\"node:fs\");"));
        assert!(!has_static_imports("console.log(import.meta.url)"));
        assert!(!has_static_imports("const important = 1;"));
    }

    #[test]
    fn rejects_server_names_that_could_smuggle_arguments() {
        for bad in ["", "a b", "x --version", "a/b", &"x".repeat(65)] {
            assert!(
                matches!(skill(Some(bad)), Err(RunError::InvalidServerName(_))),
                "{bad:?}"
            );
        }
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Invocation {
        Invocation {
            args: vec!["-c".to_owned(), script.to_owned()],
            stdin: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_output_exit_code_and_stdin() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 1024);
        let invocation = Invocation {
            stdin: Some("hello".to_owned()),
            ..sh("cat; echo err >&2; exit 3")
        };
        let output = ego
            .run(
                &invocation,
                Duration::from_secs(10),
                &CancellationToken::new(),
            )
            .await?;
        assert_eq!(output.exit_code, 3);
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "err\n");
        assert!(!output.timed_out);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stops_runs_that_exceed_the_time_limit() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 1024);
        // The background `sleep` is a second process in the group holding
        // stdout open: the whole group must die, not only the shell.
        let started = std::time::Instant::now();
        let output = ego
            .run(
                &sh("sleep 30 & sleep 30"),
                Duration::from_millis(1500),
                &CancellationToken::new(),
            )
            .await?;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(output.timed_out);
        assert_eq!(output.exit_code, TIMEOUT_EXIT_CODE);
        assert!(
            output.stderr.contains("after its 1s 500ms time limit"),
            "{}",
            output.stderr
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_processes_do_not_outlive_a_finished_run()
    -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 1024);
        let started = std::time::Instant::now();
        let output = ego
            .run(
                &sh("sleep 30 & echo done"),
                Duration::from_secs(60),
                &CancellationToken::new(),
            )
            .await?;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "done\n");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_stops_the_run() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 1024);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let output = ego
            .run(&sh("sleep 30"), Duration::from_secs(60), &cancel)
            .await?;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output.exit_code, CANCELLED_EXIT_CODE);
        assert!(output.stderr.contains("cancelled"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truncates_oversized_output() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 10);
        let output = ego
            .run(
                &sh("printf 'abcdefghijklmnop'"),
                Duration::from_secs(10),
                &CancellationToken::new(),
            )
            .await?;
        assert!(
            output
                .stdout
                .starts_with("abcdefghij\n[ego-bridge: output truncated, 6 bytes")
        );
        Ok(())
    }
}
