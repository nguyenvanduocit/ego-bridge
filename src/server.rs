//! `ego-bridge serve`: an MCP server over Streamable HTTP that runs the host's
//! ego-browser on behalf of authenticated clients.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::audit::{AuditLog, AuditRecord, now_rfc3339};
use crate::config;
use crate::mirror::{Mirror, MirrorError};
use crate::preamble;
use crate::protocol::{FilePayload, HelpArgs, NodejsArgs, RunResult, ServerArgs};
use crate::runner::{self, EgoBrowser, Invocation};
use crate::tokens::TokenStore;

const INSTRUCTIONS: &str = "\
This server drives the ego lite browser on a remote host, exactly like the local `ego-browser` command.
1. Call `skill` once and follow the guide it returns. Wherever the guide says to run
   `ego-browser nodejs`, call the `nodejs` tool with the same script instead.
2. Absolute file paths in a script refer to your own machine. Send files the script reads or
   uploads in `files` (base64), and list output directories in `dirs`. Files the script writes
   come back in the result; images are also returned as image content.
3. `help` and `version` mirror `ego-browser help [topic]` and `ego-browser --version`.";

/// Tunables for `ego-bridge serve`.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub listen: SocketAddr,
    pub home: PathBuf,
    pub ego_browser: PathBuf,
    /// Extra `Host` header values to accept (for example the tailnet name).
    pub allowed_hosts: Vec<String>,
    pub max_concurrent_runs: usize,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_output_file_bytes: u64,
    pub max_stream_bytes: usize,
}

/// The authenticated client, placed in request extensions by the auth layer.
#[derive(Debug, Clone)]
pub struct ClientIdentity(pub String);

#[derive(Debug)]
struct Shared {
    ego: EgoBrowser,
    audit: AuditLog,
    runs_dir: PathBuf,
    runs: Semaphore,
    default_timeout: Duration,
    max_timeout: Duration,
    max_output_file_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Bridge {
    shared: Arc<Shared>,
    tool_router: ToolRouter<Self>,
}

fn client_name(parts: &http::request::Parts) -> String {
    parts
        .extensions
        .get::<ClientIdentity>()
        .map_or_else(|| "unknown".to_owned(), |identity| identity.0.clone())
}

fn invalid(err: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(err.to_string(), None)
}

fn internal(err: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}

fn image_mime(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, ext)| ext)?;
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Text for MCP clients plus the full result as structured content, which
/// the `ego-browser` client reads.
fn to_tool_result(result: &RunResult) -> Result<CallToolResult, ErrorData> {
    let mut text = result.stdout.clone();
    if !result.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&result.stderr);
    }
    if result.exit_code != 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[exit code {}]", result.exit_code));
    }
    let mut content = vec![ContentBlock::text(text)];
    for file in &result.files {
        let size = file.content_base64.len() / 4 * 3;
        content.push(ContentBlock::text(format!(
            "[file written] {} (~{size} bytes, see structuredContent.files)",
            file.path
        )));
        if let Some(mime) = image_mime(&file.path) {
            content.push(ContentBlock::image(file.content_base64.clone(), mime));
        }
    }
    let mut tool_result = if result.exit_code == 0 {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    tool_result.structured_content = Some(serde_json::to_value(result).map_err(internal)?);
    Ok(tool_result)
}

impl Bridge {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a command that needs no files, under the concurrency limit.
    async fn run_simple(
        &self,
        tool: &str,
        client: &str,
        server_name: Option<&str>,
        invocation: Result<Invocation, runner::RunError>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let outcome = async {
            let invocation = invocation.map_err(invalid)?;
            let _permit = self.shared.runs.acquire().await.map_err(internal)?;
            let output = self
                .shared
                .ego
                .run(&invocation, self.shared.default_timeout, cancel)
                .await
                .map_err(internal)?;
            Ok::<_, ErrorData>(RunResult {
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                files: Vec::new(),
            })
        }
        .await;
        let error = outcome.as_ref().err().map(|err| err.message.to_string());
        self.audit(AuditRecord {
            time: now_rfc3339(),
            client,
            tool,
            server_name,
            script: None,
            files_in: Vec::new(),
            files_out: Vec::new(),
            exit_code: outcome.as_ref().ok().map(|result| result.exit_code),
            duration_ms: started.elapsed().as_millis(),
            error: error.as_deref(),
        })
        .await;
        to_tool_result(&outcome?)
    }

    async fn audit(&self, record: AuditRecord<'_>) {
        if let Err(err) = self.shared.audit.append(&record).await {
            tracing::error!(%err, "write audit log");
        }
    }

