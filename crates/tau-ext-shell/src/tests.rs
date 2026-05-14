use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::{fs, thread};

use tau_proto::{
    CborValue, EventName, Frame, FrameReader, FrameWriter, Message, ToolDisplayPayload, ToolInvoke,
};
use tempfile::TempDir;

use super::*;
use crate::agents::{discover_agents_files_from, discover_agents_files_from_roots};
use crate::argument::{
    cbor_map_int, cbor_map_text, optional_argument_bool, optional_argument_text,
};
use crate::tools::find::run_find;
use crate::tools::grep::{RipgrepError, classify_ripgrep_stderr, grep_result_map, run_grep};
use crate::tools::ls::run_ls;
use crate::tools::read::{read_file, slice_lines};
use crate::tools::shell::{command_details_value, run_command};
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME, LS_TOOL_NAME,
    READ_TOOL_NAME, SHELL_TOOL_NAME, WRITE_TOOL_NAME,
};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_head, truncate_tail};

/// Test-side wrapper around [`FrameReader`] that exposes an `Event`-flavoured
/// API so the existing tests (which don't care about the message/event split)
/// can stay mechanical. Messages other than `LogEvent` are skipped.
struct EventReader<R> {
    inner: FrameReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: FrameReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_frame()? {
                None => return Ok(None),
                Some(frame) => {
                    let (_log_id, peeled) = frame.peel_log();
                    match peeled {
                        Frame::Event(event) => return Ok(Some(event)),
                        Frame::Message(_) => continue,
                    }
                }
            }
        }
    }
    fn read_message(&mut self) -> Result<Option<Message>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_frame()? {
                None => return Ok(None),
                Some(frame) => {
                    let (_log_id, peeled) = frame.peel_log();
                    match peeled {
                        Frame::Event(_) => continue,
                        Frame::Message(message) => return Ok(Some(message)),
                    }
                }
            }
        }
    }
}

/// Test-side wrapper around [`FrameWriter`] that accepts `Event` directly.
struct EventWriter<W> {
    inner: FrameWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: FrameWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_frame(&Frame::Event(event.clone()))
    }

    fn write_frame(&mut self, frame: &Frame) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_frame(frame)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Build a `Frame::Message(Disconnect)` for tests that previously sent
/// `Event::LifecycleDisconnect`. Wrapped in this helper to keep the
/// disruption from the protocol split contained.
fn disconnect_frame(reason: Option<String>) -> Frame {
    Frame::Message(Message::Disconnect(tau_proto::Disconnect { reason }))
}

fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Integer(n)) if k == key => Some((*n).into()),
            _ => None,
        }),
        _ => None,
    }
}

fn spawn_extension() -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair should open");
    let reader_stream = runtime_stream
        .try_clone()
        .expect("runtime reader clone should succeed");
    thread::spawn(move || {
        run_impl(reader_stream, runtime_stream).expect("extension should run");
    });
    (
        EventReader::new(BufReader::new(
            harness_stream
                .try_clone()
                .expect("harness reader clone should succeed"),
        )),
        EventWriter::new(BufWriter::new(harness_stream)),
    )
}

/// Consumes startup events (tool registers). The hello/subscribe/ready
/// messages are filtered out by the test-side `EventReader` wrapper.
fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) {
    for _expected in [
        EventName::TOOL_REGISTER, // echo
        EventName::TOOL_REGISTER, // read
        EventName::TOOL_REGISTER, // write
        EventName::TOOL_REGISTER, // edit
        EventName::TOOL_REGISTER, // apply_patch
        EventName::TOOL_REGISTER, // grep
        EventName::TOOL_REGISTER, // find
        EventName::TOOL_REGISTER, // ls
        EventName::TOOL_REGISTER, // shell
        EventName::TOOL_REGISTER, // gpt_shell
    ] {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        assert_eq!(event.name(), EventName::TOOL_REGISTER);
    }
}

