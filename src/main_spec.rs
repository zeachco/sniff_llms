use crate::chat::Chat;
use crate::chat::turn::SegKind;
use crate::flow::CapturedRequest;
use crate::transcript::{render_chat, stats_text, tool_calls_text};
use crate::ui::TOOL_ORANGE;
use serde_json::json;
use crate::wire::{
    classify, detect_backend, Dir, is_llm_response, looks_like_llm, Packet, thread_key_from_body,
    thread_key_from_headers, user_agent_from_headers,
};
use crate::text::{first_chars, format_duration, last_chars, middle_elide};
use super::*;

#[test]
fn parses_repeated_and_comma_separated_interfaces() {
    let argv =
        |words: &[&str]| -> Vec<String> { words.iter().map(|word| word.to_string()).collect() };
    assert_eq!(
        requested_interfaces(&argv(&[
            "sniff-llms",
            "-i",
            "lo0,en0",
            "--interface",
            "utun3",
            "-i",
            "lo0"
        ])),
        ["lo0", "en0", "utun3"]
    );
    assert!(requested_interfaces(&argv(&["sniff-llms"])).is_empty());
}

#[test]
fn finds_helpers_on_path_or_in_an_install_prefix() {
    assert!(resolve_binary("/bin/sh").is_some());
    assert!(resolve_binary("sh").is_some());
    assert!(resolve_binary("definitely-not-a-real-helper").is_none());
}

#[test]
fn elides_the_middle_of_long_chat_ids() {
    assert_eq!(middle_elide("abcdefghijklmnopqrstuv", 11), "abcd...stuv");
    assert_eq!(middle_elide("short", 11), "short");
}

#[test]
fn retains_usage_only_stream_chunks() {
    let mut app = App::new();
    app.flows
        .entry("client <-> server".into())
        .or_default()
        .requests
        .push(CapturedRequest {
            body: json!({
                "model": "qwen3.8",
                "messages": []
            }),
            thread: None,
            user_agent: None,
        });
    app.response(
        "client <-> server",
        json!({
            "id": "chatcmpl-1234567890",
            "choices": [{"delta": {"content": "hello"}}]
        }),
    );
    app.response(
        "client <-> server",
        json!({
            "id": "chatcmpl-1234567890",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        }),
    );

    let chat = &app.chats[0];
    assert_eq!(chat.model, "qwen3.8");
    assert_eq!(chat.chunks(), 2);
    assert!(stats_text(chat).contains("prompt 10 | completion 2 | total 12"));
}

#[test]
fn groups_same_chat_id_across_ports_without_duplicating_stream() {
    let mut app = App::new();
    app.response(
        "client:5000 <-> router:8000",
        delta_response("c1", json!({"content": "hello"}), None),
    );
    app.response(
        "router:5001 <-> server:9000",
        delta_response("c1", json!({"content": "hello"}), None),
    );

    assert_eq!(app.chats.len(), 1, "same id must have only one tab");
    assert_eq!(
        app.chats[0].last().answer,
        "hello",
        "mirrored chunk must be ignored"
    );
    assert_eq!(
        app.chats[0].chunks(),
        1,
        "mirrored chunk must not affect stats"
    );
}

#[test]
fn detects_backend_from_response_markers() {
    assert_eq!(
        detect_backend(&json!({"timings": {"predicted_per_second": 1.0}})),
        Some("llama.cpp")
    );
    assert_eq!(
        detect_backend(
            &json!({"id": "c1", "choices": [], "prompt_eval_count": 10, "eval_count": 5})
        ),
        Some("ollama")
    );
    assert_eq!(
        detect_backend(&json!({"id": "gen-abc123", "choices": []})),
        Some("openrouter")
    );
    assert_eq!(
        detect_backend(&json!({"id": "msg_01abc", "type": "message"})),
        Some("anthropic")
    );
    assert_eq!(
        detect_backend(
            &json!({"id": "chatcmpl-1", "choices": [], "system_fingerprint": "abc"})
        ),
        None
    );
}

#[test]
fn tab_bar_shows_overflow_indicators_and_keeps_selected_visible() {
    let style = Style::default();
    let items: Vec<(String, Style)> = (0..6).map(|i| (format!(" tab{i} ● "), style)).collect();
    let text = build_tab_line(&items, 3, 30).to_string();
    assert!(
        text.contains("tab3"),
        "selected tab must be visible: {text}"
    );
    assert!(text.contains("+"), "overflow indicator expected: {text}");
}

#[test]
fn tab_bar_fits_all_tabs_when_wide_enough() {
    let style = Style::default();
    let items: Vec<(String, Style)> = (0..3).map(|i| (format!(" t{i} ● "), style)).collect();
    let text = build_tab_line(&items, 1, 200).to_string();
    assert!(text.contains("t0") && text.contains("t1") && text.contains("t2"));
    assert!(!text.contains("+"), "no overflow expected: {text}");
}

