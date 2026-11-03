//! Spawn the host's `ego-browser` with arguments built from typed tool input.
//! Callers never supply raw arguments, so `import` and `upgrade` cannot be
//! reached through the bridge.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

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

/// Statement that loads the preamble, put on the script's first line.
///
/// `--sdk-path` cannot carry the preamble because it replaces ego's own SDK.
/// A synchronous `require` (Node loads ESM through it) works in module,
/// CommonJS and `--print` modes alike, and keeps the script's line numbers.
pub fn preamble_loader(preamble: &Path) -> Result<String, serde_json::Error> {
    let path = serde_json::to_string(&preamble.to_string_lossy())?;
    Ok(format!(
        "process.getBuiltinModule(\"node:module\").createRequire(\"/\")({path});"
    ))
}

/// The script goes through stdin, which ego-browser accepts up to 64 MiB and
/// which avoids command-line length limits.
pub fn nodejs(args: &NodejsArgs, preamble: &Path) -> Result<Invocation, RunError> {
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
    Ok(Invocation {
        args: argv,
        stdin: Some(format!("{loader}{}", args.script)),
    })
}

#[derive(Debug, Clone)]
pub struct EgoBrowser {
    bin: PathBuf,
    max_stream_bytes: usize,
}

impl EgoBrowser {
    pub fn new(bin: PathBuf, max_stream_bytes: usize) -> Self {
        Self {
            bin,
            max_stream_bytes,
        }
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub async fn run(
        &self,
        invocation: &Invocation,
        timeout: Duration,
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

        let stdin_task = match (child.stdin.take(), invocation.stdin.clone()) {
            (Some(mut pipe), Some(text)) => Some(tokio::spawn(async move {
                pipe.write_all(text.as_bytes()).await?;
                pipe.shutdown().await
            })),
            _ => None,
        };
        let stdout = child
            .stdout
            .take()
            .map(|pipe| tokio::spawn(read_capped(pipe, self.max_stream_bytes)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(read_capped(pipe, self.max_stream_bytes)));

        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => (Some(status?), false),
            Err(_) => {
                kill_group(&mut child).await;
                (None, true)
            }
        };
        if let Some(task) = stdin_task {
            // A script that exits early closes stdin; that is not an error.
            let _ = task.await;
        }
        let stdout = join_stream(stdout).await?;
        let mut stderr = join_stream(stderr).await?;
        if timed_out {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&format!(
                "ego-bridge: run stopped after {}s time limit\n",
                timeout.as_secs()
            ));
        }
        Ok(Output {
            exit_code: status.map_or(TIMEOUT_EXIT_CODE, exit_code),
            stdout,
            stderr,
            timed_out,
        })
    }
}

#[cfg(unix)]
async fn kill_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Signal the whole group so helpers ego-browser started die with it.
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(not(unix))]
async fn kill_group(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

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

async fn join_stream(
    task: Option<tokio::task::JoinHandle<std::io::Result<String>>>,
) -> Result<String, RunError> {
    match task {
        Some(task) => task
            .await
            .map_err(|err| RunError::Io(std::io::Error::other(err)))?
            .map_err(RunError::Io),
        None => Ok(String::new()),
    }
}

/// Read a stream to the end, keeping at most `cap` bytes.
async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> std::io::Result<String> {
    let mut kept = Vec::new();
    let mut dropped: usize = 0;
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let room = cap.saturating_sub(kept.len());
        let take = room.min(read);
        kept.extend_from_slice(&buf[..take]);
        dropped += read - take;
    }
    let mut text = String::from_utf8_lossy(&kept).into_owned();
    if dropped > 0 {
        text.push_str(&format!(
            "\n[ego-bridge: output truncated, {dropped} bytes dropped]\n"
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let invocation = nodejs(&run, Path::new("/runs/1/preamble.mjs"))?;
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
    fn rejects_server_names_that_could_smuggle_arguments() {
        for bad in ["", "a b", "x --version", "a/b", &"x".repeat(65)] {
            assert!(
                matches!(skill(Some(bad)), Err(RunError::InvalidServerName(_))),
                "{bad:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_output_exit_code_and_stdin() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 1024);
        let invocation = Invocation {
            args: vec!["-c".to_owned(), "cat; echo err >&2; exit 3".to_owned()],
            stdin: Some("hello".to_owned()),
        };
        let output = ego.run(&invocation, Duration::from_secs(10)).await?;
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
        let invocation = Invocation {
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            stdin: None,
        };
        let started = std::time::Instant::now();
        let output = ego.run(&invocation, Duration::from_millis(300)).await?;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(output.timed_out);
        assert_eq!(output.exit_code, TIMEOUT_EXIT_CODE);
        assert!(output.stderr.contains("time limit"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truncates_oversized_output() -> Result<(), Box<dyn std::error::Error>> {
        let ego = EgoBrowser::new(PathBuf::from("/bin/sh"), 10);
        let invocation = Invocation {
            args: vec!["-c".to_owned(), "printf 'abcdefghijklmnop'".to_owned()],
            stdin: None,
        };
        let output = ego.run(&invocation, Duration::from_secs(10)).await?;
        assert!(
            output
                .stdout
                .starts_with("abcdefghij\n[ego-bridge: output truncated, 6 bytes")
        );
        Ok(())
    }
}