#[test]
fn startup_registers_gpt_shell_with_shell_command_visible_name() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found = false;
    for _ in 0..10 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegister(register) = event else {
            continue;
        };
        if register.tool.name == GPT_SHELL_TOOL_NAME {
            assert_eq!(
                register.tool.model_visible_name,
                Some(tau_proto::ToolName::new("shell_command"))
            );
            found = true;
            break;
        }
    }
    assert!(found, "expected gpt_shell tool registration");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn discover_agents_files_walks_ancestor_chain_in_order() {
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path();
    let nested = root.join("pkg/src");
    fs::create_dir_all(&nested).expect("mkdir");

    let root_agents = root.join("AGENTS.md");
    let pkg_agents = root.join("pkg").join("AGENTS.md");
    let empty_agents = root.join("pkg").join("src").join("AGENTS.md");

    fs::write(&root_agents, "# Root\n- rule one\n").expect("write root");
    fs::write(&pkg_agents, "# Package\n- rule two\n").expect("write pkg");
    fs::write(&empty_agents, "   \n").expect("write empty");

    let discovered = discover_agents_files_from(&nested);
    assert_eq!(discovered.len(), 2);
    assert_eq!(
        discovered[0].file_path,
        root_agents.canonicalize().expect("canonical root")
    );
    assert_eq!(
        discovered[1].file_path,
        pkg_agents.canonicalize().expect("canonical pkg")
    );
    assert!(discovered[0].content.contains("rule one"));
    assert!(discovered[1].content.contains("rule two"));
}

#[test]
fn discover_agents_files_from_roots_keeps_home_before_repo_chain() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    let repo = tempdir.path().join("repo");
    fs::create_dir_all(home.join(".agents")).expect("home agents dir");
    fs::create_dir_all(repo.join("pkg")).expect("repo pkg dir");

    let home_agents = home.join(".agents").join("AGENTS.md");
    let repo_agents = repo.join("AGENTS.md");
    let pkg_agents = repo.join("pkg").join("AGENTS.md");
    fs::write(&home_agents, "# Home\n- personal rule\n").expect("write home");
    fs::write(&repo_agents, "# Repo\n- repo rule\n").expect("write repo");
    fs::write(&pkg_agents, "# Package\n- package rule\n").expect("write pkg");

    let discovered = discover_agents_files_from_roots(vec![
        home.join(".agents"),
        repo.clone(),
        repo.join("pkg"),
    ]);

    let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            home_agents.canonicalize().expect("canonical home"),
            repo_agents.canonicalize().expect("canonical repo"),
            pkg_agents.canonicalize().expect("canonical pkg"),
        ]
    );
}