    async fn run_script(
        &self,
        args: &NodejsArgs,
        cancel: &CancellationToken,
    ) -> Result<RunResult, ErrorData> {
        if args.timeout_ms == Some(0) {
            return Err(invalid("timeout_ms must be at least 1"));
        }
        // Take a run slot before writing anything, so queued requests do not
        // fill the disk with mirrors.
        let _permit = self.shared.runs.acquire().await.map_err(internal)?;
        let run_dir = self.shared.runs_dir.join(run_id());
        let result = self.run_in(&run_dir, args, cancel).await;
        match tokio::fs::remove_dir_all(&run_dir).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(%err, run_dir = %run_dir.display(), "remove run folder"),
        }
        result
    }

    /// Build the mirror, run the script against it, and collect what the
    /// script wrote.
    async fn run_in(
        &self,
        run_dir: &Path,
        args: &NodejsArgs,
        cancel: &CancellationToken,
    ) -> Result<RunResult, ErrorData> {
        let mirror_root = run_dir.join("fs");
        let (files, dirs) = (args.files.clone(), args.dirs.clone());
        let mirror =
            tokio::task::spawn_blocking(move || Mirror::create(mirror_root, &files, &dirs))
                .await
                .map_err(internal)?
                .map_err(mirror_error)?;

        // Static imports are linked before any statement runs, so such a
        // script is written to a module file and imported after the preamble.
        let as_module = runner::has_static_imports(&args.script)
            && !args.print
            && args.input_type.as_deref() != Some("commonjs");
        let module_file = as_module.then(|| {
            run_dir.join(if args.strip_types {
                "script.mts"
            } else {
                "script.mjs"
            })
        });
        if let Some(file) = &module_file {
            tokio::fs::write(file, &args.script)
                .await
                .map_err(internal)?;
        }
        let module_url = module_file.as_deref().map(runner::file_url);
        let source = preamble::render(mirror.root(), module_url.as_deref()).map_err(internal)?;
        let preamble_path = run_dir.join("preamble.mjs");
        tokio::fs::write(&preamble_path, source)
            .await
            .map_err(internal)?;
        let invocation =
            runner::nodejs(args, &preamble_path, module_file.as_deref()).map_err(invalid)?;
        let timeout = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.shared.default_timeout)
            .min(self.shared.max_timeout);

        let output = self
            .shared
            .ego
            .run(&invocation, timeout, cancel)
            .await
            .map_err(internal)?;

        let cap = self.shared.max_output_file_bytes;
        let changes = tokio::task::spawn_blocking(move || mirror.changes(cap))
            .await
            .map_err(internal)?
            .map_err(mirror_error)?;
        let mut stderr = output.stderr;
        if !changes.skipped.is_empty() {
            stderr.push_str(&format!(
                "ego-bridge: files not returned, over the {cap}-byte output limit: {}\n",
                changes.skipped.join(", ")
            ));
        }
        Ok(RunResult {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr,
            files: changes.files,
        })
    }
}

/// Bad caller input is the caller's error; disk trouble is ours.
fn mirror_error(err: MirrorError) -> ErrorData {
    match err {
        MirrorError::Io { .. } => internal(err),
        MirrorError::NotAbsolute(_)
        | MirrorError::NoFileName(_)
        | MirrorError::BadBase64 { .. } => invalid(err),
    }
}

fn run_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let suffix: u32 = rand::random();
    format!("{millis}-{suffix:08x}")
}

