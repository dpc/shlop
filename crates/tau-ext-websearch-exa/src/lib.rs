//! Web-search extension backed by Exa's keyless free-tier MCP at
//! <https://mcp.exa.ai/mcp>.
//!
//! Registers a single `websearch-exa` tool. On each `ToolInvoke` the
//! extension proxies the call to Exa's hosted `web_search_exa` tool
//! over its Streamable-HTTP MCP transport (HTTP POST returning a
//! `text/event-stream` body with one `message` SSE event), unwraps the
//! JSON-RPC envelope, and returns the model-friendly text blob Exa
//! produces.
//!
//! Free tier: ~1000 requests/month per IP, no API key required. We
//! intentionally keep this minimal — a richer extension covering
//! `web_fetch_exa`, API-key support, etc. can layer on later.

use std::error::Error;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use tau_proto::{
    Ack, CborValue, ClientKind, Event, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, LogEventId, PROTOCOL_VERSION, ToolError, ToolInvoke,
    ToolRegister, ToolResult, ToolSideEffects, ToolSpec,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "websearch-exa";

/// Tool name the extension registers with the harness. The harness's
/// `ToolName` newtype only accepts `[a-zA-Z0-9_]+`, so we use an
/// underscore between the two halves.
pub const TOOL_NAME: &str = "websearch_exa";

/// Default Exa MCP endpoint. Override via the extension's
/// `config.endpoint` field if you want to point at a self-hosted MCP
/// server or attach an `?exaApiKey=…` query parameter for the paid
/// tier.
pub const DEFAULT_ENDPOINT: &str = "https://mcp.exa.ai/mcp";

/// Name of the upstream tool we forward to.
const REMOTE_TOOL: &str = "web_search_exa";

/// MCP protocol version we declare. Exa accepts our `tools/call`
/// requests directly without an `initialize` handshake, so this header
/// just helps version-pin behavior on the server side.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Default `numResults` we forward to Exa when the agent omits it.
/// Five is enough to triage most queries without burning the monthly
/// quota too fast.
const DEFAULT_NUM_RESULTS: u32 = 5;

/// Maximum `numResults` the agent can request. Mirrors Exa's own
/// upper bound on the `web_search_exa` schema.
const MAX_NUM_RESULTS: u32 = 100;

/// Per-request timeout. Exa search latency is in the high hundreds
/// of milliseconds; the long ceiling is in case a slow page extraction
/// is in play, but we'd rather fail fast than block the agent turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging();
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_searcher(reader, writer, Arc::new(HttpSearcher::default()))
}

/// Performs one Exa search. Abstracted so tests can stub the network
/// call without spinning up a real HTTP server.
pub trait Searcher: Send + Sync + 'static {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String>;
}

fn run_with_searcher<R, W>(
    reader: R,
    writer: W,
    searcher: Arc<dyn Searcher>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-ext-websearch-exa".into(),
        client_kind: ClientKind::Tool,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(tau_proto::EventName::TOOL_INVOKE),
            EventSelector::Exact(tau_proto::EventName::LIFECYCLE_DISCONNECT),
        ],
    }))?;
    writer.write_event(&Event::ToolRegister(ToolRegister {
        tool: tool_spec(),
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("websearch-exa ready".to_owned()),
    }))?;
    writer.flush()?;

    let (tx, rx) = mpsc::channel::<Event>();

    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for event in rx {
            writer
                .write_event(&event)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    loop {
        let Some(event) = reader.read_event()? else {
            break;
        };
        let (log_id, inner) = event.peel_log();
        match inner {
            Event::ToolInvoke(invoke) => {
                let tx = tx.clone();
                let searcher = Arc::clone(&searcher);
                std::thread::spawn(move || dispatch_tool_invoke(invoke, searcher.as_ref(), &tx));
            }
            Event::LifecycleDisconnect(_) => break,
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &tx);
        }
    }

    drop(tx);
    writer_handle
        .join()
        .map_err(|_| "writer thread panicked")?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

fn ack_log_event(id: LogEventId, tx: &mpsc::Sender<Event>) {
    let _ = tx.send(Event::Ack(Ack { up_to: id }));
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_NAME.into(),
        description: Some(
            "Search the web via Exa's free-tier hosted MCP. Returns clean, ready-to-use \
             text content (titles, URLs, highlights) from top-ranked pages. Works best with a \
             natural-language description of the *ideal page* rather than a keyword query — \
             e.g. \"blog post comparing React and Vue performance\" beats \"React vs Vue\". \
             Use category:people / category:company prefixes to scope results to LinkedIn-style \
             profiles or company pages."
                .to_owned(),
        ),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of the ideal page. May start with `category:people` or `category:company` to focus the result set."
                },
                "num_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_NUM_RESULTS,
                    "description": format!("Number of results to return (default: {DEFAULT_NUM_RESULTS}, max: {MAX_NUM_RESULTS}).")
                }
            },
            "required": ["query"]
        })),
        side_effects: ToolSideEffects::Pure,
    }
}