#[test]
fn auto_clean_closes_chats_matching_finish_reason() {
    let mut app = App::new();
    app.auto_clean = AutoClean::ToolCalls;
    app.response(
        "f",
        delta_response("c1", json!({"content": "hi"}), Some("tool_calls")),
    );
    assert!(app.chats.is_empty(), "chat should be auto-cleaned");
    assert!(app.ignored.contains("c1"));
}

#[test]
fn auto_clean_off_keeps_finished_chats() {
    let mut app = App::new();
    app.auto_clean = AutoClean::Never;
    app.response(
        "f",
        delta_response("c1", json!({}), Some("tool_calls")),
    );
    assert_eq!(app.chats.len(), 1);
}

/// Push a chat's last chunk out of the live-grace window, so it reads as a
/// conversation nobody touched for a while.
fn make_idle(app: &mut App, id: &str) {
    let chat = app
        .chats
        .iter_mut()
        .find(|chat| chat.id() == id)
        .expect("chat to age");
    chat.last_activity = Instant::now() - Duration::from_secs(120);
}

#[test]
fn clean_done_keeps_only_running_chats() {
    let mut app = App::new();
    app.response(
        "f1",
        delta_response("streaming", json!({"content": "hi"}), None),
    );
    app.response(
        "f2",
        delta_response(
            "awaiting_tools",
            json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "bash"}}]}),
            Some("tool_calls"),
        ),
    );
    app.response(
        "f3",
        delta_response("finished", json!({"content": "bye"}), Some("stop")),
    );
    make_idle(&mut app, "finished");

    app.clean_done();

    let ids: Vec<&str> = app.chats.iter().map(|chat| chat.id()).collect();
    assert_eq!(
        ids,
        ["streaming", "awaiting_tools"],
        "streaming and tool-waiting chats stay, finished ones go"
    );
    assert!(app.ignored.contains("finished"), "cleaned id is forgotten");
    assert!(app.status.contains("cleaned 1 finished"), "{}", app.status);
}

#[test]
fn clean_done_keeps_a_session_that_stopped_moments_ago() {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"content": "bye"}), Some("stop")),
    );

    app.clean_done();

    assert_eq!(
        app.chats.len(),
        1,
        "a chat inside the grace window is still a live session"
    );
    assert!(app.status.contains("nothing finished"), "{}", app.status);
}

#[test]
fn clean_done_reparents_the_children_of_a_cleaned_parent() {
    let mut app = App::new();
    app.response(
        "f",
        subagent_parent_response("p1", "review the Caverace constructor wiring"),
    );
    app.response(
        "g",
        delta_response("s1", json!({"content": "reviewing"}), None),
    );
    // The parent collects the child's result and closes its own conversation.
    app.response(
        "f",
        delta_response("p2", json!({"content": "done"}), Some("stop")),
    );
    make_idle(&mut app, "p1");
    assert_eq!(app.chats[1].parent.as_deref(), Some("p1"));

    app.clean_done();

    assert_eq!(app.chats.len(), 1, "the running child survives");
    assert_eq!(app.chats[0].id(), "s1");
    assert!(app.chats[0].parent.is_none(), "orphans become roots");
}

#[test]
fn clean_done_clamps_the_selection() {
    let mut app = App::new();
    for i in 0..3 {
        app.response(
            "f",
            delta_response(&format!("c{i}"), json!({"content": "x"}), Some("stop")),
        );
        make_idle(&mut app, &format!("c{i}"));
    }
    app.selected = 2;

    app.clean_done();

    assert!(app.chats.is_empty());
    assert_eq!(app.selected, 0);
}

#[test]
fn new_chats_do_not_steal_selection() {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"content": "a"}), None),
    );
    app.response(
        "f",
        delta_response("c2", json!({"content": "b"}), None),
    );
    assert_eq!(app.selected, 0, "selection should stay on the first chat");
    assert_eq!(app.chats.len(), 2);
}

#[test]
fn concatenates_streamed_tool_call_argument_fragments() {
    let mut app = App::new();
    // Two deltas stream the same tool call's arguments in pieces.
    app.response(
        "f",
        json!({
            "id": "c1",
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "get_weather", "arguments": "{\"city\":"}}
            ]}}]
        }),
    );
    app.response(
        "f",
        json!({
            "id": "c1",
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"Paris\"}"}}
            ]}}]
        }),
    );
    let chat = &app.chats[0];
    let calls: Vec<_> = chat.tool_calls().collect();
    assert_eq!(
        calls.len(),
        1,
        "must be one call, not one line per fragment"
    );
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].arguments, "{\"city\":\"Paris\"}");
    assert!(stats_text(chat).contains("Tool calls: 1"));
}

