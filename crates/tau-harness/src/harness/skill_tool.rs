//! The harness-owned `skill` tool.
//!
//! The `skill` tool is registered against [`HARNESS_CONNECTION_ID`] in
//! [`Harness::register_harness_tools`] and dispatched inline (bypassing the
//! bus) by [`Harness::handle_skill_tool_call`]. It surfaces the skills the
//! harness already discovered at startup (`Harness::discovered_skills`),
//! so search and load don't touch the filesystem walker again.

use tau_proto::{
    AgentToolCall, CborValue, Event, ToolCallId, ToolDisplay, ToolDisplayStats, ToolDisplayStatus,
    ToolName, ToolRequest,
};

use crate::conversation::ConversationId;
use crate::discovery::DiscoveredSkill;
use crate::error::HarnessError;
use crate::harness::{HARNESS_CONNECTION_ID, Harness};
use crate::prompt::{cbor_map_bool, cbor_map_text};

impl Harness {
    /// Register harness-owned tools (e.g. `skill`).
    pub(crate) fn register_harness_tools(&mut self) {
        let _ = self.registry.register(
            HARNESS_CONNECTION_ID,
            tau_proto::ToolSpec {
                name: ToolName::new("skill"),
                description: Some(
                    "Discover and load skills — short, focused playbooks for \
                     specific tasks. The user has likely curated skills for \
                     workflows they care about, so reach for this tool early: \
                     before tackling any request that touches a tool, command, \
                     framework, or domain you are not deeply familiar with — or \
                     anything the user might have an opinionated way of doing — \
                     run `search` first. Most skills are NOT pre-advertised in \
                     <available_skills>, so a missing entry there is no reason \
                     to skip the search. When a task could plausibly map to \
                     several names (\"commit\", \"git commit\", \"version \
                     control\"), pass them all as a `query` array — hits are \
                     merged and ranked by how many terms matched. Use \
                     `action: load` once you have an exact skill name."
                        .to_owned(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["load", "search"],
                            "description": "Which subcommand to run."
                        },
                        "name": {
                            "type": "string",
                            "description": "(action=load) Exact skill name to load."
                        },
                        "query": {
                            "type": ["string", "array"],
                            "items": {"type": "string"},
                            "description": "(action=search) One or more keywords matched case-insensitively against skill names and descriptions. Single string or array of strings."
                        },
                        "search_content": {
                            "type": "boolean",
                            "description": "(action=search) When true, also search the skill body. Default false."
                        }
                    },
                    "required": ["action"]
                })),
                side_effects: tau_proto::ToolSideEffects::Pure,
            },
        );
    }

    /// Handle the harness-owned `skill` tool call inline.
    ///
    /// Dispatches on the required `action` argument:
    /// - `load`: read skill body by exact name (returns name + content).
    /// - `search`: case-insensitive substring match across skill names and
    ///   descriptions; with `search_content: true`, also greps skill bodies.
    ///   Returns a list of `{name, description}` hits.
    pub(crate) fn handle_skill_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        let tool_name = ToolName::new("skill");

        // Track the conversation mapping first so the published
        // request + result both attribute to this conversation's
        // session via `session_id_for_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tool_names
            .insert(call_id.clone(), tool_name.clone());
        self.bump_tools_started_for(cid);
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments: call.arguments.clone(),
                originator: tau_proto::PromptOriginator::User,
            }),
        );

        let action = cbor_map_text(&call.arguments, "action");
        let result_event = match action {
            Some("load") => self.handle_skill_load(&call_id, &tool_name, &call.arguments),
            Some("search") => self.handle_skill_search(&call_id, &tool_name, &call.arguments),
            Some(other) => {
                let message =
                    format!("unknown skill action: {other:?} (expected \"load\" or \"search\")");
                Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display("", &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            None => {
                let message =
                    "missing required argument: action (\"load\" or \"search\")".to_owned();
                Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display("", &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                })
            }
        };

        // Publish, then drop the in-flight tracking — order matters:
        // `session_id_for_event` reads `tool_conversations` to
        // attribute the persisted record before we clear it.
        self.publish_for_conversation(cid, result_event);
        self.on_tool_call_complete(&call.id);
        self.clear_tool_call_tracking(call_id.as_str());

        Ok(())
    }

    fn handle_skill_load(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let Some(name) = cbor_map_text(arguments, "name") else {
            let message = "missing required argument: name (action=load)".to_owned();
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                display: Some(skill_error_display("", &message)),
                message,
                details: None,
                originator: tau_proto::PromptOriginator::User,
            });
        };
        let Some(skill) = self.discovered_skills.get(name) else {
            // Same agent that asked for `dpc-rust-code-style` very likely
            // wanted one of the skills containing "rust" or "style", so
            // run a free search using the requested name split into
            // word-like tokens. Returning the hits in `details` lets the
            // agent pick the right name on a follow-up call without
            // having to issue an explicit `search` first; the
            // surrounding event is still an error so it can't be
            // mistaken for a successful load.
            let needles = split_skill_name_into_needles(name);
            let matches = if needles.is_empty() {
                Vec::new()
            } else {
                self.search_discovered_skills(&needles, false)
            };
            let message = format!("unknown skill: {name}");
            let mut display = skill_error_display(name, &message);
            display
                .info_chips
                .insert(0, format!("({} suggestions)", matches.len()));
            return Event::ToolError(tau_proto::ToolError {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message,
                details: Some(skill_load_not_found_details(name, &needles, &matches)),
                display: Some(display),
                originator: tau_proto::PromptOriginator::User,
            });
        };
        match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => {
                let body = tau_skills::strip_frontmatter(&content);
                let mut display = skill_ok_display(name);
                display.stats = text_stats_for_skill(body);
                Event::ToolResult(tau_proto::ToolResult {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: CborValue::Map(vec![
                        (
                            CborValue::Text("name".to_owned()),
                            CborValue::Text(name.to_owned()),
                        ),
                        (
                            CborValue::Text("content".to_owned()),
                            CborValue::Text(body.to_owned()),
                        ),
                    ]),
                    display: Some(display),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(e) => {
                let message = format!("failed to read skill file: {e}");
                Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display(name, &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                })
            }
        }
    }

    fn handle_skill_search(
        &self,
        call_id: &ToolCallId,
        tool_name: &ToolName,
        arguments: &CborValue,
    ) -> Event {
        let needles = match extract_skill_search_queries(arguments) {
            Ok(needles) => needles,
            Err(message) => {
                return Event::ToolError(tau_proto::ToolError {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    display: Some(skill_error_display("search:", &message)),
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,
                });
            }
        };
        let search_content = cbor_map_bool(arguments, "search_content").unwrap_or(false);
        let hits = self.search_discovered_skills(&needles, search_content);
        let scope_label = if search_content { " [content]" } else { "" };
        let queries_label = needles.join(" ");
        let display_args = format!("search: {queries_label}{scope_label}");

        let mut display = skill_ok_display(&display_args);
        display.info_chips.push(format!("({}L)", hits.len()));

        let matches = CborValue::Array(
            hits.into_iter()
                .map(|(hit_count, name, description)| {
                    CborValue::Map(vec![
                        (CborValue::Text("name".to_owned()), CborValue::Text(name)),
                        (
                            CborValue::Text("description".to_owned()),
                            CborValue::Text(description),
                        ),
                        (
                            CborValue::Text("hit_count".to_owned()),
                            CborValue::Integer((hit_count as u64).into()),
                        ),
                    ])
                })
                .collect(),
        );
        let queries_echo =
            CborValue::Array(needles.iter().map(|n| CborValue::Text(n.clone())).collect());
        Event::ToolResult(tau_proto::ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            result: CborValue::Map(vec![
                (CborValue::Text("queries".to_owned()), queries_echo),
                (
                    CborValue::Text("search_content".to_owned()),
                    CborValue::Bool(search_content),
                ),
                (CborValue::Text("matches".to_owned()), matches),
            ]),
            display: Some(display),
            originator: tau_proto::PromptOriginator::User,
        })
    }

    /// Score each discovered skill by how many of `needles` match its
    /// name, description, and (when `search_content`) body. A skill
    /// that matches more terms is more likely the right answer when
    /// the agent fired several plausible spellings at the same time
    /// ("commit", "git commit", "version control"). Returns
    /// `(hit_count, name, description)` rows sorted by descending
    /// hit count, with ties broken by name for deterministic output.
    ///
    /// Needles are expected to already be lowercased.
    fn search_discovered_skills(
        &self,
        needles: &[String],
        search_content: bool,
    ) -> Vec<(usize, String, String)> {
        let mut hits: Vec<(usize, &tau_proto::SkillName, &DiscoveredSkill)> = self
            .discovered_skills
            .iter()
            .filter_map(|(name, skill)| {
                let lower_name = name.as_str().to_lowercase();
                let lower_desc = skill.description.to_lowercase();
                // Read the body at most once across all needles, and
                // only when at least one needle didn't match in the
                // name or description and the caller opted in.
                let mut body: Option<String> = None;
                let hit_count = needles
                    .iter()
                    .filter(|needle| {
                        if lower_name.contains(needle.as_str())
                            || lower_desc.contains(needle.as_str())
                        {
                            return true;
                        }
                        if !search_content {
                            return false;
                        }
                        let body = body.get_or_insert_with(|| {
                            std::fs::read_to_string(&skill.file_path)
                                .map(|s| s.to_lowercase())
                                .unwrap_or_else(|err| {
                                    tracing::warn!(
                                        skill = %name.as_str(),
                                        path = %skill.file_path.display(),
                                        error = %err,
                                        "skill body unreadable; treating as empty for content search",
                                    );
                                    String::new()
                                })
                        });
                        body.contains(needle.as_str())
                    })
                    .count();
                (hit_count > 0).then_some((hit_count, name, skill))
            })
            .collect();
        hits.sort_by(|(ac, an, _), (bc, bn, _)| {
            bc.cmp(ac).then_with(|| an.as_str().cmp(bn.as_str()))
        });
        hits.into_iter()
            .map(|(hit_count, name, skill)| {
                (
                    hit_count,
                    name.as_str().to_owned(),
                    skill.description.clone(),
                )
            })
            .collect()
    }
}

