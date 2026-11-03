//! The drop-in `ego-browser` client. It accepts the same command line as the
//! real ego-browser, runs the command on the bridge host, and moves files so
//! absolute paths in scripts keep meaning paths on this machine.

use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::ClientInitializeError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClientTransportConfig,
};

use crate::config::ClientConfig;
use crate::protocol::{
    FilePayload, HelpArgs, NodejsArgs, RunResult, ServerArgs, TOOL_HELP, TOOL_NODEJS, TOOL_SKILL,
    TOOL_VERSION,
};
use crate::scan::absolute_path_literals;

/// Exit code for problems in ego-bridge itself (config, network, auth).
pub const BRIDGE_ERROR_EXIT_CODE: i32 = 1;
/// Largest single input file the client will send.
pub const MAX_INPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest total of input files per run.
pub const MAX_INPUT_TOTAL_BYTES: u64 = 96 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Inline(String),
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Skill,
    Version,
    Help(Option<String>),
    Nodejs {
        source: Source,
        print: bool,
        strip_types: bool,
        input_type: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub server_name: Option<String>,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error(
        "`ego-browser {0}` changes the host's browser and is not available through ego-bridge; run it on the host"
    )]
    HostOnly(String),
    #[error("`--sdk-path` is not available through ego-bridge")]
    SdkPath,
    #[error("Unknown command: {0}")]
    UnknownCommand(String),
    #[error("{0} requires a value")]
    MissingValue(String),
    #[error("error: `ego-browser nodejs` accepts at most one source argument")]
    TooManySources,
    #[error("unknown option for nodejs: {0}")]
    UnknownOption(String),
}

/// Parse an ego-browser command line (without the program name).
pub fn parse(args: &[String]) -> Result<Parsed, ParseError> {
    let mut server_name = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut iter = args.iter().map(String::as_str).peekable();
    // Global options come before the command, as in ego-browser.
    while let Some(arg) = iter.peek().copied() {
        if let Some(name) = arg.strip_prefix("--ego-server-name=") {
            server_name = Some(name.to_owned());
            iter.next();
        } else if arg == "--ego-server-name" {
            iter.next();
            let value = iter
                .next()
                .ok_or_else(|| ParseError::MissingValue(arg.to_owned()))?;
            server_name = Some(value.to_owned());
        } else {
            break;
        }
    }
    rest.extend(iter);

    let command = match rest.split_first() {
        None => Command::Help(None),
        Some((&("--help" | "-h"), _)) => Command::Help(None),
        Some((&("--version" | "-v"), _)) => Command::Version,
        Some((&"help", topic)) => Command::Help(topic.first().map(|topic| (*topic).to_owned())),
        Some((&"skill", _)) => Command::Skill,
        Some((&"nodejs", options)) => parse_nodejs(options)?,
        Some((&name @ ("import" | "upgrade"), _)) => {
            return Err(ParseError::HostOnly(name.to_owned()));
        }
        Some((other, _)) => return Err(ParseError::UnknownCommand((*other).to_owned())),
    };
    Ok(Parsed {
        server_name,
        command,
    })
}

fn parse_nodejs(options: &[&str]) -> Result<Command, ParseError> {
    let mut source: Option<Source> = None;
    let mut print = false;
    let mut strip_types = false;
    let mut input_type = None;
    let mut set_source = |script: &str| {
        if source.is_some() {
            return Err(ParseError::TooManySources);
        }
        source = Some(Source::Inline(script.to_owned()));
        Ok(())
    };
    let mut iter = options.iter().copied();
    while let Some(arg) = iter.next() {
        match arg {
            "--help" | "-h" => return Ok(Command::Help(Some("nodejs".to_owned()))),
            "-e" | "--eval" => {
                let script = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue(arg.to_owned()))?;
                set_source(script)?;
            }
            "-p" | "--print" => print = true,
            "--strip-types" => strip_types = true,
            "--input-type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue(arg.to_owned()))?;
                input_type = Some(value.to_owned());
            }
            "--sdk-path" => return Err(ParseError::SdkPath),
            _ if arg.starts_with("--input-type=") => {
                input_type = Some(arg.trim_start_matches("--input-type=").to_owned());
            }
            _ if arg.starts_with("--eval=") => set_source(arg.trim_start_matches("--eval="))?,
            _ if arg.starts_with("--sdk-path=") => return Err(ParseError::SdkPath),
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(ParseError::UnknownOption(arg.to_owned()));
            }
            script => set_source(script)?,
        }
    }
    Ok(Command::Nodejs {
        source: source.unwrap_or(Source::Stdin),
        print,
        strip_types,
        input_type,
    })
}

/// Files and directories named in the script that exist on this machine.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Attachments {
    pub files: Vec<FilePayload>,
    pub dirs: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("{path} is {size} bytes; ego-bridge sends files up to {MAX_INPUT_FILE_BYTES} bytes")]
    FileTooLarge { path: String, size: u64 },
    #[error("input files total more than {MAX_INPUT_TOTAL_BYTES} bytes")]
    TotalTooLarge,
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
}