#[test]
fn compacts_pretty_printed_tool_call_arguments() {
    let mut app = App::new();
    app.response(
        "f",
        json!({
            "id": "c1",
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1",
                 "function": {"name": "read_file",
                              "arguments": "{\n  \"path\": \"/tmp/agent-home/.local/share/agent/workspace\"\n}"}}
            ]}}]
        }),
    );
    let chat = &app.chats[0];
    let text = tool_calls_text(chat.last());
    let lines: Vec<&str> = text.lines().collect();
    // Pretty-printed arguments must collapse to a single compact line.
    let open_braces = lines
        .iter()
        .filter(|l| l.trim_start().starts_with('{'))
        .count();
    assert_eq!(open_braces, 1, "args should be one line: {lines:?}");
    assert!(
        !lines.iter().any(|l| l.trim() == "}"),
        "no orphaned brace: {lines:?}"
    );
    assert!(text.contains("\"path\""));
    assert!(text.contains("/tmp/agent-home"));
}

#[test]
fn rename_commit_sets_and_clears_name() {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"content": "a"}), None),
    );
    app.rename = Some("my chat".into());
    app.rename_commit();
    assert_eq!(app.chats[0].name.as_deref(), Some("my chat"));
    app.rename = Some("   ".into());
    app.rename_commit();
    assert!(app.chats[0].name.is_none());
}

#[test]
fn char_helpers() {
    assert_eq!(first_chars("abcdef", 3), "abc");
    assert_eq!(last_chars("abcdef", 3), "def");
    assert_eq!(last_chars("ab", 5), "ab");
}

#[test]
fn classify_directions_by_http_and_json_shape() {
    assert_eq!(
        classify("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream"),
        Some(Dir::Response)
    );
    assert_eq!(
        classify("POST /v1/chat/completions HTTP/1.1"),
        Some(Dir::Request)
    );
    assert_eq!(
        classify("data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"),
        Some(Dir::Response)
    );
    assert_eq!(
        classify("{\"model\":\"qwen\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"),
        Some(Dir::Request)
    );
    assert_eq!(classify("GET /index.html HTTP/1.1"), Some(Dir::Request));
    assert_eq!(classify("garbage that is not llm"), None);
}

#[test]
fn llm_response_requires_id_and_body() {
    assert!(is_llm_response(
        &json!({"id":"c1","choices":[{"delta":{}}]})
    ));
    assert!(is_llm_response(&json!({"id":"c1","choices":[],"usage":{}})));
    assert!(
        !is_llm_response(&json!({"choices":[{"delta":{}}]})),
        "missing id"
    );
    assert!(
        !is_llm_response(&json!({"id":"c1","messages":[]})),
        "missing body"
    );
}

#[test]
fn prefilter_keeps_llm_traffic_only() {
    assert!(looks_like_llm("POST /v1/chat/completions HTTP/1.1"));
    assert!(looks_like_llm("{\"choices\":[],\"id\":\"c1\"}"));
    assert!(looks_like_llm("{\"messages\":[{\"role\":\"user\"}]}"));
    assert!(!looks_like_llm("GET /index.html HTTP/1.1"));
}

#[test]
fn all_ports_packet_routes_by_content_not_port() {
    let mut app = App::new();
    // Request from client 5555 to server 8080 (no fixed port configured).
    app.packet(Packet {
        header: "T 192.168.1.50:5555 -> 192.168.1.50:8080".into(),
        payload: "{\"model\":\"qwen\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"
            .into(),
    });
    // Response back from the same server endpoint.
    app.packet(Packet {
        header: "T 192.168.1.50:8080 -> 192.168.1.50:5555".into(),
        payload: "{\"id\":\"c1\",\"model\":\"qwen\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}".into(),
    });
    assert_eq!(app.chats.len(), 1, "one chat expected");
    assert_eq!(app.chats[0].id(), "c1");
    assert_eq!(app.chats[0].model, "qwen");
    // The non-LLM flow must not have been created.
    assert!(
        !app.flows.keys().any(|k| !k.contains("8080")),
        "no stray flows"
    );
}

