use super::*;

#[test]
fn build_system_prompt_includes_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("brave-search"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: PathBuf::from("/skills/brave-search/SKILL.md"),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>brave-search</name>"));
    assert!(prompt.contains("Web search via Brave API"));
    assert!(!prompt.contains("Current date:"));
    assert!(prompt.contains("Current working directory: /tmp/work"));
}

#[test]
fn build_system_prompt_excludes_hidden_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("hidden"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Should not appear".to_owned(),
            file_path: PathBuf::from("/skills/hidden/SKILL.md"),
            add_to_prompt: false,
        },
    );
    let prompt = build_system_prompt(&skills, "/tmp/work");
    assert!(!prompt.contains("<available_skills>"));
    assert!(!prompt.contains("hidden"));
}

#[test]
fn skill_tool_reads_file_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let skill_dir = td.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing.",
    )
    .expect("write");

    let mut h = echo_harness(&sp).expect("start");

    // Manually insert a discovered skill.
    h.discovered_skills.insert(
        tau_proto::SkillName::from("my-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "A test skill".to_owned(),
            file_path: skill_file,
            add_to_prompt: true,
        },
    );

    // Directly invoke the skill tool handler.
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-skill".into()]);
    let call = AgentToolCall {
        id: "call-skill".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text("my-skill".to_owned()),
            ),
        ]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify the tool result was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_result = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Result { .. },
                ..
            })
        )
    });
    assert!(has_skill_result, "expected skill tool result in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolResult(result) if result.call_id.as_str() == "call-skill"
        )),
        "expected skill tool result in durable session event log"
    );
}

#[test]
fn skill_tool_returns_error_for_unknown_skill() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid_for_state, vec!["call-missing".into()]);
    let call = AgentToolCall {
        id: "call-missing".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text("nonexistent".to_owned()),
            ),
        ]),
        display: None,
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify a tool error was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_error = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Error { .. },
                ..
            })
        )
    });
    assert!(has_skill_error, "expected skill tool error in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolError(error) if error.call_id.as_str() == "call-missing"
        )),
        "expected skill tool error in durable session event log"
    );
}

