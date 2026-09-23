//! The MCP server and its tools.

use std::time::Duration;

use claustrum_sandbox::{
    ExecOptions, Sandbox, WORKSPACE,
    native::{GrepMode, GrepOptions, ReadOptions},
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, Implementation, ProgressNotificationParam,
        ServerCapabilities, ServerConfig,
    },
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::format;

/// Maximum timeout a single Bash call may request.
const MAX_BASH_TIMEOUT: Duration = Duration::from_secs(600);
/// How often a progress notification is sent while a command runs, so that
/// Claude Code's idle timeout does not fire on long-running commands.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ClaustrumServer {
    sandbox: Sandbox,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashParams {
    /// The shell command to run with `bash -c`.
    pub command: String,
    /// Optional timeout in milliseconds (max 600000). Defaults to the sandbox policy.
    pub timeout: Option<u64>,
    /// Short description of what the command does, for the user's benefit.
    #[allow(dead_code)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// Path of the file to read. Absolute guest path (e.g. `/workspace/src/main.rs`) or relative to the working directory.
    pub file_path: String,
    /// 1-based line number to start reading from.
    pub offset: Option<usize>,
    /// Maximum number of lines to read (default 2000).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteParams {
    /// Path of the file to create or overwrite.
    pub file_path: String,
    /// Complete new contents of the file.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditParams {
    /// Path of the file to edit.
    pub file_path: String,
    /// Exact text to replace. Must match the file contents exactly and, unless `replace_all` is set, occur exactly once.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobParams {
    /// Glob pattern such as `**/*.rs` or `src/**/*.test.ts`.
    pub pattern: String,
    /// Directory to search in. Defaults to the working directory.
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepParams {
    /// Regular expression (Rust regex syntax).
    pub pattern: String,
    /// File or directory to search. Defaults to the working directory.
    pub path: Option<String>,
    /// Restrict the search to files matching this glob, e.g. `*.rs`.
    pub glob: Option<String>,
    /// Output mode: `files_with_matches` (default), `content` or `count`.
    pub output_mode: Option<String>,
    /// Case-insensitive search.
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    /// Lines of context around each match (content mode only).
    #[serde(rename = "-C")]
    pub context: Option<usize>,
    /// Maximum number of results.
    pub head_limit: Option<usize>,
}

fn text(s: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn tool_error(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("Error: {e}"))])
}

