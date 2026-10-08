//! Hermes's hook payloads, in their own process: the harness is chosen once
//! per process, and every other test runs as Claude Code.

use collide_hooks::harness;
use serde_json::json;

#[test]
fn hermes_calls_read_as_the_canonical_events_and_tools() {
    harness::set("hermes");
    let write = harness::normalize(json!({"hook_event_name": "pre_tool_call", "tool_name": "write_file",
        "tool_input": {"path": "src/app.py", "content": "x"}, "session_id": "s1", "cwd": "/repo"}));
    assert_eq!(write["hook_event_name"], "PreToolUse");
    assert_eq!(write["tool_name"], "Write");
    assert_eq!(write["tool_input"]["file_path"], "/repo/src/app.py");

    let edit = harness::normalize(json!({"hook_event_name": "post_tool_call", "tool_name": "patch",
        "tool_input": {"path": "/abs/a.py", "old_string": "a", "new_string": "b"}, "cwd": "/repo"}));
    assert_eq!(edit["tool_name"], "Edit");
    assert_eq!(edit["tool_input"]["file_path"], "/abs/a.py");

    let v4a = harness::normalize(json!({"hook_event_name": "pre_tool_call", "tool_name": "patch",
        "tool_input": {"mode": "patch", "patch": "*** Begin Patch\n*** Update File: a.py\n"}, "cwd": "/repo"}));
    assert_eq!(v4a["tool_name"], "apply_patch");
    assert!(v4a["tool_input"]["command"].as_str().unwrap().starts_with("*** Begin Patch"));

    for (event, canonical) in [("on_session_start", "SessionStart"), ("on_session_end", "Stop"),
                               ("on_session_finalize", "SessionEnd"), ("pre_llm_call", "UserPromptSubmit")] {
        assert_eq!(harness::normalize(json!({"hook_event_name": event}))["hook_event_name"], canonical);
    }

    // an allowed call says nothing: Hermes's `approve` would ask the person
    assert_eq!(harness::render_decision(true, ""), (String::new(), String::new(), 0));
    let (out, _, code) = harness::render_decision(false, "Becca renamed it");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), json!({"action": "block", "message": "Becca renamed it"}));
    assert_eq!(code, 0);
}