#[tool_router]
impl Bridge {
    #[tool(
        description = "Same as `ego-browser skill`: print the ego-browser usage guide for the host's ego lite version. Read it before your first `nodejs` call."
    )]
    async fn skill(
        &self,
        Parameters(args): Parameters<ServerArgs>,
        Extension(parts): Extension<http::request::Parts>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&parts);
        let server_name = args.server_name.as_deref();
        self.run_simple(
            "skill",
            &client,
            server_name,
            runner::skill(server_name),
            &context.ct,
        )
        .await
    }

    #[tool(description = "Same as `ego-browser help [topic]`.")]
    async fn help(
        &self,
        Parameters(args): Parameters<HelpArgs>,
        Extension(parts): Extension<http::request::Parts>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&parts);
        self.run_simple(
            "help",
            &client,
            args.server_name.as_deref(),
            runner::help(&args),
            &context.ct,
        )
        .await
    }

    #[tool(
        description = "Same as `ego-browser --version`: ego lite, Chromium and Node versions on the host."
    )]
    async fn version(
        &self,
        Parameters(args): Parameters<ServerArgs>,
        Extension(parts): Extension<http::request::Parts>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&parts);
        let server_name = args.server_name.as_deref();
        self.run_simple(
            "version",
            &client,
            server_name,
            runner::version(server_name),
            &context.ct,
        )
        .await
    }

    #[tool(
        description = "Same as `ego-browser nodejs`: run a browser script with the ego-browser API on the host. Absolute paths in the script mean paths on your machine: pass input files in `files`, output directories in `dirs`; written files come back in the result."
    )]
    async fn nodejs(
        &self,
        Parameters(args): Parameters<NodejsArgs>,
        Extension(parts): Extension<http::request::Parts>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&parts);
        let started = Instant::now();
        let outcome = self.run_script(&args, &context.ct).await;
        let error = outcome.as_ref().err().map(|err| err.message.to_string());
        let files_out: Vec<&str> = outcome
            .as_ref()
            .map(|result| result.files.iter().map(|file| file.path.as_str()).collect())
            .unwrap_or_default();
        self.audit(AuditRecord {
            time: now_rfc3339(),
            client: &client,
            tool: "nodejs",
            server_name: args.server_name.as_deref(),
            script: Some(&args.script),
            files_in: args
                .files
                .iter()
                .map(|file: &FilePayload| file.path.as_str())
                .collect(),
            files_out,
            exit_code: outcome.as_ref().ok().map(|result| result.exit_code),
            duration_ms: started.elapsed().as_millis(),
            error: error.as_deref(),
        })
        .await;
        to_tool_result(&outcome?)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ego-bridge", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

async fn require_token(
    State(tokens): State<TokenStore>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let Some(token) = presented else {
        return unauthorized();
    };
    let lookup = tokio::task::spawn_blocking(move || tokens.authenticate(&token)).await;
    match lookup {
        Ok(Ok(Some(name))) => {
            request.extensions_mut().insert(ClientIdentity(name));
            next.run(request).await
        }
        Ok(Ok(None)) => unauthorized(),
        Ok(Err(err)) => {
            tracing::error!(%err, "read token store");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(err) => {
            tracing::error!(%err, "token lookup task");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"ego-bridge\"")],
        "missing or invalid bearer token\n",
    )
        .into_response()
}

/// Build the HTTP app: `/mcp` behind bearer auth, `/healthz` open.
pub fn app(options: &ServeOptions, cancel: CancellationToken) -> std::io::Result<axum::Router> {
    std::fs::create_dir_all(&options.home)?;
    config::restrict_dir(&options.home)?;
    let runs_dir = config::runs_dir(&options.home);
    // Leftovers from a crash are never needed again.
    match std::fs::remove_dir_all(&runs_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    std::fs::create_dir_all(&runs_dir)?;
    // Canonical so `realpath` inside scripts maps back to caller paths.
    let runs_dir = std::fs::canonicalize(&runs_dir)?;

    let shared = Arc::new(Shared {
        ego: EgoBrowser::new(options.ego_browser.clone(), options.max_stream_bytes),
        audit: AuditLog::new(config::audit_path(&options.home)),
        runs_dir,
        runs: Semaphore::new(options.max_concurrent_runs.max(1)),
        default_timeout: options.default_timeout,
        max_timeout: options.max_timeout,
        max_output_file_bytes: options.max_output_file_bytes,
    });

    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    hosts.extend(options.allowed_hosts.iter().cloned());
    let mcp_config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        // Stateless: every call stands alone, and a client that disconnects
        // cancels its run (rmcp only cancels on disconnect without sessions).
        .with_legacy_session_mode(false)
        .with_cancellation_token(cancel.child_token());
    let mut mcp_config = mcp_config;
    mcp_config.max_request_body_bytes = options.max_request_bytes;

    let factory_shared = Arc::clone(&shared);
    let mcp = StreamableHttpService::new(
        move || Ok(Bridge::new(Arc::clone(&factory_shared))),
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );
    let tokens = TokenStore::new(config::tokens_path(&options.home));
    let protected = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(tokens, require_token));
    Ok(axum::Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(protected))
}

/// Serve until Ctrl-C or SIGTERM.
pub async fn serve(options: ServeOptions) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    let router = app(&options, cancel.clone())?;
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    tracing::info!(
        listen = %listener.local_addr()?,
        ego_browser = %options.ego_browser.display(),
        home = %options.home.display(),
        "ego-bridge serving MCP at /mcp"
    );
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        shutdown.cancel();
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel.cancelled_owned().await })
        .await?;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!(%err, "install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