fn first_request() -> Value {
    json!({
        "model": "qwen",
        "messages": [{"role": "user", "content": "find the file"}]
    })
}
fn continuation_request() -> Value {
    json!({
        "model": "qwen",
        "messages": [
            {"role": "user", "content": "find the file"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"file\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "found /tmp/file"}
        ]
    })
}

#[test]
fn continuation_after_tool_call_extends_the_same_chat() {
    let mut app = App::new();
    app.packet(request_body(5000, first_request()));
    app.packet(response_packet(
        5000,
        tool_call_response("c1", "call_1", "search", "{\"q\":\"file\"}"),
    ));
    // The client comes back on a fresh connection with the tool result.
    app.packet(request_body(5001, continuation_request()));
    app.packet(response_packet(
        5001,
        delta_response("c2", json!({"content": "the file is /tmp/file"}), Some("stop")),
    ));

    assert_eq!(app.chats.len(), 1, "continuation must reuse the tab");
    let chat = &app.chats[0];
    assert_eq!(chat.id(), "c1", "the tab keeps the first session id");
    assert_eq!(chat.turns.len(), 2);
    assert_eq!(chat.turns[1].id, "c2");
    assert_eq!(chat.last().answer, "the file is /tmp/file");
    let dump = render_chat(chat);
    assert!(dump.contains("=== TURN 2 ==="));
    assert!(
        dump.contains("found /tmp/file"),
        "only the new messages are rendered"
    );
    assert_eq!(
        dump.matches("find the file").count(),
        1,
        "history is not repeated"
    );
}

#[test]
fn continuation_matches_by_tool_call_id_without_a_previous_request() {
    let mut app = App::new();
    // The previous request was never captured (capture started mid-flight).
    app.response(
        "f",
        tool_call_response("c1", "call_1", "search", "{}"),
    );
    // The new request has different leading messages (dynamic system
    // prompt) but echoes the tool-call id emitted by turn 1.
    app.packet(Packet {
        header: "T 127.0.0.1:5001 -> 127.0.0.1:8080".into(),
        payload: json!({
            "model": "qwen",
            "messages": [
                {"role": "system", "content": "context changed"},
                {"role": "user", "content": "find the file"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "search", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "found"}
            ]
        })
        .to_string(),
    });
    app.response(
        &flow_key(5001),
        delta_response("c2", json!({"content": "done"}), None),
    );

    assert_eq!(
        app.chats.len(),
        1,
        "the tool-call id must regroup the session"
    );
    assert_eq!(app.chats[0].turns.len(), 2);
}

fn request_start(client_port: u16, headers: &[(&str, &str)]) -> Packet {
    let mut payload = String::from("POST /v1/chat/completions HTTP/1.1\r\n");
    for (name, value) in headers {
        payload.push_str(&format!("{name}: {value}\r\n"));
    }
    payload.push_str("\r\n");
    Packet {
        header: format!("T 127.0.0.1:{client_port} -> 127.0.0.1:8080"),
        payload,
    }
}
fn request_body(client_port: u16, body: Value) -> Packet {
    Packet {
        header: format!("T 127.0.0.1:{client_port} -> 127.0.0.1:8080"),
        payload: body.to_string(),
    }
}
fn delta_response(id: &str, delta: Value, finish: Option<&str>) -> Value {
    let mut choice = json!({ "delta": delta });
    if let Some(f) = finish {
        choice["finish_reason"] = json!(f);
    }
    json!({ "id": id, "choices": [choice] })
}
fn response_packet(client_port: u16, body: Value) -> Packet {
    Packet {
        header: format!("T 127.0.0.1:8080 -> 127.0.0.1:{client_port}"),
        payload: body.to_string(),
    }
}

/// A finished response whose only output is one tool call.
fn tool_call_response(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    delta_response(
        id,
        json!({"tool_calls": [
            {"index": 0, "id": call_id, "function": {"name": name, "arguments": arguments}}
        ]}),
        Some("tool_calls"),
    )
}
fn subagent_parent_response(id: &str, brief: &str) -> Value {
    let arguments = json!({"agent": "reviewer", "task": brief}).to_string();
    tool_call_response(id, &format!("call_{id}"), "subagent", &arguments)
}
/// The flow key of a loopback client on `port` talking to the server on 8080.
fn flow_key(port: u16) -> String {
    format!("127.0.0.1:{port} <-> 127.0.0.1:8080")
}

#[test]
fn subagent_generation_is_grouped_under_its_parent() {
    let mut app = App::new();
    app.response(
        "f",
        subagent_parent_response("p1", "review the Caverace constructor wiring"),
    );
    // The child request was not captured; grouping falls back to the open
    // subagent tool call on the recent parent.
    app.response(
        "g",
        delta_response("s1", json!({"content": "reviewing"}), None),
    );

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[0].id(), "p1");
    assert_eq!(app.chats[1].id(), "s1");
    assert_eq!(app.chats[1].parent.as_deref(), Some("p1"));
    assert_eq!(app.selected, 0, "grouping must not steal the selection");
}