#[test]
fn skill_load_unknown_attaches_split_name_search_suggestions() {
    // Agents routinely guess at skill names; when the load misses we
    // free-search using the requested name split on `-`/`_` so the
    // error response carries plausible alternatives.
    //
    // Use synthetic tokens that won't collide with the user's real
    // `~/.agents/skills` library that `echo_harness` discovers, then
    // wipe `discovered_skills` so the assertions are deterministic.
    const PREFIX: &str = "qzxtest";
    const TKLANG: &str = "qzxlang";
    const TKSTYLE: &str = "qzxstyle";

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let make_skill = |name: &str, description: &str| {
        let dir = td.path().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .expect("write");
        path
    };
    let lang_name = format!("{PREFIX}-{TKLANG}-helper");
    let style_name = format!("{PREFIX}-{TKSTYLE}-guide");
    let decoy_name = "totally-unrelated-skill".to_owned();
    let lang_path = make_skill(&lang_name, &format!("{TKLANG} helpers"));
    let style_path = make_skill(&style_name, &format!("{TKSTYLE} guide"));
    let decoy_path = make_skill(&decoy_name, "unrelated thing");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.clear();
    for (name, desc, path) in [
        (lang_name.clone(), format!("{TKLANG} helpers"), lang_path),
        (style_name.clone(), format!("{TKSTYLE} guide"), style_path),
        (decoy_name.clone(), "unrelated thing".to_owned(), decoy_path),
    ] {
        h.discovered_skills.insert(
            tau_proto::SkillName::from(name.as_str()),
            DiscoveredSkill {
                source_id: "skills".into(),
                description: desc,
                file_path: path,
                add_to_prompt: false,
            },
        );
    }

    let requested = format!("{PREFIX}-{TKLANG}-{TKSTYLE}");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-miss".into()]);
    let call = AgentToolCall {
        id: "call-miss".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("load".to_owned()),
            ),
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text(requested.clone()),
            ),
        ]),
        display: None,
    };
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    let events = h.store.session_events("s1").expect("session events");
    let err = events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ToolError(e) if e.call_id.as_str() == "call-miss" => Some(e),
            _ => None,
        })
        .expect("tool error");
    assert!(
        err.message.contains("unknown skill"),
        "unexpected message: {}",
        err.message
    );
    let details = err.details.as_ref().expect("details");
    let CborValue::Map(entries) = details else {
        panic!("details should be a map: {details:?}");
    };
    let get = |key: &str| {
        entries.iter().find_map(|(k, v)| match k {
            CborValue::Text(s) if s == key => Some(v),
            _ => None,
        })
    };
    assert_eq!(
        get("name"),
        Some(&CborValue::Text(requested.clone())),
        "details.name should echo the requested name"
    );
    let queries = match get("queries") {
        Some(CborValue::Array(a)) => a.clone(),
        other => panic!("queries should be array: {other:?}"),
    };
    let needles: Vec<String> = queries
        .iter()
        .filter_map(|v| match v {
            CborValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(needles, vec![PREFIX, TKLANG, TKSTYLE]);
    let matches = match get("matches") {
        Some(CborValue::Array(a)) => a.clone(),
        other => panic!("matches should be array: {other:?}"),
    };
    let match_names: Vec<String> = matches
        .iter()
        .filter_map(|m| match m {
            CborValue::Map(fields) => fields.iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    // Both helpers should be suggested (each shares two needles with
    // the requested name); the unrelated decoy must not appear.
    assert!(
        match_names.iter().any(|n| n == &lang_name),
        "expected {lang_name} in suggestions, got: {match_names:?}"
    );
    assert!(
        match_names.iter().any(|n| n == &style_name),
        "expected {style_name} in suggestions, got: {match_names:?}"
    );
    assert!(
        !match_names.iter().any(|n| n == &decoy_name),
        "unrelated decoy leaked into suggestions: {match_names:?}"
    );
}

#[test]
fn skill_tool_search_matches_name_description_and_optional_content() {
    // The search action backs progressive skill discovery: when most
    // skills are not advertised at session start, the agent must be
    // able to find them by keyword. Default scope is name +
    // description; `search_content: true` opts into grepping bodies.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    // Use unique tokens that won't collide with the user's real
    // `~/.agents/skills` library that `echo_harness` discovers during
    // eager init.
    const KW: &str = "zqxtoken";
    const BODY_KW: &str = "zqxbody";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!(
            "---\nname: zqx-alpha\ndescription: {KW} helpers\n---\nalpha body, mentions {BODY_KW}"
        ),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!(
            "---\nname: zqx-beta\ndescription: unrelated thing\n---\nbeta body, mentions {BODY_KW} too"
        ),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        "---\nname: zqx-gamma\ndescription: a different topic\n---\nno keyword references here",
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("{KW} helpers"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated thing".to_owned(),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "a different topic".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search = |query: &str, search_content: bool, id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("search".to_owned()),
            ),
            (
                CborValue::Text("query".to_owned()),
                CborValue::Text(query.to_owned()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
        ]),
        display: None,
    };

    let read_matches = |h: &Harness, call_id: &str| -> Vec<String> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                entries
                    .into_iter()
                    .find_map(|(k, v)| match (k, v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => Some(v),
                        _ => None,
                    })
                    .expect("name in match")
            })
            .collect()
    };

    // Description match: KW only appears in zqx-alpha's description.
    seed_tools_running(&mut h, &cid, vec!["call-1".into()]);
    h.handle_skill_tool_call(&cid, &call_search(KW, false, "call-1"))
        .expect("search 1");
    assert_eq!(read_matches(&h, "call-1"), vec!["zqx-alpha"]);

    // Default scope must NOT search content: BODY_KW appears only in
    // alpha and beta bodies. With search_content=false → no hits.
    seed_tools_running(&mut h, &cid, vec!["call-2".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, false, "call-2"))
        .expect("search 2");
    let empty: Vec<String> = Vec::new();
    assert_eq!(read_matches(&h, "call-2"), empty);

    // Opt into content search: now alpha and beta both match,
    // sorted alphabetically.
    seed_tools_running(&mut h, &cid, vec!["call-3".into()]);
    h.handle_skill_tool_call(&cid, &call_search(BODY_KW, true, "call-3"))
        .expect("search 3");
    assert_eq!(read_matches(&h, "call-3"), vec!["zqx-alpha", "zqx-beta"]);

    // Name match works case-insensitively.
    seed_tools_running(&mut h, &cid, vec!["call-4".into()]);
    h.handle_skill_tool_call(&cid, &call_search("ZQX-ALPHA", false, "call-4"))
        .expect("search 4");
    assert_eq!(read_matches(&h, "call-4"), vec!["zqx-alpha"]);
}

