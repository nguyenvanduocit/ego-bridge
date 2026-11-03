use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ego_bridge::config::{self, ClientConfig};
use ego_bridge::protocol::{ServerArgs, TOOL_VERSION};
use ego_bridge::server::{self, ServeOptions};
use ego_bridge::tokens::TokenStore;
use ego_bridge::{client, service};

const MIB: usize = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "ego-bridge",
    version,
    about = "Share this machine's ego lite browser with agents on other machines over MCP.",
    long_about = "Share this machine's ego lite browser with agents on other machines over MCP.\n\n\
        On the host: `ego-bridge token add <name>`, then `ego-bridge serve` (or `service install`).\n\
        On a client: `ego-bridge connect --url <url>/mcp --token <token>`, then link this binary as\n\
        `ego-browser` and use it exactly like the real one."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Serve the MCP endpoint at /mcp (host side).
    Serve(ServeArgs),
    /// Manage client tokens (host side).
    #[command(subcommand)]
    Token(TokenCommand),
    /// Save the bridge URL and token for this client and check they work.
    Connect {
        /// MCP endpoint, for example https://mac.example.ts.net/mcp
        #[arg(long)]
        url: String,
        #[arg(long)]
        token: String,
    },
    /// Run an ego-browser command line through the bridge (client side).
    /// Linking this binary as `ego-browser` does the same.
    Client {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install or remove the macOS LaunchAgent that keeps `serve` running.
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, Clone, clap::Args)]
struct ServeArgs {
    /// Address to listen on. Keep it on loopback and expose it with
    /// `tailscale serve`, Tailscale Funnel or a Cloudflare Tunnel.
    #[arg(long, env = "EGO_BRIDGE_LISTEN", default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// The host's ego-browser. Default: `ego-browser` on PATH, then
    /// ~/.local/bin/ego-browser.
    #[arg(long, env = "EGO_BROWSER_BIN")]
    ego_browser: Option<PathBuf>,
    /// Extra Host header to accept, such as `mac.example.ts.net`. Repeatable.
    #[arg(
        long = "allowed-host",
        env = "EGO_BRIDGE_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,
    /// Scripts that may run at the same time.
    #[arg(long, default_value_t = 4)]
    max_concurrent_runs: usize,
    /// Time limit for a run that does not ask for one.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    default_timeout: Duration,
    /// Upper bound for any run's time limit.
    #[arg(long, default_value = "60m", value_parser = humantime::parse_duration)]
    max_timeout: Duration,
    /// Largest accepted request in MiB (scripts plus base64 input files).
    #[arg(long, default_value_t = 160)]
    max_request_mib: usize,
    /// Largest total of output files returned per run, in MiB.
    #[arg(long, default_value_t = 64)]
    max_output_mib: usize,
    /// Largest stdout or stderr kept per run, in MiB.
    #[arg(long, default_value_t = 16)]
    max_stream_mib: usize,
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Create a token for a client. It is printed once and never stored.
    Add { name: String },
    /// List clients.
    List,
    /// Revoke a client's token. Takes effect on its next request.
    Revoke { name: String },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Write and start the LaunchAgent with these `serve` options.
    Install(ServeArgs),
    /// Stop and remove the LaunchAgent.
    Uninstall,
}

fn find_ego_browser() -> anyhow::Result<PathBuf> {
    let me = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok());
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("ego-browser"));
    let local =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin/ego-browser"));
    on_path
        .chain(local)
        .filter(|candidate| candidate.is_file())
        // Never pick this binary when it is linked as `ego-browser`.
        .find(|candidate| candidate.canonicalize().ok() != me)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot find the host's ego-browser; install ego lite or pass --ego-browser <path>"
            )
        })
}

fn serve_options(args: &ServeArgs, home: PathBuf) -> anyhow::Result<ServeOptions> {
    let ego_browser = match &args.ego_browser {
        Some(path) => path.clone(),
        None => find_ego_browser()?,
    };
    Ok(ServeOptions {
        listen: args.listen,
        home,
        ego_browser,
        allowed_hosts: args.allowed_hosts.clone(),
        max_concurrent_runs: args.max_concurrent_runs,
        default_timeout: args.default_timeout,
        max_timeout: args.max_timeout,
        max_request_bytes: args.max_request_mib.saturating_mul(MIB),
        max_output_file_bytes: u64::try_from(args.max_output_mib.saturating_mul(MIB))
            .unwrap_or(u64::MAX),
        max_stream_bytes: args.max_stream_mib.saturating_mul(MIB),
    })
}