#[test]
fn subagent_request_groups_to_the_parent_with_matching_brief() {
    let mut app = App::new();
    let brief_a = "review the login handler and report the token refresh bug";
    let brief_b = "review the Caverace constructor and report the missing callbacks";
    app.response("a", subagent_parent_response("p1", brief_a));
    app.response("b", subagent_parent_response("p2", brief_b));

    app.packet(request_start(5000, &[]));
    app.packet(request_body(
        5000,
        json!({
            "model": "qwen",
            "messages": [
                {"role": "system", "content": "You are a code review subagent. Review what you were asked to review."},
                {"role": "user", "content": brief_b}
            ]
        }),
    ));
    app.response(
        &flow_key(5000),
        delta_response("s1", json!({"content": "reviewing"}), None),
    );

    let child = app
        .chats
        .iter()
        .find(|c| c.id() == "s1")
        .expect("child chat");
    assert_eq!(
        child.parent.as_deref(),
        Some("p2"),
        "brief must pick the right parent"
    );
}

#[test]
fn shared_thread_id_is_disambiguated_by_tool_call_id() {
    let mut app = App::new();
    app.response(
        "f",
        tool_call_response("p1", "call_p", "search", "{}"),
    );
    app.response(
        "g",
        tool_call_response("s1", "call_s", "search", "{}"),
    );
    // A harness may send the same root session id for a parent and its
    // subagent. Content must still route each follow-up to its own chat.
    for chat in &mut app.chats {
        chat.thread = Some("thread:root-abcdefgh".into());
    }
    let next_request = |tool_id: &str| {
        json!({
            "model": "qwen",
            "prompt_cache_key": "root-abcdefgh",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": tool_id, "type": "function",
                    "function": {"name": "search", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": tool_id, "content": "ok"}
            ]
        })
    };
    app.packet(request_start(5000, &[]));
    app.packet(request_body(5000, next_request("call_s")));
    app.response(
        &flow_key(5000),
        delta_response("c3", json!({"content": "child continued"}), None),
    );
    app.packet(request_start(5001, &[]));
    app.packet(request_body(5001, next_request("call_p")));
    app.response(
        &flow_key(5001),
        delta_response("c4", json!({"content": "parent continued"}), None),
    );

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[0].turns.len(), 2);
    assert_eq!(app.chats[0].last().answer, "parent continued");
    assert_eq!(app.chats[1].turns.len(), 2);
    assert_eq!(app.chats[1].last().answer, "child continued");
}

#[test]
fn normal_conversation_is_not_grouped_under_a_waiting_parent() {
    let mut app = App::new();
    app.response(
        "f",
        subagent_parent_response("p1", "review the Caverace constructor wiring"),
    );
    app.packet(request_start(5000, &[]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "messages": [{"role": "user", "content": "unrelated question"}]}),
    ));
    app.response(
        &flow_key(5000),
        delta_response("c2", json!({"content": "answer"}), None),
    );

    assert_eq!(app.chats.len(), 2);
    assert!(
        app.chats.iter().all(|c| c.parent.is_none()),
        "only subagent-shaped requests may be grouped"
    );
}

#[test]
fn closed_parent_reparents_its_children() {
    let mut app = App::new();
    app.response(
        "f",
        subagent_parent_response("p1", "review the Caverace constructor wiring"),
    );
    app.response(
        "g",
        delta_response("s1", json!({"content": "reviewing"}), None),
    );
    app.selected = 0;
    app.close();

    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].id(), "s1");
    assert!(app.chats[0].parent.is_none(), "orphans become roots");
}

#[test]
fn mirrored_leg_contributes_thread_id_and_request() {
    let mut app = App::new();
    // Router backend leg wins the response id first, with no request.
    app.response(
        "backend <-> upstream",
        delta_response("c1", json!({"content": "hello"}), None),
    );
    assert!(app.chats[0].thread.is_none());
    // Client leg carries the declared session and the request body.
    app.packet(request_start(5000, &[("x-session-id", "thread-abcdefgh")]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
    ));
    app.response(
        &flow_key(5000),
        delta_response("c1", json!({"content": "hello"}), None),
    );

    assert_eq!(app.chats.len(), 1);
    assert_eq!(
        app.chats[0].thread.as_deref(),
        Some("thread:thread-abcdefgh")
    );
    assert!(app.chats[0].last().request.is_some(), "request adopted");
    assert_eq!(
        app.chats[0].last().answer,
        "hello",
        "mirror not appended twice"
    );
}

#[test]
fn request_user_agent_labels_the_chat_and_the_stats_line() {
    let mut app = App::new();
    app.packet(request_start(5000, &[("User-Agent", "pi/1.4.0")]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}]}),
    ));
    app.response(
        &flow_key(5000),
        delta_response("c1", json!({"content": "hello"}), Some("stop")),
    );

    assert_eq!(app.chats[0].client.as_deref(), Some("pi/1.4.0"));
    assert!(stats_text(&app.chats[0]).contains("Client: pi/1.4.0"));

    let mut no_agent = App::new();
    no_agent.response(
        "client <-> server",
        delta_response("c2", json!({"content": "x"}), Some("stop")),
    );
    assert!(no_agent.chats[0].client.is_none());
}