/// Collect what a script needs on the host: every named file that exists
/// here, every named directory, and the parent directory of every named
/// path that does not exist yet (so the script can create it).
pub fn collect_attachments(script: &str) -> Result<Attachments, AttachError> {
    let mut attachments = Attachments::default();
    let mut total: u64 = 0;
    let add_dir = |dirs: &mut Vec<String>, dir: &str| {
        if !dirs.iter().any(|existing| existing == dir) {
            dirs.push(dir.to_owned());
        }
    };
    for path in absolute_path_literals(script) {
        let Ok(meta) = fs::metadata(&path) else {
            if let Some(parent) = Path::new(&path).parent().and_then(Path::to_str)
                && parent != "/"
                && Path::new(parent).is_dir()
            {
                add_dir(&mut attachments.dirs, parent);
            }
            continue;
        };
        if meta.is_dir() {
            if path != "/" {
                add_dir(&mut attachments.dirs, &path);
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_INPUT_FILE_BYTES {
            return Err(AttachError::FileTooLarge {
                path,
                size: meta.len(),
            });
        }
        total += meta.len();
        if total > MAX_INPUT_TOTAL_BYTES {
            return Err(AttachError::TotalTooLarge);
        }
        let bytes = fs::read(&path).map_err(|source| AttachError::Read {
            path: path.clone(),
            source,
        })?;
        attachments.files.push(FilePayload {
            path,
            content_base64: BASE64.encode(bytes),
        });
    }
    Ok(attachments)
}

/// Write files returned by the host to their paths on this machine.
pub fn write_returned_files(files: &[FilePayload]) -> Vec<String> {
    let mut errors = Vec::new();
    for file in files {
        let written = BASE64
            .decode(&file.content_base64)
            .map_err(|err| err.to_string())
            .and_then(|bytes| {
                if let Some(parent) = Path::new(&file.path).parent() {
                    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
                }
                fs::write(&file.path, bytes).map_err(|err| err.to_string())
            });
        if let Err(err) = written {
            errors.push(format!("ego-bridge: could not write {}: {err}", file.path));
        }
    }
    errors
}

fn to_arguments<T: serde::Serialize>(args: &T) -> anyhow::Result<rmcp::model::JsonObject> {
    match serde_json::to_value(args)? {
        serde_json::Value::Object(map) => Ok(map),
        other => anyhow::bail!("tool arguments must be an object, got {other}"),
    }
}

/// Whether the bridge answered 401: a missing, wrong or revoked token.
fn rejected_token(err: &ClientInitializeError) -> bool {
    let ClientInitializeError::TransportError { error, .. } = err else {
        return false;
    };
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error.error.as_ref());
    while let Some(error) = current {
        if error.downcast_ref::<AuthRequiredError>().is_some() {
            return true;
        }
        current = error.source();
    }
    false
}

/// Call one bridge tool and return its structured result.
pub async fn call(
    config: &ClientConfig,
    tool: &'static str,
    arguments: rmcp::model::JsonObject,
) -> anyhow::Result<RunResult> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(config.url.as_str())
            .auth_header(config.token.as_str()),
    );
    let client = ().serve(transport).await.map_err(|err| {
        if rejected_token(&err) {
            anyhow::anyhow!(
                "{} rejected the token; check `ego-bridge connect` or {}",
                config.url,
                crate::config::TOKEN_ENV
            )
        } else {
            anyhow::anyhow!("connect to {}: {err}", config.url)
        }
    })?;
    let response = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await;
    let _ = client.cancel().await;
    let result = response.map_err(|err| anyhow::anyhow!("{tool} failed: {err}"))?;
    let structured = result
        .structured_content
        .ok_or_else(|| anyhow::anyhow!("{tool}: bridge returned no structured result"))?;
    Ok(serde_json::from_value(structured)?)
}

fn read_stdin_script() -> anyhow::Result<String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        anyhow::bail!(
            "the interactive REPL is not available through ego-bridge; pass a script with -e or on stdin"
        );
    }
    let mut script = String::new();
    stdin.read_to_string(&mut script)?;
    Ok(script)
}