#[test]
fn skill_tool_search_accepts_multiple_terms_and_ranks_by_hit_count() {
    // Multi-term search: the agent fires several plausible terms at
    // once, the harness scores each skill by how many terms matched
    // it, and returns hits sorted by score descending. Ties break on
    // name to keep the output deterministic.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    const T1: &str = "zqxalpha";
    const T2: &str = "zqxbeta";

    let alpha_dir = td.path().join("zqx-alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir");
    let alpha_file = alpha_dir.join("SKILL.md");
    std::fs::write(
        &alpha_file,
        format!("---\nname: zqx-alpha\ndescription: matches {T1} and {T2}\n---\nbody"),
    )
    .expect("write alpha");

    let beta_dir = td.path().join("zqx-beta");
    std::fs::create_dir_all(&beta_dir).expect("mkdir");
    let beta_file = beta_dir.join("SKILL.md");
    std::fs::write(
        &beta_file,
        format!("---\nname: zqx-beta\ndescription: matches only {T1}\n---\nbody"),
    )
    .expect("write beta");

    let gamma_dir = td.path().join("zqx-gamma");
    std::fs::create_dir_all(&gamma_dir).expect("mkdir");
    let gamma_file = gamma_dir.join("SKILL.md");
    std::fs::write(
        &gamma_file,
        "---\nname: zqx-gamma\ndescription: unrelated\n---\nbody",
    )
    .expect("write gamma");

    let mut h = echo_harness(&sp).expect("start");
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-alpha"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches {T1} and {T2}"),
            file_path: alpha_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-beta"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: format!("matches only {T1}"),
            file_path: beta_file,
            add_to_prompt: false,
        },
    );
    h.discovered_skills.insert(
        tau_proto::SkillName::from("zqx-gamma"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "unrelated".to_owned(),
            file_path: gamma_file,
            add_to_prompt: false,
        },
    );

    let cid = h.default_conversation_id.clone();
    let call_search_array = |terms: &[&str], id: &str| AgentToolCall {
        id: id.into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("action".to_owned()),
                CborValue::Text("search".to_owned()),
            ),
            (
                CborValue::Text("query".to_owned()),
                CborValue::Array(
                    terms
                        .iter()
                        .map(|t| CborValue::Text((*t).to_owned()))
                        .collect(),
                ),
            ),
        ]),
        display: None,
    };

    let read_match_records = |h: &Harness, call_id: &str| -> Vec<(String, u64)> {
        let events = h.store.session_events("s1").expect("events");
        let result = events
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                Event::ToolResult(r) if r.call_id.as_str() == call_id => Some(r.result.clone()),
                _ => None,
            })
            .expect("tool result");
        let CborValue::Map(top) = result else {
            panic!("result must be a map")
        };
        let matches = top
            .iter()
            .find_map(|(k, v)| match (k, v) {
                (CborValue::Text(k), CborValue::Array(arr)) if k == "matches" => Some(arr.clone()),
                _ => None,
            })
            .expect("matches array");
        matches
            .into_iter()
            .map(|m| {
                let CborValue::Map(entries) = m else {
                    panic!("match must be a map")
                };
                let mut name = None;
                let mut hits: Option<u64> = None;
                for (k, v) in entries {
                    match (&k, &v) {
                        (CborValue::Text(k), CborValue::Text(v)) if k == "name" => {
                            name = Some(v.clone());
                        }
                        (CborValue::Text(k), CborValue::Integer(i)) if k == "hit_count" => {
                            let n: i128 = (*i).into();
                            hits = Some(n as u64);
                        }
                        _ => {}
                    }
                }
                (name.expect("name"), hits.expect("hit_count"))
            })
            .collect()
    };

    seed_tools_running(&mut h, &cid, vec!["call-multi".into()]);
    h.handle_skill_tool_call(&cid, &call_search_array(&[T1, T2], "call-multi"))
        .expect("multi search");
    let records = read_match_records(&h, "call-multi");
    assert_eq!(
        records,
        vec![("zqx-alpha".to_owned(), 2), ("zqx-beta".to_owned(), 1),],
        "alpha matches both terms (rank 2), beta matches one (rank 1), \
         gamma matches none and must be filtered out",
    );

    // A single-element array must behave the same as a single string —
    // the per-term matcher is unchanged.
    seed_tools_running(&mut h, &cid, vec!["call-single".into()]);
    h.handle_skill_tool_call(&cid, &call_search_array(&[T2], "call-single"))
        .expect("single in array");
    assert_eq!(
        read_match_records(&h, "call-single"),
        vec![("zqx-alpha".to_owned(), 1)],
    );

    // Empty array should error rather than silently returning every
    // skill — the agent passing `[]` is almost always a bug.
    seed_tools_running(&mut h, &cid, vec!["call-empty".into()]);
    h.handle_skill_tool_call(
        &cid,
        &AgentToolCall {
            id: "call-empty".into(),
            name: "skill".into(),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("search".to_owned()),
                ),
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Array(Vec::new()),
                ),
            ]),
            display: None,
        },
    )
    .expect("call");
    let events = h.store.session_events("s1").expect("events");
    let saw_error = events.iter().rev().any(|entry| {
        matches!(
            &entry.event,
            Event::ToolError(e) if e.call_id.as_str() == "call-empty"
        )
    });
    assert!(saw_error, "empty query array must produce a ToolError");
}

#[test]
fn skill_tool_unknown_action_returns_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.default_conversation_id.clone();
    seed_tools_running(&mut h, &cid, vec!["call-bogus".into()]);
    let call = AgentToolCall {
        id: "call-bogus".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![(
            CborValue::Text("action".to_owned()),
            CborValue::Text("invoke".to_owned()),
        )]),
        display: None,
    };
    h.handle_skill_tool_call(&cid, &call).expect("dispatch");
    let events = h.store.session_events("s1").expect("events");
    let err = events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ToolError(e) if e.call_id.as_str() == "call-bogus" => Some(e.message.clone()),
            _ => None,
        })
        .expect("tool error");
    assert!(
        err.contains("unknown skill action"),
        "unexpected error message: {err}"
    );
}

#[test]
fn skill_tool_registered_in_tool_list() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let h = echo_harness(&sp).expect("start");
    let defs = h.gather_tool_definitions();
    assert!(
        defs.iter().any(|d| d.name == "skill"),
        "skill tool should be registered; got: {:?}",
        defs.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
}
