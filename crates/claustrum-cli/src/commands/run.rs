//! `claustrum run`: launch Claude Code against the sandbox.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use claustrum_sandbox::WORKSPACE;

use crate::config::Config;

/// MCP server name; tools appear as `mcp__claustrum__<Tool>`.
pub const SERVER_NAME: &str = "claustrum";

/// Built-in tools kept for plan mode.
const PLAN_TOOLS: &[&str] = &["EnterPlanMode", "ExitPlanMode"];

#[derive(clap::Args, Debug)]
#[command(trailing_var_arg = true)]
pub struct Args {
    /// Host directory to mount at /workspace. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,

    /// Claude Code permission mode (default, acceptEdits, plan, dontAsk, bypassPermissions).
    #[arg(long)]
    pub permission_mode: Option<String>,

    /// Path to the `claude` binary.
    #[arg(long)]
    pub claude: Option<PathBuf>,

    /// Print the command line instead of executing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Arguments passed through to `claude` (e.g. `-p "prompt"`).
    #[arg(allow_hyphen_values = true)]
    pub claude_args: Vec<String>,
}

pub fn run(config: Config, args: Args) -> Result<()> {
    let workspace = config.workspace(args.workspace.as_deref())?;
    config.migrate_state(&workspace);
    let actions = config.validate_actions(&workspace)?;
    let network = config.network_policy(&workspace)?;
    let confined = config.confinement()?.active().map_err(anyhow::Error::msg)?;
    eprintln!("claustrum: {}", Config::network_notice(&network));
    eprintln!("claustrum: {}", Config::confinement_notice(confined));
    if let Some(notice) = Config::actions_notice(&actions, confined) {
        eprintln!("claustrum: {notice}");
    }
    // Fail early with a helpful message instead of letting claude report a
    // dead MCP server.
    let missing: Vec<_> = config
        .packages()
        .into_iter()
        .filter(|p| !p.file.is_file())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "missing package file(s): {}\nRun `claustrum pkg sync` first.",
            missing
                .iter()
                .map(|p| p.file.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let claude = args
        .claude
        .clone()
        .or_else(|| config.file.claude.binary.clone())
        .map(Ok)
        .unwrap_or_else(|| which::which("claude").map_err(anyhow::Error::from))
        .context("cannot find the `claude` binary; pass --claude or set [claude].binary")?;

    let plans = config.host_plans(&workspace).map(|p| p.dir().to_path_buf());
    if let Some(dir) = &plans {
        eprintln!(
            "claustrum: plans: {} (written through WritePlan/EditPlan, this workspace's files only)",
            dir.display()
        );
    }

    let this = std::env::current_exe().context("cannot determine own executable path")?;
    let mut serve_args = vec![
        "serve".to_owned(),
        "--workspace".to_owned(),
        workspace.display().to_string(),
        "--packages-dir".to_owned(),
        config.packages_dir.display().to_string(),
    ];
    if let Some(cfg) = &config.path {
        serve_args.push("--config".to_owned());
        serve_args.push(
            cfg.canonicalize()
                .unwrap_or_else(|_| cfg.clone())
                .display()
                .to_string(),
        );
    }
    let mcp_config = serde_json::json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "stdio",
                "command": this.display().to_string(),
                "args": serve_args,
            }
        }
    });

    // Remove every built-in tool (unless configured otherwise); MCP tools are
    // unaffected by --tools. The plan mode tools only switch the mode.
    let mut tools = config.file.claude.tools.clone();
    if plans.is_some() {
        for t in PLAN_TOOLS {
            if !tools.iter().any(|x| x == t) {
                tools.push((*t).to_owned());
            }
        }
    }

    let mut cmd = Command::new(&claude);
    cmd.current_dir(&workspace)
        .arg("--tools")
        .arg(tools.join(","))
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(mcp_config.to_string())
        .arg("--allowedTools")
        .arg(format!("mcp__{SERVER_NAME}__*"))
        .arg("--append-system-prompt")
        .arg(system_prompt(
            &config,
            &network,
            plans.as_deref(),
            config.file.claude.system_prompt.as_deref(),
        ));
    if let Some(mode) = &args.permission_mode {
        cmd.arg("--permission-mode").arg(mode);
    }
    cmd.args(&config.file.claude.args);
    cmd.args(&args.claude_args);

    if args.dry_run {
        println!("{}", shell_words(&cmd));
        return Ok(());
    }

    tracing::info!(claude = %claude.display(), workspace = %workspace.display(), "launching claude");
    let err = cmd.exec();
    Err(anyhow::Error::from(err).context(format!("failed to execute {}", claude.display())))
}

fn system_prompt(
    config: &Config,
    network: &claustrum_sandbox::NetworkPolicy,
    plans: Option<&Path>,
    extra: Option<&str>,
) -> String {
    let mut s = format!(
        "You are running inside Claustrum, a WASIX sandbox. Your only tools are the \
         mcp__{SERVER_NAME}__Bash, Read, Write, Edit, Glob and Grep tools; they replace the \
         built-in tools of the same name and take the same parameters. The project directory is \
         mounted at {WORKSPACE}, which is the working directory; refer to files by paths under \
         {WORKSPACE} or relative to it. Only the sandbox's own commands are available in Bash \
         (bash, coreutils, python, jq and whatever else is installed; the MCP server \
         instructions list them); there is no git, no host toolchain and no \
         access to files outside the sandbox. If a needed command is missing, \
         say so instead of trying workarounds on the host. The sandbox configuration \
         ({WORKSPACE}/claustrum.toml) and the Claude Code settings \
         ({WORKSPACE}/.claude/settings.json and settings.local.json) are read-only for you: \
         you can read them but not change, replace or delete them; if a setting there needs \
         to change, ask the user. "
    );
    if let Some(dir) = plans {
        s.push_str(&format!(
            "Plan mode names a plan file in {}, which is outside the sandbox. Write and edit \
             it only with mcp__{SERVER_NAME}__WritePlan and mcp__{SERVER_NAME}__EditPlan, \
             using the exact path plan mode gives you; plan mode refuses the regular Write, \
             Edit and Bash tools, and Read cannot open the plan file. ",
            dir.display()
        ));
    }
    s.push_str(&claustrum_sandbox::net::describe_for_model(
        network.mode,
        &network.allow,
    ));
    let mounts = &config.file.mounts;
    if !mounts.is_empty() {
        s.push_str(&format!(
            " Additional directories are mounted at: {}.",
            mounts
                .iter()
                .map(|m| format!(
                    "{} ({})",
                    m.guest,
                    if m.writable {
                        "read/write"
                    } else {
                        "read-only"
                    }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let actions = &config.file.actions.list;
    if !actions.is_empty() {
        let command = config.action_command();
        s.push_str(&format!(
            " The user has declared host actions: fixed commands that run on the host machine \
             outside the sandbox when you trigger them. They are the only way to reach the host. \
             Trigger one with the mcp__{SERVER_NAME}__Action tool or with `{command} <name> \
             [inputs]` in Bash; `{command}` alone lists them with their inputs. Inputs are \
             validated against the declaration and anything else is refused. Available: {}.",
            actions
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(extra) = extra {
        s.push_str("\n\n");
        s.push_str(extra);
    }
    s
}

fn shell_words(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| {
            let a = a.to_string_lossy();
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || "\"'{}$*".contains(c))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