#[test]
fn session_started_emits_ready_after_startup() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: tau_proto::SessionStartReason::Initial,
        }))
        .expect("request");
    writer.flush().expect("flush");

    loop {
        let event = reader.read_event().expect("read").expect("context event");
        if event.name() == EventName::EXTENSION_CONTEXT_READY {
            let Event::ExtensionContextReady(ready) = event else {
                unreachable!("matched on event name");
            };
            assert_eq!(ready.session_id, "s1");
            break;
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_reads_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("README.txt");
    fs::write(&file_path, "hello from file").expect("write fixture");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(file_path.display().to_string()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, READ_TOOL_NAME);
    assert_eq!(
        optional_argument_text(&result.result, "content"),
        Some("hello from file".to_owned())
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_read_missing_file_reports_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("/definitely/missing/file.txt".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(!error.message.contains("failed to read"));
    assert!(error.message.contains("No such file or directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_writes_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("output.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(WRITE_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("content".to_owned()),
                    CborValue::Text("written content".to_owned()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, WRITE_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "written content"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_write_missing_parent_reports_short_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_parent = tempdir.path().join("missing-parent");
    let file_path = missing_parent.join("child.txt");
    fs::write(&missing_parent, "not a dir").expect("write blocker");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(WRITE_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("content".to_owned()),
                    CborValue::Text("x".to_owned()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, WRITE_TOOL_NAME);
    assert!(!error.message.contains("failed to create directories"));
    assert!(!error.message.contains(file_path.to_string_lossy().as_ref()));
    assert!(error.message.contains("Not a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_write_directory_reports_short_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(WRITE_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("/tmp".to_owned()),
                ),
                (
                    CborValue::Text("content".to_owned()),
                    CborValue::Text("x".to_owned()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, WRITE_TOOL_NAME);
    assert!(!error.message.contains("failed to write"));
    assert!(error.message.contains("Is a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_writes_file_creates_directories() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("a/b/c/deep.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(WRITE_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("content".to_owned()),
                    CborValue::Text("deep content".to_owned()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "deep content"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_updates_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-1".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "after\n"
    );
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nM {}",
            file_path.display()
        ))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_reports_context_mismatch_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-missing\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-2".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to find expected lines"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_move_renames_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let src = tempdir.path().join("old.txt");
    let dst = tempdir.path().join("new.txt");
    fs::write(&src, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
        src.display(),
        dst.display()
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-3".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert!(!src.exists(), "source path should be removed after move");
    assert_eq!(fs::read_to_string(&dst).expect("read back"), "after\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_operations() {
    let tempdir = TempDir::new().expect("tempdir");
    let add_path = tempdir.path().join("nested/new.txt");
    let modify_path = tempdir.path().join("modify.txt");
    let delete_path = tempdir.path().join("delete.txt");
    fs::write(&modify_path, "line1\nline2\n").expect("write modify");
    fs::write(&delete_path, "obsolete\n").expect("write delete");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+created\n*** Delete File: {}\n*** Update File: {}\n@@\n-line2\n+changed\n*** End Patch",
        add_path.display(),
        delete_path.display(),
        modify_path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-4".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&add_path).expect("read added"),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(&modify_path).expect("read modified"),
        "line1\nchanged\n"
    );
    assert!(!delete_path.exists(), "deleted path should be removed");
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nA {}\nM {}\nD {}",
            add_path.display(),
            modify_path.display(),
            delete_path.display(),
        ))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_chunks() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("multi.txt");
    fs::write(&target_path, "line1\nline2\nline3\nline4\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-line2\n+changed2\n@@\n-line4\n+changed4\n*** End Patch",
        target_path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-5".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&target_path).expect("read back"),
        "line1\nchanged2\nline3\nchanged4\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_failure_after_partial_success_leaves_changes() {
    let tempdir = TempDir::new().expect("tempdir");
    let created_path = tempdir.path().join("created.txt");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        created_path.display(),
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-5b".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    let display = error.display.expect("error display");
    assert_eq!(
        display.payload,
        Some(ToolDisplayPayload::Text {
            text: format!(
                "Partial changes applied before failure:\nA {}",
                created_path.display()
            ),
        })
    );
    assert_eq!(
        fs::read_to_string(&created_path).expect("created file should remain"),
        "hello\n"
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_requires_existing_file_for_update() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-6".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert!(
        error
            .message
            .contains(missing_path.to_string_lossy().as_ref())
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_add_overwrites_existing_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("duplicate.txt");
    fs::write(&path, "old content\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+new content\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-7".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        "new content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_update_appends_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("no_newline.txt");
    fs::write(&path, "no newline at end").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-patch-8".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    let contents = fs::read_to_string(&path).expect("read back");
    assert!(contents.ends_with('\n'));
    assert_eq!(contents, "first line\nsecond line\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_read_failure_reports_short_reason() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("/definitely/missing/file.txt".to_owned()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("a".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("b".to_owned()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(!error.message.contains("failed to read"));
    assert!(error.message.contains("No such file or directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_errors_use_short_reasons() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("missing".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("x".to_owned()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(
        error.message,
        "oldText match count mismatch: expected 1, found 0; no changes written"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_errors_include_path_details() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("missing".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("x".to_owned()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(
        error.message,
        "oldText match count mismatch: expected 1, found 0; no changes written"
    );
    let details = error.details.expect("details");
    let path = cbor_map_text(&details, "path").expect("path");
    assert_eq!(path, file_path.display().to_string());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_can_replace_expected_multiple_matches() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one fish two fish\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("fish".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("cat".to_owned()),
                        ),
                        (
                            CborValue::Text("expected_matches".to_owned()),
                            CborValue::Integer(2.into()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one cat two cat\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_reports_actual_match_count_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one fish two fish\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("fish".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("cat".to_owned()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(
        error.message,
        "oldText match count mismatch: expected 1, found 2; no changes written"
    );
    let details = error.details.expect("details");
    assert_eq!(cbor_map_text(&details, "oldText"), Some("fish"));
    assert_eq!(cbor_map_int(&details, "expected_matches"), Some(1));
    assert_eq!(cbor_map_int(&details, "actual_matches"), Some(2));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one fish two fish\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_reports_zero_applied_for_expected_zero_matches() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(file_path.display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("oldText".to_owned()),
                            CborValue::Text("missing".to_owned()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("x".to_owned()),
                        ),
                        (
                            CborValue::Text("expected_matches".to_owned()),
                            CborValue::Integer(0.into()),
                        ),
                    ])]),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(cbor_map_int(&result.result, "edits_applied"), Some(0),);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_finds_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(FIND_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("pattern".to_owned()),
                    CborValue::Text("**/*.rs".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(tempdir.path().display().to_string()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, FIND_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "matches"), Some(2));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(LS_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(tempdir.path().display().to_string()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, LS_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "entries"), Some(3));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    assert!(matches!(progress, Event::ToolProgress(_)));

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, SHELL_TOOL_NAME);
    assert_eq!(
        optional_argument_text(&result.result, "stdout"),
        Some("hello".to_owned())
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn gpt_shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-gpt-shell".into(),
            tool_name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    let Event::ToolProgress(progress) = progress else {
        panic!("expected tool progress");
    };
    assert_eq!(progress.tool_name, GPT_SHELL_TOOL_NAME);

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, GPT_SHELL_TOOL_NAME);
    assert_eq!(
        optional_argument_text(&result.result, "stdout"),
        Some("hello".to_owned())
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_applies_configured_prefix_and_command() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&Frame::Message(Message::Configure(tau_proto::Configure {
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![
                    (
                        CborValue::Text("prefix".to_owned()),
                        CborValue::Array(vec![
                            CborValue::Text("env".to_owned()),
                            CborValue::Text("TAU_SHELL_PREFIX_TEST=ok".to_owned()),
                        ]),
                    ),
                    (
                        CborValue::Text("command".to_owned()),
                        CborValue::Text("sh".to_owned()),
                    ),
                ]),
            )]),
        })))
        .expect("configure");
    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf %s \"$TAU_SHELL_PREFIX_TEST\"".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let _progress = reader.read_event().expect("read").expect("progress");
    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(
        optional_argument_text(&result.result, "stdout"),
        Some("ok".to_owned())
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn command_isolation_preserves_explicit_environment() {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg("printf %s \"${TAU_EXPLICIT_ENV_TEST-unset}\"")
        .env("TAU_EXPLICIT_ENV_TEST", "ok")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::isolation::apply_command_isolation(&mut cmd);
    let output = cmd.output().expect("run env probe");
    assert!(output.status.success(), "env probe failed: {output:?}");
    assert_eq!(String::from_utf8(output.stdout).expect("utf8 stdout"), "ok");
}

#[test]
fn shell_extension_rejects_invalid_config() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&Frame::Message(Message::Configure(tau_proto::Configure {
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("prefix".to_owned()),
                    CborValue::Text("nope".to_owned()),
                )]),
            )]),
        })))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let Message::ConfigError(error) = message {
            break error;
        }
    };
    assert!(error.message.contains("invalid type"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_multiline_display_uses_short_args_and_text_payload() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf hello\nprintf world".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let output = run_command(&args, &crate::config::ShellConfig::default()).expect("run");
    assert_eq!(output.display.args, "printf hello");
    assert_eq!(
        output.display.payload,
        Some(tau_proto::ToolDisplayPayload::Text {
            text: "printf hello\nprintf world".to_owned(),
        })
    );
}

#[test]
fn shell_tool_long_display_args_are_middle_shortened() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf 12345678901234567890".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let output = run_command(&args, &crate::config::ShellConfig::default()).expect("run");
    assert_eq!(output.display.args, "printf 123┄1234567890");
    assert_eq!(output.display.payload, None);
}

#[test]
fn shell_tool_reports_failures_with_details() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("exit 7".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let _progress = reader.read_event().expect("read").expect("progress");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, SHELL_TOOL_NAME);
    assert!(error.message.contains("command exited with status 7"));
    assert!(error.details.is_some());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

// -- Truncation ---------------------------------------------------------

#[test]
fn truncate_head_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_head(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

#[test]
fn truncate_head_limits_by_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("line 1\n"));
    assert!(result.content.contains("[Showing lines 1-"));
    assert!(
        result
            .content
            .contains("Use start_line and line_count to continue reading.")
    );
    // Should not contain lines beyond the limit.
    let content_before_notice = result.content.split("\n\n[").next().unwrap_or("");
    let kept_count = content_before_notice.lines().count();
    assert!(kept_count <= MAX_OUTPUT_LINES);
}

#[test]
fn truncate_head_limits_by_bytes() {
    // Create input that's within line count but exceeds byte limit.
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\n{big_line}\nthird");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.starts_with("first"));
    assert!(result.content.contains("[Showing lines 1-"));
}

#[test]
fn grep_result_map_echoes_request_context_for_ui() {
    // The CLI renders grep completions using fields read back from
    // the tool result (pattern/path/glob/matches/output). Lock the
    // wire contract so a future shape change doesn't silently
    // regress the UI back to "grep: done".
    let with_glob = grep_result_map(
        "foo",
        "src",
        Some("*.rs"),
        Some(0),
        3,
        "src/a.rs:1:foo".to_owned(),
    );
    assert_eq!(cbor_map_text(&with_glob, "pattern"), Some("foo"));
    assert_eq!(cbor_map_text(&with_glob, "path"), Some("src"));
    assert_eq!(cbor_map_text(&with_glob, "glob"), Some("*.rs"));
    assert_eq!(cbor_int_field(&with_glob, "status"), Some(0));
    assert_eq!(cbor_int_field(&with_glob, "matches"), Some(3));
    assert_eq!(cbor_map_text(&with_glob, "output"), Some("src/a.rs:1:foo"));
    assert_eq!(cbor_int_field(&with_glob, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&with_glob, "output_bytes"), Some(14));

    // No-glob form omits the field entirely rather than emitting
    // an empty string.
    let no_glob = grep_result_map("foo", ".", None, Some(1), 0, "no matches found".to_owned());
    assert!(cbor_map_text(&no_glob, "glob").is_none());
    assert_eq!(cbor_int_field(&no_glob, "status"), Some(1));
    assert_eq!(cbor_int_field(&no_glob, "matches"), Some(0));
    assert_eq!(cbor_int_field(&no_glob, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&no_glob, "output_bytes"), Some(16));
}

#[test]
fn classify_ripgrep_stderr_recognizes_stable_prefixes() {
    // Bad regex from the agent. The trailing `error: <diagnostic>`
    // line is the useful one — the header and caret lines aren't.
    let parsed = classify_ripgrep_stderr(
        "regex parse error:\n    (?:Result<(.*Address.*TweakIdx)\n    ^\nerror: unclosed group",
    );
    assert!(
        matches!(parsed, RipgrepError::Usage { .. }),
        "got: {parsed:?}"
    );
    assert_eq!(parsed.to_string(), "regex parse error: unclosed group");
    // Missing path / file.
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)"),
        RipgrepError::NotFound,
    );
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)").to_string(),
        "no such file or directory",
    );
    // Permission denied.
    assert_eq!(
        classify_ripgrep_stderr("Permission denied (os error 13)"),
        RipgrepError::Permission,
    );
    // Anything else (genuine runtime fault) keeps the first stderr
    // line so the chip still carries a useful signal.
    assert_eq!(
        classify_ripgrep_stderr("some unfamiliar ripgrep failure").to_string(),
        "ripgrep error: some unfamiliar ripgrep failure",
    );
}

#[test]
fn command_details_value_records_stdout_and_stderr_stats() {
    let details = command_details_value(
        "echo hi".to_owned(),
        None,
        Some(0),
        "hi\nthere\n".to_owned(),
        "oops\n".to_owned(),
    );
    assert_eq!(cbor_int_field(&details, "stdout_lines"), Some(2));
    assert_eq!(cbor_int_field(&details, "stdout_bytes"), Some(9));
    assert_eq!(cbor_int_field(&details, "stderr_lines"), Some(1));
    assert_eq!(cbor_int_field(&details, "stderr_bytes"), Some(5));
}

#[test]
fn optional_argument_bool_rejects_present_non_bool_values() {
    let args = CborValue::Map(vec![(
        CborValue::Text("ignoreCase".to_owned()),
        CborValue::Text("True".to_owned()),
    )]);

    let err = optional_argument_bool(&args, "ignoreCase").expect_err("non-bool should fail");

    assert_eq!(err, "argument `ignoreCase` must be a boolean");
}

fn grep_args(pattern: &str, path: &str, extra: Vec<(CborValue, CborValue)>) -> CborValue {
    let mut entries = vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern.to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.to_owned()),
        ),
    ];
    entries.extend(extra);
    CborValue::Map(entries)
}

#[test]
fn run_grep_rejects_string_bool_argument() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\n").expect("write a");

    let args = grep_args(
        "alpha",
        &tempdir.path().display().to_string(),
        vec![(
            CborValue::Text("ignoreCase".to_owned()),
            CborValue::Text("True".to_owned()),
        )],
    );
    let err = run_grep(&args).expect_err("string bool should fail");

    assert_eq!(err.message, "argument `ignoreCase` must be a boolean");
}

