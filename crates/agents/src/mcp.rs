//! MCP client (guide §13.2): bridge a Model Context Protocol server's tools
//! into the skill registry.
//!
//! One config file per server under `sica-settings/mcp/`, stdio transport,
//! **tools only** — resources and prompts are a different shape of thing and
//! the registry has nowhere to put them.
//!
//! ```toml
//! # sica-settings/mcp/filesystem.toml
//! command = "npx"
//! args    = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! enabled = true
//! [env]
//! NODE_ENV = "production"
//! ```
//!
//! Each remote tool becomes a [`McpTool`] named `mcp__<server>__<tool>`,
//! normalised to the function-name charset so a server that ships a tool
//! called `read file!` still produces a name a provider will accept. Its
//! `positional_args` come from the schema's `required` list, everything
//! else in `properties` becomes an optional arg, and the schema itself goes
//! into the `tools` array *verbatim* — an MCP tool's arguments are typed
//! (numbers, booleans, nested objects) and flattening them to strings the
//! way the built-in skills do would lose exactly what makes them callable.
//!
//! **Failures are never fatal.** A server that will not start, a config
//! that will not parse, a `tools/list` that times out: each is a warning the
//! operator reads, and the agent comes up with the tools it does have. An
//! MCP server is somebody else's process; the harness must not be hostage
//! to it.
//!
//! Results are untrusted (`trusted() == false`): an MCP server returns text
//! from wherever it fetched it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::skill::{Skill, SkillContext, SkillOutcome};

/// How long a server gets to start up and answer `tools/list`. A server
/// that npx has to download can be slow the first time; beyond this it is
/// holding up backend start, which is worse than doing without its tools.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Wall clock for one `tools/call`.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// One `sica-settings/mcp/<name>.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Executable to run. Launched directly, not through a shell — an MCP
    /// server is a program plus argv, and a shell in between only adds a
    /// quoting layer to get wrong.
    pub command: String,
    #[serde(default)]
    pub args:    Vec<String>,
    #[serde(default)]
    pub env:     BTreeMap<String, String>,
    /// Working directory for the child. Relative paths resolve against the
    /// agent's working directory; absent means the working directory.
    #[serde(default)]
    pub cwd:     Option<String>,
    /// Set `false` to keep the file but not start the server.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Directory holding the per-server config files.
pub fn config_dir() -> PathBuf {
    sica_core::paths::workspace_root().join("sica-settings").join("mcp")
}

/// Read every `*.toml` in `dir`. The file stem is the server name. Returns
/// the configs in name order plus warnings for the files that would not
/// parse — a malformed config must not hide the servers that are fine.
pub fn read_configs(dir: &Path) -> (Vec<(String, ServerConfig)>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // No directory is the normal case: MCP is opt-in.
        Err(_) => return (out, warnings),
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml")))
        .collect();
    paths.sort();
    for path in paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("mcp: {} unreadable: {e}", path.display()));
                continue;
            }
        };
        match toml::from_str::<ServerConfig>(&text) {
            Ok(cfg) => out.push((name, cfg)),
            Err(e) => warnings.push(format!("mcp: {} is malformed: {e}", path.display())),
        }
    }
    (out, warnings)
}

/// A connected server. Held behind an `Arc` by every tool it provides, so
/// the child process outlives the `load` call and one connection serves all
/// of that server's tools.
pub struct McpConnection {
    pub server: String,
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

impl McpConnection {
    async fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<String, String> {
        let params =
            rmcp::model::CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let fut = self.service.call_tool(params);
        let result = match tokio::time::timeout(CALL_TIMEOUT, fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("{}/{tool} failed: {e}", self.server)),
            Err(_) => {
                return Err(format!(
                    "{}/{tool} timed out after {}s",
                    self.server,
                    CALL_TIMEOUT.as_secs()
                ));
            }
        };
        let text = render_content(&result);
        if result.is_error.unwrap_or(false) {
            // The server said the *tool* failed, which is a normal outcome
            // the model reads and corrects — not a transport error.
            return Err(text);
        }
        Ok(text)
    }
}

/// Flatten an MCP result into the text the model reads. Non-text blocks are
/// named rather than dropped: "there was an image here" is information, and
/// silently returning nothing for an image-only result would read as an
/// empty success.
pub fn render_content(result: &rmcp::model::CallToolResult) -> String {
    use rmcp::model::ContentBlock;
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(_) => parts.push("[image content omitted]".into()),
            ContentBlock::Audio(_) => parts.push("[audio content omitted]".into()),
            ContentBlock::Resource(_) => parts.push("[embedded resource omitted]".into()),
            ContentBlock::ResourceLink(r) => parts.push(format!("[resource link: {}]", r.uri)),
            // `ContentBlock` is `#[non_exhaustive]`: a newer block kind must
            // read as "something was here", never as nothing at all.
            _ => parts.push("[unsupported content block omitted]".into()),
        }
    }
    if parts.is_empty() {
        if let Some(v) = &result.structured_content {
            // A server that answers only with `structuredContent` is
            // conformant; returning nothing for it would be our bug.
            return serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
        }
        return "[the tool returned no content]".to_string();
    }
    parts.join("\n")
}