#[test]
fn header_thread_id_groups_requests_across_connections() {
    let mut app = App::new();
    app.packet(request_start(5000, &[("x-session-id", "thread-abcdefgh")]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "messages": [{"role": "user", "content": "start"}]}),
    ));
    app.packet(response_packet(
        5000,
        delta_response("c1", json!({"content": "one"}), Some("stop")),
    ));
    // A separate connection, a different request body, same declared thread.
    app.packet(request_start(5001, &[("X-Session-Id", "thread-abcdefgh")]));
    app.packet(request_body(
        5001,
        json!({"model": "qwen", "messages": [
            {"role": "user", "content": "totally different opening"},
            {"role": "assistant", "content": "one"},
            {"role": "user", "content": "follow up"}
        ]}),
    ));
    app.packet(response_packet(
        5001,
        delta_response("c2", json!({"content": "two"}), Some("stop")),
    ));

    assert_eq!(
        app.chats.len(),
        1,
        "the declared thread id is authoritative"
    );
    assert_eq!(app.chats[0].turns.len(), 2);
    assert_eq!(
        app.chats[0].thread.as_deref(),
        Some("thread:thread-abcdefgh")
    );
}

#[test]
fn different_header_thread_ids_never_merge_even_with_matching_tool_ids() {
    let mut app = App::new();
    app.packet(request_start(5000, &[("x-session-id", "thread-aaaaaaaa")]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "messages": [{"role": "user", "content": "a"}]}),
    ));
    app.packet(response_packet(
        5000,
        tool_call_response("c1", "call_1", "search", "{}"),
    ));
    // Same tool-call id, but the client declares another thread.
    app.packet(request_start(5001, &[("x-session-id", "thread-bbbbbbbb")]));
    app.packet(request_body(
        5001,
        json!({"model": "qwen", "messages": [
            {"role": "user", "content": "b"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "search", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "found"}
        ]}),
    ));
    app.packet(response_packet(
        5001,
        delta_response("c2", json!({"content": "two"}), None),
    ));

    assert_eq!(
        app.chats.len(),
        2,
        "different declared threads must stay apart"
    );
}

#[test]
fn prompt_cache_key_groups_requests_as_a_thread() {
    let mut app = App::new();
    let key = "cache-key-12345678";
    app.packet(request_start(5000, &[]));
    app.packet(request_body(
        5000,
        json!({"model": "qwen", "prompt_cache_key": key, "messages": [{"role": "user", "content": "start"}]}),
    ));
    app.packet(response_packet(
        5000,
        delta_response("c1", json!({"content": "one"}), None),
    ));
    app.packet(request_start(5001, &[]));
    app.packet(request_body(
        5001,
        json!({"model": "qwen", "prompt_cache_key": key, "messages": [{"role": "user", "content": "unrelated"}]}),
    ));
    app.packet(response_packet(
        5001,
        delta_response("c2", json!({"content": "two"}), None),
    ));

    assert_eq!(app.chats.len(), 1);
    assert_eq!(
        app.chats[0].thread.as_deref(),
        Some("thread:cache-key-12345678")
    );
}

#[test]
fn thread_keys_ignore_generic_short_values() {
    assert_eq!(
        thread_key_from_headers(
            "POST /v1/chat/completions HTTP/1.1\r\nx-session-id: abcdefgh\r\n\r\n"
        ),
        Some("thread:abcdefgh".into())
    );
    assert_eq!(
        thread_key_from_headers(
            "POST /v1/chat/completions HTTP/1.1\r\nx-session-id: default\r\n\r\n"
        ),
        None
    );
    assert_eq!(
        thread_key_from_body(&json!({"prompt_cache_key": "short"})),
        None
    );
    assert_eq!(
        thread_key_from_body(&json!({"metadata": {"session_id": "abcdefgh"}})),
        Some("thread:abcdefgh".into())
    );
}

#[test]
fn user_agent_header_is_parsed_from_the_request_block() {
    assert_eq!(
        user_agent_from_headers(
            "POST /v1/chat/completions HTTP/1.1\r\nUser-Agent: pi/1.4.0\r\nHost: x\r\n\r\n"
        ),
        Some("pi/1.4.0".into())
    );
    assert_eq!(
        user_agent_from_headers("POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n\r\n"),
        None
    );
    assert_eq!(
        user_agent_from_headers("POST /v1/chat/completions HTTP/1.1\r\nUser-Agent:\r\n\r\n"),
        None
    );
}