/// Serve flags for the LaunchAgent, with ego-browser resolved now because
/// launchd starts with a minimal PATH.
fn service_args(args: &ServeArgs, ego_browser: &Path) -> Vec<String> {
    let mut out = vec![
        "--listen".to_owned(),
        args.listen.to_string(),
        "--ego-browser".to_owned(),
        ego_browser.to_string_lossy().into_owned(),
        "--max-concurrent-runs".to_owned(),
        args.max_concurrent_runs.to_string(),
        "--default-timeout".to_owned(),
        humantime::format_duration(args.default_timeout).to_string(),
        "--max-timeout".to_owned(),
        humantime::format_duration(args.max_timeout).to_string(),
        "--max-request-mib".to_owned(),
        args.max_request_mib.to_string(),
        "--max-output-mib".to_owned(),
        args.max_output_mib.to_string(),
        "--max-stream-mib".to_owned(),
        args.max_stream_mib.to_string(),
    ];
    for host in &args.allowed_hosts {
        out.push("--allowed-host".to_owned());
        out.push(host.clone());
    }
    out
}

fn init_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn run_client(args: &[String]) -> anyhow::Result<i32> {
    let home = config::home_dir()?;
    let config = ClientConfig::load(&home)?;
    client::run(args, &config).await
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    let home = config::home_dir()?;
    match cli.command {
        Commands::Serve(args) => {
            init_tracing("info");
            server::serve(serve_options(&args, home)?).await?;
        }
        Commands::Token(command) => {
            let store = TokenStore::new(config::tokens_path(&home));
            match command {
                TokenCommand::Add { name } => {
                    let token = store.add(&name)?;
                    println!("{token}");
                    eprintln!("Token for {name:?} created. It will not be shown again.");
                }
                TokenCommand::List => {
                    for client in store.list()? {
                        println!("{}\tcreated {}", client.name, client.created_at);
                    }
                }
                TokenCommand::Revoke { name } => {
                    if store.revoke(&name)? {
                        eprintln!("Revoked {name:?}.");
                    } else {
                        anyhow::bail!("no client named {name:?}");
                    }
                }
            }
        }
        Commands::Connect { url, token } => {
            let config = ClientConfig { url, token };
            let arguments = match serde_json::to_value(ServerArgs::default())? {
                serde_json::Value::Object(map) => map,
                _ => anyhow::bail!("unexpected argument encoding"),
            };
            let version = client::call(&config, TOOL_VERSION, arguments).await?;
            config.save(&home)?;
            // ego-browser prints its version on stderr.
            eprint!("{}{}", version.stdout, version.stderr);
            eprintln!(
                "Connected. Saved to {}.",
                config::client_config_path(&home).display()
            );
        }
        Commands::Client { args } => return run_client(&args).await,
        Commands::Service(ServiceCommand::Install(args)) => {
            // launchd needs an absolute path; a relative EGO_BRIDGE_HOME would
            // resolve against a different working directory.
            let home = std::path::absolute(&home)?;
            let options = serve_options(&args, home.clone())?;
            std::fs::create_dir_all(&home)?;
            let log = home.join("serve.log");
            let path = service::install(&service_args(&args, &options.ego_browser), &home, &log)?;
            eprintln!("Installed {} (logs: {}).", path.display(), log.display());
        }
        Commands::Service(ServiceCommand::Uninstall) => {
            let path = service::uninstall()?;
            eprintln!("Removed {}.", path.display());
        }
    }
    Ok(0)
}

fn invoked_as_ego_browser() -> bool {
    std::env::args_os()
        .next()
        .and_then(|arg0| {
            Path::new(&arg0)
                .file_name()
                .map(|name| name == "ego-browser")
        })
        .unwrap_or(false)
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("ego-bridge: start runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = if invoked_as_ego_browser() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        runtime.block_on(run_client(&args))
    } else {
        runtime.block_on(run(Cli::parse()))
    };
    match outcome {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(err) => {
            eprintln!("ego-bridge: {err:#}");
            ExitCode::from(u8::try_from(client::BRIDGE_ERROR_EXIT_CODE).unwrap_or(1))
        }
    }
}