/// One remote tool, as a local skill.
pub struct McpTool {
    conn:        Arc<McpConnection>,
    /// The name the server knows it by.
    remote:      String,
    /// `mcp__<server>__<tool>`, normalised.
    name:        String,
    description: String,
    /// The server's own JSON Schema, passed to the provider verbatim.
    schema:      Value,
    required:    Vec<String>,
    optional:    Vec<String>,
}

impl McpTool {
    pub fn remote_name(&self) -> &str {
        &self.remote
    }
}

/// `mcp__<server>__<tool>`, with every character outside the function-name
/// charset replaced. Providers reject names with spaces or punctuation, and
/// a tool that cannot be named cannot be called.
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect()
    };
    format!("mcp__{}__{}", clean(server), clean(tool))
}

/// Split a schema's `properties` into the required names (in the schema's
/// own `required` order) and the rest (sorted, so the catalogue is stable
/// across runs).
pub fn split_args(schema: &Value) -> (Vec<String>, Vec<String>) {
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut optional: Vec<String> = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|o| {
            o.keys()
                .filter(|k| !required.contains(k))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    optional.sort();
    (required, optional)
}

#[async_trait]
impl Skill for McpTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn positional_args(&self) -> Vec<String> { self.required.clone() }
    fn optional_args(&self) -> Vec<String> { self.optional.clone() }
    fn timeout(&self) -> Duration { CALL_TIMEOUT + Duration::from_secs(10) }

    /// The server's own schema, verbatim. An MCP tool's arguments are typed;
    /// the registry's default all-strings shape would make a tool taking a
    /// number or an array uncallable.
    fn parameters_schema(&self) -> Option<Value> {
        Some(self.schema.clone())
    }

    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        let arguments = match args {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                return SkillOutcome {
                    ok: false,
                    summary: format!("expected an arguments object, got {other}"),
                };
            }
        };
        match self.conn.call(&self.remote, arguments).await {
            Ok(text) => SkillOutcome { ok: true, summary: text },
            Err(e) => SkillOutcome { ok: false, summary: e },
        }
    }
}

