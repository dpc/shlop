use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread;

use tau_proto::{EventName, ToolInvoke};

use super::*;

struct StubSearcher {
    calls: Mutex<Vec<(String, u32)>>,
    response: Mutex<Result<String, String>>,
}

impl StubSearcher {
    fn ok(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Ok(text.into())),
        })
    }

    fn err(message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Err(message.into())),
        })
    }
}

impl Searcher for StubSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((query.to_owned(), num_results));
        self.response.lock().expect("lock").clone()
    }
}

fn spawn_extension(
    searcher: Arc<dyn Searcher>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    thread::spawn(move || {
        run_with_searcher(reader_stream, ext_stream, searcher).expect("run");
    });
    (
        EventReader::new(BufReader::new(
            harness_stream.try_clone().expect("harness clone"),
        )),
        EventWriter::new(BufWriter::new(harness_stream)),
    )
}

fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) {
    for expected in [
        EventName::LIFECYCLE_HELLO,
        EventName::LIFECYCLE_SUBSCRIBE,
        EventName::TOOL_REGISTER,
        EventName::LIFECYCLE_READY,
    ] {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event")
            ;
        assert_eq!(event.name(), expected);
    }
}

#[test]
fn forwards_query_and_num_results_to_searcher_and_returns_text() {
    let searcher = StubSearcher::ok("Title: hi\nURL: https://x\n");
    let (mut reader, mut writer) = spawn_extension(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: TOOL_NAME.into(),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("rust async runtime comparison".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(3.into()),
                ),
            ]),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "call-1");
    assert_eq!(result.tool_name.as_str(), TOOL_NAME);
    let CborValue::Text(text) = result.result else {
        panic!("expected Text result");
    };
    assert!(text.contains("Title: hi"));

    let calls = searcher.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "rust async runtime comparison");
    assert_eq!(calls[0].1, 3);
}

#[test]
fn defaults_num_results_when_omitted() {
    let searcher = StubSearcher::ok("ok");
    let (mut reader, mut writer) = spawn_extension(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-2".into(),
            tool_name: TOOL_NAME.into(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("hello world".to_owned()),
            )]),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    assert!(matches!(event, Event::ToolResult(_)));
    assert_eq!(
        searcher.calls.lock().expect("lock")[0].1,
        DEFAULT_NUM_RESULTS,
    );
}

#[test]
fn missing_query_returns_tool_error() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-3".into(),
            tool_name: TOOL_NAME.into(),
            arguments: CborValue::Map(Vec::new()),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains("query"), "message: {}", err.message);
}

#[test]
fn searcher_error_surfaces_as_tool_error() {
    let searcher = StubSearcher::err("upstream timed out");
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-4".into(),
            tool_name: TOOL_NAME.into(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("anything".to_owned()),
            )]),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert_eq!(err.message, "upstream timed out");
}

#[test]
fn rejects_num_results_out_of_range() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-5".into(),
            tool_name: TOOL_NAME.into(),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("anything".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(0.into()),
                ),
            ]),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains(">= 1"), "message: {}", err.message);
}

// ---- Wire decoding ----

#[test]
fn decodes_sse_message_frame() {
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body).expect("decode");
    assert_eq!(text, "hello");
}

#[test]
fn concatenates_multiple_text_content_parts() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#;
    let text = decode_mcp_text_result(body).expect("decode");
    assert_eq!(text, "first\n\nsecond");
}

#[test]
fn surfaces_jsonrpc_error_message() {
    let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}"#;
    let err = decode_mcp_text_result(body).expect_err("should fail");
    assert!(err.contains("bad params"), "err: {err}");
}

#[test]
fn fails_when_response_has_no_text_content() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":""}]}}"#;
    let err = decode_mcp_text_result(body).expect_err("should fail");
    assert!(err.contains("no text content"), "err: {err}");
}

/// Live smoke test against the real Exa keyless free tier. Ignored by
/// default so CI / `cargo test` doesn't consume the user's monthly
/// quota; run with `cargo test -p tau-ext-websearch-exa -- --ignored`
/// to exercise the full HTTP path against `mcp.exa.ai`.
#[test]
#[ignore = "hits the real Exa MCP endpoint; consumes free-tier quota"]
fn live_exa_query_returns_text() {
    let searcher = HttpSearcher::default();
    let text = searcher
        .search("blog post about the Rust borrow checker", 1)
        .expect("live exa query");
    assert!(!text.is_empty(), "exa returned empty text");
    assert!(
        text.contains("Title:") || text.contains("URL:"),
        "exa response missing expected formatting: {text}",
    );
}