#[tool_router]
impl ClaustrumServer {
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    #[tool(
        name = "Bash",
        description = "Run a shell command inside the sandbox with `bash -c`. The project is mounted at /workspace (the working directory). Only the sandbox's own commands are available (bash, coreutils, ...); there is no network. Output is captured and returned when the command finishes.",
        annotations(
            title = "Bash (sandboxed)",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn bash(
        &self,
        Parameters(p): Parameters<BashParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = p
            .timeout
            .map(|ms| Duration::from_millis(ms).min(MAX_BASH_TIMEOUT));
        let options = ExecOptions {
            timeout,
            ..Default::default()
        };

        let run = self.sandbox.bash(&p.command, options);
        tokio::pin!(run);

        // Keep the client informed while the command runs.
        let token = ctx.meta.get_progress_token();
        let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
        tick.tick().await; // first tick completes immediately
        let mut n = 0u32;
        let result = loop {
            tokio::select! {
                r = &mut run => break r,
                _ = tick.tick() => {
                    n += 1;
                    if let Some(token) = &token {
                        let param = ProgressNotificationParam::new(token.clone(), f64::from(n))
                            .with_message(format!("still running ({}s)", u64::from(n) * PROGRESS_INTERVAL.as_secs()));
                        let _ = ctx.peer.notify_progress(param).await;
                    }
                }
            }
        };

        Ok(match result {
            Ok(out) => {
                let rendered = format::exec(&out);
                if out.success() {
                    text(rendered)
                } else {
                    CallToolResult::error(vec![ContentBlock::text(rendered)])
                }
            }
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        name = "Read",
        description = "Read a file from the sandbox. Returns the contents with line numbers (`cat -n` style). Use offset/limit for large files.",
        annotations(title = "Read file", read_only_hint = true, idempotent_hint = true)
    )]
    async fn read(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let opts = ReadOptions {
            offset: p.offset,
            limit: p.limit,
        };
        Ok(match self.sandbox.read(&p.file_path, opts).await {
            Ok(out) => text(format::read(&out)),
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        name = "Write",
        description = "Create or overwrite a file in the sandbox with the given contents. Parent directories are created as needed. Prefer Edit for changes to existing files.",
        annotations(
            title = "Write file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn write(
        &self,
        Parameters(p): Parameters<WriteParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.sandbox.write(&p.file_path, &p.content).await {
            Ok(out) => text(format::write(&out)),
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        name = "Edit",
        description = "Replace an exact string in a file. `old_string` must match the file contents exactly (including whitespace) and be unique unless `replace_all` is true.",
        annotations(title = "Edit file", read_only_hint = false, destructive_hint = true)
    )]
    async fn edit(
        &self,
        Parameters(p): Parameters<EditParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match self
                .sandbox
                .edit(
                    &p.file_path,
                    &p.old_string,
                    &p.new_string,
                    p.replace_all.unwrap_or(false),
                )
                .await
            {
                Ok(out) => text(format::edit(&out)),
                Err(e) => tool_error(e),
            },
        )
    }

    #[tool(
        name = "Glob",
        description = "Find files by glob pattern (e.g. `**/*.rs`). Returns matching paths, most recently modified first. `.git`, `node_modules` and `target` are skipped.",
        annotations(title = "Find files", read_only_hint = true, idempotent_hint = true)
    )]
    async fn glob(
        &self,
        Parameters(p): Parameters<GlobParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.sandbox.glob(&p.pattern, p.path.as_deref()) {
            Ok(out) => text(format::glob(&out)),
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        name = "Grep",
        description = "Search file contents with a regular expression. Output modes: `files_with_matches` (default), `content` (matching lines with line numbers) or `count`.",
        annotations(title = "Search files", read_only_hint = true, idempotent_hint = true)
    )]
    async fn grep(
        &self,
        Parameters(p): Parameters<GrepParams>,
    ) -> Result<CallToolResult, McpError> {
        let mode = match p.output_mode.as_deref() {
            None | Some("files_with_matches") => GrepMode::FilesWithMatches,
            Some("content") => GrepMode::Content,
            Some("count") => GrepMode::Count,
            Some(other) => {
                return Ok(tool_error(format!(
                    "unknown output_mode `{other}`; use files_with_matches, content or count"
                )));
            }
        };
        let opts = GrepOptions {
            path: p.path,
            glob: p.glob,
            case_insensitive: p.case_insensitive.unwrap_or(false),
            mode,
            context: p.context.unwrap_or(0),
            max_results: p.head_limit,
        };
        Ok(match self.sandbox.grep(&p.pattern, opts).await {
            Ok(out) => text(format::grep(&out)),
            Err(e) => tool_error(e),
        })
    }
}

#[tool_handler]
impl ServerHandler for ClaustrumServer {
    fn get_info(&self) -> ServerConfig {
        let commands = self.sandbox.commands();
        let instructions = format!(
            "Claustrum runs your tools inside a WASIX sandbox. The project directory is mounted \
             read/write at {WORKSPACE}, which is also the working directory; changes there are \
             visible on the host. Nothing outside {WORKSPACE}, /tmp and /home/claude is \
             accessible, and there is no network access. Use Read/Write/Edit/Glob/Grep for file \
             work and Bash for everything else. Available commands in Bash: {}.",
            commands.join(", ")
        );
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("claustrum", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }
}

/// Serve the sandbox over stdio until the client disconnects.
pub async fn serve_stdio(sandbox: Sandbox) -> anyhow::Result<()> {
    let server = ClaustrumServer::new(sandbox);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
