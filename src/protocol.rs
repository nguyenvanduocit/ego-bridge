//! Tool arguments and results shared by the server and the client.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// Tool names. The client and the server must agree on these.
pub const TOOL_SKILL: &str = "skill";
pub const TOOL_HELP: &str = "help";
pub const TOOL_VERSION: &str = "version";
pub const TOOL_NODEJS: &str = "nodejs";

/// Arguments for tools that only select an ego lite instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServerArgs {
    /// Same as `--ego-server-name`: connect to the matching named browser
    /// service on the host. Omit for the default service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct HelpArgs {
    /// Help topic, as in `ego-browser help <topic>`. Omit for general help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

/// A file carried across the bridge, addressed by its path on the caller's
/// machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilePayload {
    /// Absolute path on the caller's machine.
    pub path: String,
    /// File contents, standard base64.
    pub content_base64: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NodejsArgs {
    /// JavaScript to run, exactly what you would pass to
    /// `ego-browser nodejs -e`. Absolute file paths in the script refer to
    /// the caller's machine: send input files in `files`, and every file the
    /// script writes comes back in the result.
    pub script: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Same as `ego-browser nodejs --print`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub print: bool,
    /// Same as `ego-browser nodejs --strip-types`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strip_types: bool,
    /// Same as `ego-browser nodejs --input-type=<value>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    /// Input files the script reads or uploads, keyed by their absolute path
    /// on the caller's machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FilePayload>,
    /// Directories that exist on the caller's machine and that the script
    /// writes into. They are created empty so writes succeed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<String>,
    /// Upper bound for this run in milliseconds. Capped by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Result of a host-side `ego-browser` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Files the script created or changed, at their caller-side paths.
    #[serde(default)]
    pub files: Vec<FilePayload>,
}