fn dispatch_tool_invoke(invoke: ToolInvoke, searcher: &dyn Searcher, tx: &mpsc::Sender<Event>) {
    if invoke.tool_name.as_str() != TOOL_NAME {
        let _ = tx.send(Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            message: "unknown tool".to_owned(),
            details: None,
        }));
        return;
    }
    let event = match parse_args(&invoke.arguments) {
        Ok((query, num_results)) => match searcher.search(&query, num_results) {
            Ok(text) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    query = %query,
                    num_results,
                    response_len = text.len(),
                    "exa search returned",
                );
                Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    result: CborValue::Text(text),
                })
            }
            Err(message) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    query = %query,
                    error = %message,
                    "exa search failed",
                );
                Event::ToolError(ToolError {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    message,
                    details: Some(invoke.arguments),
                })
            }
        },
        Err(message) => Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            message,
            details: Some(invoke.arguments),
        }),
    };
    let _ = tx.send(event);
}

fn parse_args(arguments: &CborValue) -> Result<(String, u32), String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut query: Option<String> = None;
    let mut num_results: Option<u32> = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "query" => match v {
                CborValue::Text(text) => query = Some(text.clone()),
                _ => return Err("`query` must be a string".to_owned()),
            },
            "num_results" => num_results = Some(parse_num_results(v)?),
            // Forward-compat: ignore unknown keys instead of erroring.
            _ => {}
        }
    }
    let query = query.ok_or_else(|| "missing string argument: query".to_owned())?;
    if query.trim().is_empty() {
        return Err("`query` must not be empty".to_owned());
    }
    Ok((query, num_results.unwrap_or(DEFAULT_NUM_RESULTS)))
}

fn parse_num_results(value: &CborValue) -> Result<u32, String> {
    let raw: i128 = match value {
        CborValue::Integer(n) => (*n).into(),
        _ => return Err("`num_results` must be an integer".to_owned()),
    };
    if raw < 1 {
        return Err("`num_results` must be >= 1".to_owned());
    }
    if raw > i128::from(MAX_NUM_RESULTS) {
        return Err(format!("`num_results` must be <= {MAX_NUM_RESULTS}"));
    }
    Ok(raw as u32)
}

// ---------------------------------------------------------------------------
// Default Searcher: HTTP → Exa MCP
// ---------------------------------------------------------------------------

struct HttpSearcher {
    endpoint: String,
    agent: ureq::Agent,
}

impl Default for HttpSearcher {
    fn default() -> Self {
        Self::new(DEFAULT_ENDPOINT.to_owned())
    }
}

impl HttpSearcher {
    fn new(endpoint: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(REQUEST_TIMEOUT)
            .build();
        Self { endpoint, agent }
    }
}

impl Searcher for HttpSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": REMOTE_TOOL,
                "arguments": {
                    "query": query,
                    "numResults": num_results,
                },
            },
        });
        let response = self
            .agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
            .send_string(&body.to_string())
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let body = resp.into_string().unwrap_or_default();
                    format!("exa MCP returned HTTP {code}: {body}")
                }
                ureq::Error::Transport(err) => format!("exa MCP transport error: {err}"),
            })?;
        let payload = response
            .into_string()
            .map_err(|e| format!("reading exa MCP response: {e}"))?;
        decode_mcp_text_result(&payload)
    }
}

/// Pull the `result.content[*].text` blob out of an MCP JSON-RPC
/// response, accepting either a bare JSON body or a `text/event-stream`
/// body whose `data:` line(s) carry the JSON. Concatenates multiple
/// text content parts with blank lines so the agent sees one prose
/// blob.
fn decode_mcp_text_result(payload: &str) -> Result<String, String> {
    let json = parse_sse_or_json(payload)?;
    if let Some(error) = json.get("error") {
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("exa MCP returned a JSON-RPC error");
        return Err(message.to_owned());
    }
    let content = json
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| "exa MCP response missing `result.content`".to_owned())?;
    let mut chunks = Vec::new();
    for part in content {
        if part.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            chunks.push(text.to_owned());
        }
    }
    if chunks.is_empty() {
        return Err("exa MCP response had no text content".to_owned());
    }
    Ok(chunks.join("\n\n"))
}

/// Parse Exa's response, which may be `application/json` or
/// `text/event-stream` (the MCP Streamable-HTTP transport). For SSE we
/// only look at `data:` lines — Exa sends a single `event: message`
/// frame, but we tolerate multiple frames by taking the first
/// well-formed one.
fn parse_sse_or_json(payload: &str) -> Result<serde_json::Value, String> {
    let trimmed = payload.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON from exa MCP: {e}"));
    }
    let mut buf = String::new();
    let reader = std::io::Cursor::new(payload.as_bytes());
    let reader = std::io::BufReader::new(reader);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("reading SSE stream: {e}"))?;
        if let Some(rest) = line.strip_prefix("data:") {
            buf.push_str(rest.trim_start());
            buf.push('\n');
        } else if line.is_empty() && !buf.is_empty() {
            return serde_json::from_str(buf.trim())
                .map_err(|e| format!("invalid JSON from exa MCP SSE frame: {e}"));
        }
    }
    if buf.is_empty() {
        return Err("exa MCP returned no SSE data frames".to_owned());
    }
    serde_json::from_str(buf.trim())
        .map_err(|e| format!("invalid JSON from exa MCP SSE frame: {e}"))
}

#[cfg(test)]
mod tests;