/// Connect to one server and turn its tools into skills.
///
/// Every failure path returns `Err(String)`: the caller reports it and
/// carries on. Nothing here is allowed to be fatal.
pub async fn connect(name: &str, cfg: &ServerConfig) -> Result<Vec<Arc<McpTool>>, String> {
    use rmcp::ServiceExt;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

    let root = sica_core::paths::working_dir();
    let cwd = match &cfg.cwd {
        Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
        Some(c) => root.join(c),
        None => root,
    };
    let command = tokio::process::Command::new(&cfg.command).configure(|c| {
        c.args(&cfg.args).current_dir(&cwd);
        for (k, v) in &cfg.env {
            c.env(k, v);
        }
    });
    let transport = TokioChildProcess::new(command)
        .map_err(|e| format!("mcp: {name} would not start ({}): {e}", cfg.command))?;

    let service = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| {
            format!(
                "mcp: {name} did not finish initialising within {}s",
                CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("mcp: {name} initialise failed: {e}"))?;

    let tools = tokio::time::timeout(CONNECT_TIMEOUT, service.list_all_tools())
        .await
        .map_err(|_| format!("mcp: {name} did not answer tools/list in time"))?
        .map_err(|e| format!("mcp: {name} tools/list failed: {e}"))?;

    let conn = Arc::new(McpConnection { server: name.to_string(), service });
    Ok(tools
        .into_iter()
        .map(|t| {
            let schema = Value::Object((*t.input_schema).clone());
            let (required, optional) = split_args(&schema);
            let description = match t.description.as_deref() {
                Some(d) if !d.trim().is_empty() => format!("[{name}] {}", d.trim()),
                // A tool with no description is still callable; naming its
                // server is more use to the model than an empty string.
                _ => format!("[{name}] {} (no description provided)", t.name),
            };
            Arc::new(McpTool {
                conn: conn.clone(),
                remote: t.name.to_string(),
                name: tool_name(name, &t.name),
                description,
                schema,
                required,
                optional,
            })
        })
        .collect())
}

/// What a load produced: the skills to register, and everything the
/// operator needs told.
pub struct LoadReport {
    pub tools:    Vec<Arc<McpTool>>,
    pub warnings: Vec<String>,
    /// `(server, tool count)` for the servers that came up.
    pub servers:  Vec<(String, usize)>,
}

/// Read `dir`, start every enabled server, and collect their tools.
pub async fn load_all(dir: &Path) -> LoadReport {
    let (configs, mut warnings) = read_configs(dir);
    let mut tools = Vec::new();
    let mut servers = Vec::new();
    for (name, cfg) in configs {
        if !cfg.enabled {
            continue;
        }
        match connect(&name, &cfg).await {
            Ok(t) => {
                servers.push((name, t.len()));
                tools.extend(t);
            }
            Err(e) => warnings.push(e),
        }
    }
    LoadReport { tools, warnings, servers }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sica-mcp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_config_dir_is_not_an_error() {
        // MCP is opt-in; almost every installation has no such directory.
        let (cfgs, warns) = read_configs(Path::new("no/such/dir"));
        assert!(cfgs.is_empty());
        assert!(warns.is_empty());
    }

    #[test]
    fn configs_load_in_name_order_and_default_to_enabled() {
        let dir = tempdir("cfg");
        std::fs::write(
            dir.join("zeta.toml"),
            "command = \"node\"\nargs = [\"z.js\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("alpha.toml"),
            "command = \"npx\"\nargs = [\"-y\", \"srv\"]\nenabled = false\n\
             [env]\nTOKEN = \"x\"\n",
        )
        .unwrap();
        // Not a config; must be ignored rather than warned about.
        std::fs::write(dir.join("README.md"), "notes").unwrap();

        let (cfgs, warns) = read_configs(&dir);
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(cfgs.len(), 2);
        assert_eq!(cfgs[0].0, "alpha");
        assert!(!cfgs[0].1.enabled);
        assert_eq!(cfgs[0].1.env.get("TOKEN").map(String::as_str), Some("x"));
        assert_eq!(cfgs[1].0, "zeta");
        assert!(cfgs[1].1.enabled, "enabled defaults to true");
        assert_eq!(cfgs[1].1.args, vec!["z.js".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_config_warns_and_the_others_still_load() {
        let dir = tempdir("bad");
        std::fs::write(dir.join("broken.toml"), "command =\n").unwrap();
        std::fs::write(dir.join("good.toml"), "command = \"node\"\n").unwrap();
        let (cfgs, warns) = read_configs(&dir);
        assert_eq!(cfgs.len(), 1, "the good one must still load");
        assert_eq!(cfgs[0].0, "good");
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("malformed"), "{warns:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_normalised_to_the_function_charset() {
        // Providers reject a function name with a space or a bang in it, and
        // a tool that cannot be named cannot be called.
        assert_eq!(tool_name("fs", "read_file"), "mcp__fs__read_file");
        assert_eq!(tool_name("my server", "read file!"), "mcp__my_server__read_file_");
        assert_eq!(tool_name("a.b", "c/d"), "mcp__a_b__c_d");
    }

    #[test]
    fn required_and_optional_come_from_the_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string" },
                "recursive": { "type": "boolean" },
                "depth":     { "type": "number" },
            },
            "required": ["path"],
        });
        let (req, opt) = split_args(&schema);
        assert_eq!(req, vec!["path".to_string()]);
        // Sorted, so the catalogue does not reshuffle between runs.
        assert_eq!(opt, vec!["depth".to_string(), "recursive".to_string()]);
    }

    #[test]
    fn a_schema_without_properties_yields_no_args() {
        let (req, opt) = split_args(&serde_json::json!({ "type": "object" }));
        assert!(req.is_empty());
        assert!(opt.is_empty());
    }

    #[test]
    fn text_blocks_join_and_non_text_blocks_are_named() {
        use rmcp::model::{CallToolResult, ContentBlock};
        let result = CallToolResult::success(vec![
            ContentBlock::text("first"),
            ContentBlock::image("base64", "image/png"),
            ContentBlock::text("second"),
        ]);
        assert_eq!(render_content(&result), "first\n[image content omitted]\nsecond");
    }

    #[test]
    fn a_structured_only_result_is_rendered_rather_than_lost() {
        use rmcp::model::CallToolResult;
        let mut result = CallToolResult::success(Vec::new());
        result.structured_content = Some(serde_json::json!({ "count": 3 }));
        assert!(render_content(&result).contains("\"count\": 3"));
    }

    #[test]
    fn an_empty_result_says_so_rather_than_returning_nothing() {
        use rmcp::model::CallToolResult;
        let result = CallToolResult::success(Vec::new());
        assert_eq!(render_content(&result), "[the tool returned no content]");
    }

    #[tokio::test]
    async fn a_server_that_will_not_start_is_a_warning_not_a_failure() {
        // The rule that matters: an MCP server is somebody else's process,
        // and the harness must come up without it.
        let dir = tempdir("nostart");
        std::fs::write(
            dir.join("ghost.toml"),
            "command = \"this-binary-does-not-exist-anywhere\"\n",
        )
        .unwrap();
        let report = load_all(&dir).await;
        assert!(report.tools.is_empty());
        assert!(report.servers.is_empty());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("would not start"), "{:?}", report.warnings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_disabled_server_is_skipped_without_a_warning() {
        let dir = tempdir("disabled");
        std::fs::write(
            dir.join("off.toml"),
            "command = \"this-binary-does-not-exist-anywhere\"\nenabled = false\n",
        )
        .unwrap();
        let report = load_all(&dir).await;
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.tools.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