/// Split a skill name into lowercased word-like needles by treating
/// `-` and `_` as separators. Used when an agent's `load` request
/// names a skill that doesn't exist: searching the discovered skills
/// for these needles often surfaces the one the agent actually
/// wanted (e.g. `dpc-rust-code-style` → `[dpc, rust, code, style]`).
/// Empty parts are dropped; duplicates are removed in first-seen
/// order so a name like `foo-foo` doesn't double-count itself.
fn skill_ok_display(args: &str) -> ToolDisplay {
    ToolDisplay {
        args: args.to_owned(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

fn skill_error_display(args: &str, message: &str) -> ToolDisplay {
    let chip = error_chip_text(message);
    ToolDisplay {
        args: args.to_owned(),
        status: ToolDisplayStatus::Error,
        status_text: chip,
        ..Default::default()
    }
}

fn text_stats_for_skill(text: &str) -> ToolDisplayStats {
    if text.is_empty() {
        return ToolDisplayStats::default();
    }
    ToolDisplayStats {
        matches: None,
        lines: Some(text.lines().count() as u64),
        bytes: Some(text.len() as u64),
    }
}

fn error_chip_text(message: &str) -> String {
    let first = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.is_empty() {
        return "err".to_owned();
    }
    const MAX: usize = 64;
    let label = if first.chars().count() <= MAX {
        first.to_owned()
    } else {
        let mut s: String = first.chars().take(MAX.saturating_sub(1)).collect();
        s.push('…');
        s
    };
    format!("err: {label}")
}

fn split_skill_name_into_needles(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in name.split(['-', '_']) {
        if part.is_empty() {
            continue;
        }
        let lower = part.to_lowercase();
        if !out.iter().any(|existing| existing == &lower) {
            out.push(lower);
        }
    }
    out
}

/// Build the `details` payload for a failed `skill` load. Mirrors
/// the shape of a successful `search` result (`query`,
/// `search_content`, `matches`) so a UI that already knows how to
/// render skill-search hits can show the suggestion count next to
/// the error, and so the agent reading the details on its next turn
/// sees a familiar structure.
fn skill_load_not_found_details(
    name: &str,
    needles: &[String],
    matches: &[(usize, String, String)],
) -> CborValue {
    let matches_cbor = CborValue::Array(
        matches
            .iter()
            .map(|(hit_count, skill_name, description)| {
                CborValue::Map(vec![
                    (
                        CborValue::Text("name".to_owned()),
                        CborValue::Text(skill_name.clone()),
                    ),
                    (
                        CborValue::Text("description".to_owned()),
                        CborValue::Text(description.clone()),
                    ),
                    (
                        CborValue::Text("hit_count".to_owned()),
                        CborValue::Integer((*hit_count as u64).into()),
                    ),
                ])
            })
            .collect(),
    );
    let queries_echo =
        CborValue::Array(needles.iter().map(|n| CborValue::Text(n.clone())).collect());
    CborValue::Map(vec![
        (
            CborValue::Text("name".to_owned()),
            CborValue::Text(name.to_owned()),
        ),
        (CborValue::Text("queries".to_owned()), queries_echo),
        (
            CborValue::Text("search_content".to_owned()),
            CborValue::Bool(false),
        ),
        (CborValue::Text("matches".to_owned()), matches_cbor),
    ])
}

/// Parse the `query` argument of a `skill` tool call's `search` action
/// into one-or-more lowercased search needles. Accepts either a single
/// string (one needle) or an array of strings (multiple needles whose
/// hits are merged and ranked by hit-count). Returns a user-facing
/// error message string on missing/empty/malformed input.
fn extract_skill_search_queries(arguments: &CborValue) -> Result<Vec<String>, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("missing required argument: query (action=search)".to_owned());
    };
    let raw = entries
        .iter()
        .find_map(|(k, v)| match k {
            CborValue::Text(k) if k == "query" => Some(v),
            _ => None,
        })
        .ok_or_else(|| "missing required argument: query (action=search)".to_owned())?;

    let needles: Vec<String> = match raw {
        CborValue::Text(s) => vec![s.to_lowercase()],
        CborValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    CborValue::Text(s) => out.push(s.to_lowercase()),
                    _ => return Err("query array entries must all be strings".to_owned()),
                }
            }
            out
        }
        _ => {
            return Err("query must be a string or an array of strings (action=search)".to_owned());
        }
    };

    let needles: Vec<String> = needles.into_iter().filter(|n| !n.is_empty()).collect();
    if needles.is_empty() {
        return Err("query must include at least one non-empty term".to_owned());
    }
    Ok(needles)
}