#[test]
fn run_grep_counts_matches_across_directory() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\nbeta\nalpha\n").expect("write a");
    fs::write(tempdir.path().join("b.txt"), "alpha\n").expect("write b");

    let args = grep_args("alpha", &tempdir.path().display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
}

#[test]
fn run_grep_counts_matches_in_single_file() {
    // Regression: when `path` is a single file, rg drops the
    // `PATH:` prefix from each line. Without `--with-filename` the
    // match-line classifier misses every match and `matches` falls
    // back to 0 even though output clearly contains hits.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(&file, "alpha\nbeta\nalpha\ngamma\nalpha\n").expect("write");

    let args = grep_args("alpha", &file.display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains(&format!("{}:1:alpha", file.display())),
        "expected PATH:LINE:CONTENT shape, got: {output}"
    );
}

#[test]
fn run_grep_with_context_counts_only_match_lines() {
    // Context lines (`PATH-LINE-CONTENT`) must not be counted as
    // matches. Search a single file so we also exercise the
    // `--with-filename` path.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(
        &file,
        "filler 1\nfiller 2\nalpha\nfiller 3\nfiller 4\nalpha\nfiller 5\n",
    )
    .expect("write");

    let args = grep_args(
        "alpha",
        &file.display().to_string(),
        vec![(
            CborValue::Text("context".to_owned()),
            CborValue::Integer(1.into()),
        )],
    );
    let result = run_grep(&args).expect("grep").result;

    // Two matches; surrounding context lines are present in output
    // but must not inflate the count.
    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains(":3:alpha"), "first match missing: {output}");
    assert!(
        output.contains(":6:alpha"),
        "second match missing: {output}"
    );
    assert!(
        output.contains("-2-filler 2"),
        "context line missing: {output}"
    );
}