/// Run an ego-browser command line against the bridge; returns the exit code.
pub async fn run(args: &[String], config: &ClientConfig) -> anyhow::Result<i32> {
    let parsed = parse(args)?;
    let server_name = parsed.server_name;
    let result = match parsed.command {
        Command::Skill => {
            call(
                config,
                TOOL_SKILL,
                to_arguments(&ServerArgs { server_name })?,
            )
            .await?
        }
        Command::Version => {
            call(
                config,
                TOOL_VERSION,
                to_arguments(&ServerArgs { server_name })?,
            )
            .await?
        }
        Command::Help(topic) => {
            call(
                config,
                TOOL_HELP,
                to_arguments(&HelpArgs { topic, server_name })?,
            )
            .await?
        }
        Command::Nodejs {
            source,
            print,
            strip_types,
            input_type,
        } => {
            let script = match source {
                Source::Inline(script) => script,
                Source::Stdin => read_stdin_script()?,
            };
            let attachments = collect_attachments(&script)?;
            let args = NodejsArgs {
                script,
                server_name,
                print,
                strip_types,
                input_type,
                files: attachments.files,
                dirs: attachments.dirs,
                timeout_ms: None,
            };
            call(config, TOOL_NODEJS, to_arguments(&args)?).await?
        }
    };

    let write_errors = write_returned_files(&result.files);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(result.stdout.as_bytes())?;
    stdout.flush()?;
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(result.stderr.as_bytes())?;
    for error in &write_errors {
        writeln!(stderr, "{error}")?;
    }
    if result.exit_code == 0 && !write_errors.is_empty() {
        return Ok(BRIDGE_ERROR_EXIT_CODE);
    }
    Ok(result.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn nodejs(source: Source) -> Command {
        Command::Nodejs {
            source,
            print: false,
            strip_types: false,
            input_type: None,
        }
    }

    #[test]
    fn parses_the_ego_browser_command_line() -> Result<(), ParseError> {
        assert_eq!(parse(&argv(&[]))?.command, Command::Help(None));
        assert_eq!(parse(&argv(&["--help"]))?.command, Command::Help(None));
        assert_eq!(parse(&argv(&["-v"]))?.command, Command::Version);
        assert_eq!(parse(&argv(&["skill"]))?.command, Command::Skill);
        assert_eq!(
            parse(&argv(&["help", "nodejs"]))?.command,
            Command::Help(Some("nodejs".to_owned()))
        );
        assert_eq!(parse(&argv(&["nodejs"]))?.command, nodejs(Source::Stdin));
        assert_eq!(
            parse(&argv(&["nodejs", "-e", "1+1"]))?.command,
            nodejs(Source::Inline("1+1".to_owned()))
        );
        assert_eq!(
            parse(&argv(&["nodejs", "console.log(1)"]))?.command,
            nodejs(Source::Inline("console.log(1)".to_owned()))
        );
        assert_eq!(
            parse(&argv(&["nodejs", "--help"]))?.command,
            Command::Help(Some("nodejs".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn parses_global_server_name_and_nodejs_flags() -> Result<(), ParseError> {
        let parsed = parse(&argv(&[
            "--ego-server-name=app-a",
            "nodejs",
            "--print",
            "--strip-types",
            "--input-type=module",
            "--eval",
            "x",
        ]))?;
        assert_eq!(parsed.server_name.as_deref(), Some("app-a"));
        assert_eq!(
            parsed.command,
            Command::Nodejs {
                source: Source::Inline("x".to_owned()),
                print: true,
                strip_types: true,
                input_type: Some("module".to_owned()),
            }
        );
        let spaced = parse(&argv(&["--ego-server-name", "b", "skill"]))?;
        assert_eq!(spaced.server_name.as_deref(), Some("b"));
        Ok(())
    }

    #[test]
    fn refuses_host_only_and_unsupported_commands() {
        assert_eq!(
            parse(&argv(&["upgrade"])),
            Err(ParseError::HostOnly("upgrade".to_owned()))
        );
        assert_eq!(
            parse(&argv(&["import", "list"])),
            Err(ParseError::HostOnly("import".to_owned()))
        );
        assert_eq!(
            parse(&argv(&["nodejs", "--sdk-path", "x.js", "-e", "1"])),
            Err(ParseError::SdkPath)
        );
        assert_eq!(
            parse(&argv(&["nodejs", "-e", "1", "2"])),
            Err(ParseError::TooManySources)
        );
        assert_eq!(
            parse(&argv(&["frobnicate"])),
            Err(ParseError::UnknownCommand("frobnicate".to_owned()))
        );
    }

    #[test]
    fn attaches_existing_files_dirs_and_parents_of_new_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let input = root.join("in.pdf");
        fs::write(&input, b"%PDF")?;
        let out_dir = root.join("out");
        fs::create_dir(&out_dir)?;
        let script = format!(
            r#"await page.setInputFiles("input", "{}");
               await page.screenshot({{ path: "{}/shot.png" }});
               await page.goto("/not/a/local/thing");"#,
            input.display(),
            out_dir.display()
        );
        let attachments = collect_attachments(&script)?;
        assert_eq!(attachments.files.len(), 1);
        assert_eq!(attachments.files[0].path, input.to_string_lossy());
        assert_eq!(attachments.files[0].content_base64, BASE64.encode(b"%PDF"));
        assert_eq!(
            attachments.dirs,
            vec![out_dir.to_string_lossy().into_owned()]
        );
        Ok(())
    }

    #[test]
    fn writes_returned_files_creating_parents() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("deep/dir/shot.png");
        let errors = write_returned_files(&[FilePayload {
            path: target.to_string_lossy().into_owned(),
            content_base64: BASE64.encode(b"png"),
        }]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read(&target)?, b"png");
        Ok(())
    }
}