#[test]
fn continuation_on_the_same_connection_regroups_when_request_was_missed() {
    let mut app = App::new();
    // Only the response side was captured.
    app.response(
        "client <-> server",
        tool_call_response("c1", "call_1", "search", "{}"),
    );
    // The follow-up runs on the same keep-alive connection.
    app.response(
        "client <-> server",
        delta_response("c2", json!({"content": "done"}), None),
    );

    assert_eq!(
        app.chats.len(),
        1,
        "same connection after tool calls = one session"
    );
    assert_eq!(app.chats[0].turns.len(), 2);
}

#[test]
fn unrelated_tool_call_ids_start_a_new_chat() {
    let mut app = App::new();
    app.response(
        "f",
        tool_call_response("c1", "call_1", "search", "{}"),
    );
    app.packet(Packet {
        header: "T 127.0.0.1:5002 -> 127.0.0.1:8080".into(),
        payload: json!({
            "model": "qwen",
            "messages": [
                {"role": "user", "content": "another task"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_2", "type": "function",
                    "function": {"name": "search", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_2", "content": "found"}
            ]
        })
        .to_string(),
    });
    app.response(
        &flow_key(5002),
        delta_response("c2", json!({"content": "done"}), None),
    );
    assert_eq!(
        app.chats.len(),
        2,
        "a different tool id is a different session"
    );
}

#[test]
fn manual_grouping_marks_a_parent_then_moves_the_selected_subtree() {
    let mut app = App::new();
    for id in ["a", "b", "c"] {
        app.response(
            id,
            delta_response(id, json!({"content": id}), None),
        );
    }
    // Mark a as the group parent, then attach c to it.
    app.selected = 0;
    app.group();
    assert_eq!(app.group_mark.as_deref(), Some("a"));
    app.selected = 2;
    app.group();
    assert_eq!(app.chats[1].id(), "c");
    assert_eq!(app.chats[1].parent.as_deref(), Some("a"));
    assert_eq!(app.selected, 1);
    assert!(app.group_mark.is_none());

    // c is now a child of a; grouping a under c must be refused (cycle).
    app.selected = 1;
    app.group(); // mark c
    app.selected = 0;
    app.group(); // try to attach a under c
    assert_eq!(app.chats[0].parent, None, "cycle must be rejected");

    // Marking b then attaching c moves c's subtree under b.
    app.selected = 2; // b
    app.group();
    app.selected = 1; // c
    app.group();
    assert_eq!(app.chats[1].id(), "b");
    assert_eq!(app.chats[2].id(), "c");
    assert_eq!(app.chats[2].parent.as_deref(), Some("b"));
}

#[test]
fn merge_prev_regroups_two_open_chats() {
    let mut app = App::new();
    app.response(
        "f",
        tool_call_response("c1", "call_1", "search", "{}"),
    );
    app.response(
        "g",
        delta_response("c2", json!({"content": "done"}), None),
    );
    app.chats[1].thread = Some("thread:abcdefgh".into());
    app.selected = 1;
    app.merge_prev();

    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].turns.len(), 2);
    assert_eq!(app.chats[0].turns[1].id, "c2");
    assert_eq!(app.selected, 0);
    assert_eq!(app.chats[0].thread.as_deref(), Some("thread:abcdefgh"));
    assert!(app.chats[0].has_id("c2"));
}

#[test]
fn continuation_search_skips_chats_without_a_captured_request() {
    let mut app = App::new();
    // A chat captured mid-stream has no request to compare against.
    app.response(
        "f",
        delta_response("c0", json!({"content": "hi"}), None),
    );
    // A second, fully captured chat must still be found after skipping it.
    app.packet(request_body(5000, first_request()));
    app.response(
        &flow_key(5000),
        tool_call_response("c1", "call_1", "search", "{}"),
    );
    app.packet(request_body(5001, continuation_request()));
    app.response(
        &flow_key(5001),
        delta_response("c2", json!({"content": "done"}), None),
    );

    assert_eq!(app.chats.len(), 2, "no extra tab");
    assert_eq!(
        app.chats[1].turns.len(),
        2,
        "the captured chat must continue"
    );
}

#[test]
fn a_request_without_tool_output_starts_a_new_chat() {
    let mut app = App::new();
    app.packet(request_body(5000, first_request()));
    app.response(
        "a",
        delta_response("c1", json!({"content": "hi"}), None),
    );
    // Same first message but nothing added: a different conversation, not a continuation.
    app.packet(Packet {
        header: "T 127.0.0.1:5002 -> 127.0.0.1:8080".into(),
        payload: json!({
            "model": "qwen",
            "messages": [
                {"role": "user", "content": "find the file"},
                {"role": "user", "content": "another question"}
            ]
        })
        .to_string(),
    });
    app.response(
        "b",
        delta_response("c2", json!({"content": "yo"}), None),
    );
    assert_eq!(app.chats.len(), 2);
}