#[test]
fn truncate_tail_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_tail(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

#[test]
fn truncate_tail_keeps_last_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(
        result
            .content
            .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
    );
    assert!(result.content.contains("[Showing lines"));
    // Should not contain the very first line.
    assert!(!result.content.contains("line 1\n"));
}

#[test]
fn truncate_tail_limits_by_bytes() {
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\nsecond\n{big_line}\nlast");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("last"));
    assert!(result.content.contains("[Showing lines"));
}

#[test]
fn slice_lines_returns_requested_window() {
    let sliced = slice_lines("a\nb\nc\nd", 2, Some(2));
    assert_eq!(sliced.content, "b\nc");
    assert_eq!(sliced.start_line, 2);
    assert_eq!(sliced.line_count, 2);
}

#[test]
fn slice_lines_clamps_past_end() {
    let sliced = slice_lines("a\nb\nc", 10, Some(5));
    assert_eq!(sliced.content, "");
    assert_eq!(sliced.start_line, 10);
    assert_eq!(sliced.line_count, 0);
}

#[test]
fn read_file_honors_start_line_and_line_count() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "line 1\nline 2\nline 3\nline 4\nline 5\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer(3.into()),
        ),
    ]);
    let result = read_file(&args).expect("read").result;
    assert_eq!(
        cbor_map_text(&result, "content"),
        Some("line 2\nline 3\nline 4")
    );
    assert_eq!(cbor_int_field(&result, "start_line"), Some(2));
    assert_eq!(cbor_int_field(&result, "line_count"), Some(3));
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(5));
}