#[test]
fn timeline_tracks_thinking_answer_and_tool_segments() {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"reasoning_content": "pondering"}), None),
    );
    assert_eq!(app.chats[0].last().segments.len(), 1);
    assert_eq!(app.chats[0].state_color(), Color::DarkGray);
    app.response(
        "f",
        delta_response("c1", json!({"content": "answer"}), None),
    );
    app.response(
        "f",
        tool_call_response("c1", "call_1", "search", "{\"q\":\"x\"}"),
    );

    let chat = &app.chats[0];
    let segments = &chat.last().segments;
    assert_eq!(
        segments.iter().map(|s| s.kind).collect::<Vec<_>>(),
        vec![SegKind::Thinking, SegKind::Answer, SegKind::Tool]
    );
    assert!(
        segments.iter().all(|s| s.ended.is_some()),
        "finish must close the active segment"
    );
    assert_eq!(
        chat.state_color(),
        TOOL_ORANGE,
        "last output was a tool call"
    );
    let timeline = timeline_body(chat, None).text.to_string();
    assert!(timeline.contains("thinking"), "{timeline}");
    assert!(timeline.contains("answer"), "{timeline}");
    assert!(timeline.contains("tool"), "{timeline}");
}

fn chat_with_three_segments() -> Chat {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"reasoning_content": "pondering"}), None),
    );
    app.response(
        "f",
        delta_response("c1", json!({"content": "hello world"}), None),
    );
    app.response(
        "f",
        tool_call_response("c1", "call_1", "search", "{\"q\":\"x\"}"),
    );
    app.chats.remove(0)
}

#[test]
fn segment_anchors_point_at_the_start_of_each_segment_in_the_body() {
    let chat = chat_with_three_segments();
    let body = render_body(&chat);
    // Wider than every line here, so a logical line index is its row.
    let anchors = segment_anchor_rows(&chat, &body, 120);
    assert_eq!(anchors.len(), 3);
    let line_row = |needle: &str| -> usize {
        body.lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.as_ref() == needle))
            .unwrap_or_else(|| panic!("line {needle:?} not in body: {body:?}"))
    };
    assert_eq!(anchors[0], line_row("pondering"));
    assert_eq!(anchors[1], line_row("hello world"));
    assert_eq!(anchors[2], line_row("[call_1] search"));
    assert!(anchors[0] < anchors[1] && anchors[1] < anchors[2]);
}

#[test]
fn segment_anchors_count_wrapped_rows() {
    let mut app = App::new();
    app.response(
        "f",
        delta_response("c1", json!({"reasoning_content": "aaaaaaaaaa bbbbbbbbbb cccccccccc"}), None),
    );
    let chat = &app.chats[0];
    let body = render_body(chat);
    let anchors = segment_anchor_rows(chat, &body, 10);
    // Body at width 10: REQUEST header (2 rows) + no-capture line (3) +
    // blank (1) + REASONING header (2) = 8 rows before the thinking text.
    assert_eq!(anchors[0], 8);
}

#[test]
fn timeline_underlines_the_active_entry_only() {
    let chat = chat_with_three_segments();
    let tl = timeline_body(&chat, Some(1));
    assert_eq!(tl.segments, vec![Some(0), Some(1), Some(2)]);
    let underlined =
        |l: &Line<'static>| l.spans[0].style.add_modifier.contains(Modifier::UNDERLINED);
    assert!(underlined(&tl.text.lines[1]));
    assert!(
        tl.text
            .lines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .all(|(_, l)| !underlined(l))
    );
    let none = timeline_body(&chat, None);
    assert!(none.text.lines.iter().all(|l| !underlined(l)));
}

#[test]
fn formats_durations_compactly() {
    assert_eq!(format_duration(Duration::from_millis(350)), "350ms");
    assert_eq!(format_duration(Duration::from_secs(12)), "12s");
    assert_eq!(format_duration(Duration::from_secs(125)), "2m05s");
}

#[test]
fn dump_and_body_share_one_layout() {
    let mut app = App::new();
    app.packet(request_body(5000, first_request()));
    app.packet(response_packet(
        5000,
        delta_response(
            "c1",
            json!({"content": "calling search"}),
            Some("tool_calls"),
        ),
    ));
    app.packet(request_body(5001, continuation_request()));
    app.packet(response_packet(
        5001,
        delta_response("c2", json!({"content": "done"}), Some("stop")),
    ));
    assert_eq!(app.chats.len(), 1);
    let chat = &app.chats[0];
    let dump = render_chat(chat);
    let body = render_body(chat);
    let dump_sections = &dump[dump.find("=== ").unwrap()..];
    let body_lines: Vec<String> = body
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();
    assert_eq!(
        dump_sections.lines().collect::<Vec<_>>(),
        body_lines,
        "the dump and the body panel must render the same layout"
    );
}