#[test]
fn read_file_reports_empty_file_as_zero_lines() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(cbor_map_text(&result, "content"), Some(""));
    assert_eq!(cbor_int_field(&result, "start_line"), Some(1));
    assert_eq!(cbor_int_field(&result, "line_count"), Some(0));
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(0));
}

#[test]
fn read_file_reports_no_trailing_newline_as_one_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("no-newline.txt");
    std::fs::write(&path, "text").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(cbor_map_text(&result, "content"), Some("text"));
    assert_eq!(cbor_int_field(&result, "start_line"), Some(1));
    assert_eq!(cbor_int_field(&result, "line_count"), Some(1));
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(1));
}

#[test]
fn read_file_rejects_invalid_line_arguments() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("start_line=0 should fail")
            .message,
        "start_line must be >= 1"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("line_count=0 should fail")
            .message,
        "line_count must be >= 1"
    );
}

#[test]
fn read_file_truncates_large_output() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("big.txt");
    let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "content").expect("content field");
    assert!(content.contains("line 1\n"));
    assert!(content.contains("[Showing lines 1-"));
    assert!(content.contains("Use start_line and line_count to continue reading."));
    assert_eq!(cbor_int_field(&result, "start_line"), Some(1));
    assert_eq!(cbor_int_field(&result, "line_count"), Some(3000));
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(3000));
}

#[test]
fn run_find_double_star_matches_top_level_files() {
    // Regression: `**/*.rs` should match both nested AND
    // top-level Rust files. `globset`'s native `**` requires one
    // path separator; we work around that in `compile_find_glob`.
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("top.rs"), "fn top() {}\n").expect("write top");
    fs::write(tempdir.path().join("src/lib.rs"), "fn nested() {}\n").expect("write nested");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write readme");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains("top.rs"),
        "top-level match missing: {output}"
    );
    assert!(
        output.contains("src/lib.rs"),
        "nested match missing: {output}"
    );
    assert!(!output.contains("README.md"));
}

#[test]
fn run_find_returns_matching_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));
}

#[test]
fn run_ls_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(tempdir.path().display().to_string()),
    )]);
    let result = run_ls(&args).expect("ls").result;

    assert_eq!(cbor_int_field(&result, "entries"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));
}
