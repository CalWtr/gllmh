use std::{process::Stdio, time::Duration};

use gllmh::{
    frame::{ErrorBody, Frame, codes},
    paths,
    profile::{
        BuiltinComposer, BuiltinMode, ComposerEngine, ComposerProfile, LlmProfile, LlmProtocol, NativeThinking,
        ReturnedThinking, Thinking, ThinkingReplay, WorkMode,
    },
    protocol::{
        broker::{Edges, SendInput, SendOutput},
        composer::{
            COMPOSE, ComposeInput, ComposeOutput, ContentBlock, INIT, InitInput, InitOutput, Message, Role, Stop,
        },
        records::{
            LlmCallError, LlmCallFailure, LlmCallInput, LlmCallOutput, ToolCallInput, ToolCallOutput, ToolResult,
        },
    },
    store::{ActionRequest, Store},
    types::{
        ActionId, ActionKind, Block, Interaction, InteractionId, InteractionKind, InteractionState, Sender,
        TurnOutcome, now,
    },
};
use serde::Serialize;
use serde_json::{Value, json, to_value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

struct Fixture {
    dir: TempDir,
    store: Store,
    composer: ComposerProfile,
    llm: LlmProfile,
    tools: Vec<String>,
}

impl Fixture {
    fn new(mode: BuiltinMode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        Self {
            dir,
            store,
            composer: ComposerProfile {
                name: "builtin".into(),
                engine: ComposerEngine::Builtin,
                custom_composer_bin: None,
                work_mode: WorkMode::React,
                max_steps_per_turn: 20,
                max_parallel_turns: if mode == BuiltinMode::Directive { 2 } else { 1 },
                compose_timeout_s: 60,
                builtin: Some(BuiltinComposer { mode, ..Default::default() }),
                custom: None,
            },
            llm: LlmProfile {
                name: "primary".into(),
                protocol: LlmProtocol::Anthropic,
                model_id: "fixture".into(),
                base_url: "http://unused.invalid".into(),
                api_key_file: None,
                thinking: Thinking {
                    native: NativeThinking::Optional,
                    on: false,
                    returned: ReturnedThinking::Full,
                    replay: ThinkingReplay::Required,
                },
                context_window: 4000,
                max_output: 500,
                tuning: Vec::new(),
                timeout_s: 300,
                max_retries: 3,
            },
            tools: vec!["lookup".into(), "send_message".into(), "send_directive".into()],
        }
    }

    fn init(&self) -> InitInput {
        let mut other = self.llm.clone();
        other.name = "secondary".into();
        other.context_window = 1;
        InitInput {
            user: "agent".into(),
            composer_profile: to_value(&self.composer).unwrap(),
            llm_profiles: vec![to_value(&self.llm).unwrap(), to_value(other).unwrap()],
            tools: self.tools.clone(),
            store: paths::store_db(self.dir.path()),
            edges: [
                (
                    Sender::user("agent"),
                    Edges { messages: vec!["user".into(), "peer".into()], directives: vec!["worker_agent".into()] },
                ),
                (
                    Sender::service("agent", "timer"),
                    Edges { messages: vec!["service_only".into()], directives: vec![] },
                ),
            ]
            .into(),
        }
    }

    fn receive(&mut self, interaction: Interaction, open_turn: bool) -> InteractionId {
        self.store.record_receive(&interaction, open_turn).unwrap();
        interaction.id
    }

    fn request_llm(&mut self, turn: InteractionId) -> ActionId {
        let input = LlmCallInput {
            llm_profile: self.llm.name.clone(),
            messages: vec![],
            tools: self.tools.clone(),
            thinking: true,
            request: Value::Null,
        };
        let mut request = ActionRequest::new(ActionKind::LlmCall, to_value(input).unwrap());
        request.turn = Some(turn);
        request.step = Some(self.store.increment_turn_step(turn, self.composer.max_steps_per_turn).unwrap());
        let id = self.store.request_action(request).unwrap();
        self.store.start_action(id).unwrap();
        id
    }

    fn llm(&mut self, turn: InteractionId, content: Vec<ContentBlock>) -> ActionId {
        let id = self.request_llm(turn);
        let reasoning: Vec<_> = content.iter().filter_map(|block| block.raw.as_ref()).collect();
        let reasoning_json = (!reasoning.is_empty()).then(|| serde_json::to_string(&reasoning).unwrap());
        let output =
            LlmCallOutput { content, stop_reason: "fixture".into(), usage: Default::default(), response: Value::Null };
        self.store.succeed_action(id, &to_value(output).unwrap(), reasoning_json.as_deref()).unwrap();
        id
    }

    fn request_tool(&mut self, turn: InteractionId, parent: ActionId, call_id: &str) -> ActionId {
        let input = ToolCallInput { tool: "lookup".into(), arguments: json!({}) };
        let mut request = ActionRequest::new(ActionKind::ToolCall, to_value(input).unwrap());
        request.turn = Some(turn);
        request.requested_by = Some(parent);
        request.call_id = Some(call_id.into());
        self.store.request_action(request).unwrap()
    }

    fn tool(&mut self, turn: InteractionId, parent: ActionId, call_id: &str, text: &str) {
        let id = self.request_tool(turn, parent, call_id);
        self.store.start_action(id).unwrap();
        let output = ToolCallOutput {
            tool_result: ToolResult { text: text.into(), is_error: false },
            exit_code: Some(0),
            stderr: None,
        };
        self.store.succeed_action(id, &to_value(output).unwrap(), None).unwrap();
    }

    fn send(&mut self, turn: InteractionId, tool: Option<(ActionId, &str)>) -> SendOutput {
        let input = SendInput {
            client_ref: ActionId::new(),
            kind: InteractionKind::Message,
            to: "user".into(),
            service: None,
            caused_by: Some(turn),
            body: Some(json!("sent from agent")),
            context: None,
            turn_outcome: None,
            local: false,
        };
        let mut request = ActionRequest::new(ActionKind::Send, to_value(input).unwrap());
        request.turn = Some(turn);
        if let Some((parent, call_id)) = tool {
            request.requested_by = Some(parent);
            request.call_id = Some(call_id.into());
        }
        let id = self.store.request_action(request).unwrap();
        self.store.start_action(id).unwrap();
        let output = SendOutput { id: InteractionId::new(), root: turn, hops: 1 };
        self.store.succeed_send(id, &output).unwrap();
        output
    }
}

fn interaction(kind: InteractionKind, from: Sender) -> Interaction {
    let id = InteractionId::new();
    Interaction {
        id,
        kind,
        client_ref: ActionId::new(),
        from,
        to: "agent".into(),
        caused_by: None,
        root: id,
        hops: 0,
        body: None,
        context: None,
        turn_outcome: None,
        state: InteractionState::Delivered,
        outcome: None,
        accepted_at: now(),
        closed_at: None,
    }
}

fn message(from: Sender, body: &str) -> Interaction {
    Interaction { body: Some(json!(body)), ..interaction(InteractionKind::Message, from) }
}

fn directive(instructions: &str) -> Interaction {
    Interaction {
        context: Some(vec![
            Block { name: "instructions".into(), content: instructions.into() },
            Block { name: "data".into(), content: "fixture data".into() },
        ]),
        ..interaction(InteractionKind::Directive, Sender::user("director"))
    }
}

fn result() -> Interaction {
    let directive = InteractionId::new();
    Interaction {
        caused_by: Some(directive),
        root: directive,
        hops: 1,
        body: Some(json!({"answer": 42})),
        turn_outcome: Some(TurnOutcome::Done),
        ..interaction(InteractionKind::Result, Sender::user("worker_agent"))
    }
}

fn text(role: Role, text: &str) -> Message {
    Message { role, content: vec![ContentBlock::text(text)] }
}

fn request(id: &str, op: &str, input: impl Serialize) -> String {
    let frame = Frame {
        id: id.into(),
        op: Some(op.into()),
        input: Some(to_value(input).unwrap()),
        re: None,
        output: None,
        error: None,
    };
    format!("{}\n", serde_json::to_string(&frame).unwrap())
}

// Exercise the hidden binary with real pipes. Submit the compose batch before
// reading any of its replies, and half-close stdin while replies are pending.
fn compose(init: InitInput, turns: &[InteractionId]) -> Vec<Result<ComposeOutput, ErrorBody>> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        timeout(Duration::from_secs(15), async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_gllmh"))
                .arg("composer")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut replies = BufReader::new(child.stdout.take().unwrap()).lines();

            input.write_all(request("init", INIT, init).as_bytes()).await.unwrap();
            let reply: Frame = serde_json::from_str(&replies.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(reply.re.as_deref(), Some("init"));
            assert_eq!(reply.error, None);
            assert_eq!(serde_json::from_value::<InitOutput>(reply.output.unwrap()).unwrap(), InitOutput {});

            let batch: String = turns
                .iter()
                .enumerate()
                .map(|(idx, &turn)| request(&idx.to_string(), COMPOSE, ComposeInput { turn }))
                .collect();
            input.write_all(batch.as_bytes()).await.unwrap();
            drop(input);

            let mut outputs = vec![None; turns.len()];
            while let Some(line) = replies.next_line().await.unwrap() {
                let reply: Frame = serde_json::from_str(&line).unwrap();
                let idx: usize = reply.re.unwrap().parse().unwrap();
                let result = match reply.error {
                    Some(error) => Err(error),
                    None => Ok(serde_json::from_value(reply.output.unwrap()).unwrap()),
                };
                assert!(outputs[idx].replace(result).is_none(), "duplicate reply {idx}");
            }
            assert!(child.wait().await.unwrap().success());
            outputs.into_iter().map(|reply| reply.expect("compose reply missing")).collect()
        })
        .await
        .expect("composer did not finish")
    })
}

fn one(fixture: &Fixture, turn: InteractionId) -> ComposeOutput {
    compose(fixture.init(), &[turn]).pop().unwrap().unwrap()
}

#[test]
fn message_context_golden() {
    let mut fixture = Fixture::new(BuiltinMode::Message);
    let first = fixture.dir.path().join("first.txt");
    let last = fixture.dir.path().join("last.txt");
    std::fs::write(&first, "First tuning file").unwrap();
    std::fs::write(&last, "Last tuning file").unwrap();
    fixture.llm.tuning = vec![first, fixture.dir.path().join("missing.txt"), last];

    fixture.receive(message(Sender::service("agent", "timer"), "tick"), false);
    fixture.receive(directive("Do work"), false);
    let turn = fixture.receive(result(), true);
    fixture.store.start_turn(turn).unwrap();
    let assistant = vec![
        ContentBlock::thinking(json!({"type":"thinking","thinking":"still thinking","signature":"abc\n=="})),
        ContentBlock::text("I'll look it up."),
        ContentBlock::tool_use("lookup", "lookup", json!({})),
        ContentBlock::tool_use("missing", "lookup", json!({})),
        ContentBlock::tool_use("denied", "lookup", json!({})),
        ContentBlock::tool_use("interrupted", "lookup", json!({})),
        ContentBlock::tool_use("send", "send_message", json!({"to":"user","body":"sent from agent"})),
    ];
    let llm = fixture.llm(turn, assistant.clone());
    let sent = fixture.send(turn, Some((llm, "send")));
    let interrupted = fixture.request_tool(turn, llm, "interrupted");
    fixture.store.interrupt_action(interrupted).unwrap();
    let denied = fixture.request_tool(turn, llm, "denied");
    fixture.store.deny_action(denied).unwrap();
    fixture.tool(turn, llm, "lookup", "found");
    fixture.send(turn, None);
    fixture.store.set_mark(turn, "ignored", &json!(true), turn).unwrap();

    let expected = ComposeOutput {
        llm_profile: "primary".into(),
        tools: fixture.tools.clone(),
        thinking: Some(true),
        marks: vec![],
        stop: None,
        messages: vec![
            text(
                Role::System,
                "First tuning file\n\nLast tuning file\n\nYou are agent.\nYou may message user, peer.\nYou may direct worker_agent.",
            ),
            text(Role::User, "[message from agent::timer]\ntick"),
            text(Role::User, "[directive from director]\n## instructions\nDo work\n\n## data\nfixture data"),
            text(Role::User, "[result from worker_agent: done]\n{\"answer\":42}"),
            Message { role: Role::Assistant, content: assistant },
            Message {
                role: Role::ToolResult,
                content: vec![
                    ContentBlock::tool_result("lookup", "found", false),
                    ContentBlock::tool_result("missing", "not run", true),
                    ContentBlock::tool_result("denied", "denied", true),
                    ContentBlock::tool_result("interrupted", "interrupted", true),
                    ContentBlock::tool_result("send", format!("sent message {} to user", sent.id), false),
                ],
            },
        ],
    };
    assert_eq!(one(&fixture, turn), expected);

    fixture.llm.thinking.replay = ThinkingReplay::Optional;
    assert_eq!(one(&fixture, turn), expected);

    fixture.llm.thinking.replay = ThinkingReplay::Never;
    fixture.tools.retain(|tool| tool != "send_directive");
    let mut expected = expected;
    expected.tools = fixture.tools.clone();
    expected.messages[0] =
        text(Role::System, "First tuning file\n\nLast tuning file\n\nYou are agent.\nYou may message user, peer.");
    expected.messages[4].content.remove(0);
    assert_eq!(one(&fixture, turn), expected);
}

#[test]
fn directive_contexts_golden_over_concurrent_pipes() {
    let mut fixture = Fixture::new(BuiltinMode::Directive);
    fixture.tools.clear();
    fixture.receive(message(Sender::user("user"), "unrelated"), false);
    let first = fixture.receive(directive("First task"), true);
    let second = fixture.receive(directive("Second task"), true);
    fixture.store.start_turn(first).unwrap();
    fixture.store.start_turn(second).unwrap();
    fixture.llm(first, vec![ContentBlock::text("First progress")]);
    fixture.llm(second, vec![ContentBlock::text("Second progress")]);

    let outputs = compose(fixture.init(), &[second, first, InteractionId::new()]);
    for (output, task) in outputs[..2].iter().zip(["Second", "First"]) {
        assert_eq!(
            output.as_ref().unwrap().messages,
            vec![
                text(Role::System, "You are agent."),
                text(Role::User, &format!("## instructions\n{task} task\n\n## data\nfixture data")),
                text(Role::Assistant, &format!("{task} progress")),
            ]
        );
    }
    assert_eq!(outputs[2].as_ref().unwrap_err().code, codes::BAD_INPUT);

    fixture.composer.builtin.as_mut().unwrap().budget_fraction = 0.0;
    assert_eq!(one(&fixture, first).messages, outputs[1].as_ref().unwrap().messages[..2]);

    let turn = fixture.receive(result(), true);
    assert_eq!(
        one(&fixture, turn).messages,
        vec![
            text(Role::System, "You are agent."),
            text(Role::User, "[result from worker_agent: done]\n{\"answer\":42}"),
        ]
    );
}

#[test]
fn trimming_keeps_trigger_and_evicts_model_calls_with_their_tool_rows() {
    let mut fixture = Fixture::new(BuiltinMode::Message);
    fixture.tools = vec!["lookup".into()];
    fixture.llm.context_window = 300;
    let old = fixture.receive(message(Sender::user("user"), "old"), true);
    fixture.store.start_turn(old).unwrap();
    let request = ContentBlock::tool_use("reused", "lookup", json!({}));
    let llm = fixture.llm(old, vec![ContentBlock::text("old assistant"), request.clone()]);
    let turn = fixture.receive(message(Sender::user("peer"), "keep me"), true);
    let old_output = "large output ".repeat(100);
    fixture.tool(old, llm, "reused", &old_output);
    fixture.store.end_turn(old, TurnOutcome::Done).unwrap();
    fixture.store.start_turn(turn).unwrap();
    let llm = fixture.llm(turn, vec![request.clone()]);
    fixture.tool(turn, llm, "reused", "new output");
    fixture.receive(message(Sender::user("user"), "newer than trigger"), false);

    let pinned = vec![text(Role::System, "You are agent."), text(Role::User, "[message from peer]\nkeep me")];
    let mut expected = pinned.clone();
    expected.extend([
        Message { role: Role::Assistant, content: vec![request.clone()] },
        Message { role: Role::ToolResult, content: vec![ContentBlock::tool_result("reused", "new output", false)] },
        text(Role::User, "[message from user]\nnewer than trigger"),
    ]);
    assert_eq!(one(&fixture, turn).messages, expected);

    fixture.llm.context_window = 4000;
    let mut full = expected;
    full.splice(
        1..1,
        [
            text(Role::User, "[message from user]\nold"),
            Message { role: Role::Assistant, content: vec![ContentBlock::text("old assistant"), request] },
            Message { role: Role::ToolResult, content: vec![ContentBlock::tool_result("reused", old_output, false)] },
        ],
    );
    assert_eq!(one(&fixture, turn).messages, full);

    fixture.llm.context_window = 1;
    assert_eq!(one(&fixture, turn).messages, pinned);
}

#[test]
fn work_modes_choose_tools_and_thinking() {
    let mut fixture = Fixture::new(BuiltinMode::Message);
    let turn = fixture.receive(message(Sender::user("user"), "prompt"), true);
    for (mode, tools, thinking) in [
        (WorkMode::Inst, false, false),
        (WorkMode::Reason, false, true),
        (WorkMode::Act, true, false),
        (WorkMode::React, true, true),
    ] {
        fixture.composer.work_mode = mode;
        let output = one(&fixture, turn);
        assert_eq!(output.thinking, Some(thinking), "{mode:?}");
        assert_eq!(output.tools.as_slice(), if tools { fixture.tools.as_slice() } else { &[] }, "{mode:?}");
    }
}

#[test]
fn only_the_current_turns_latest_model_call_can_stop_composition() {
    let mut fixture = Fixture::new(BuiltinMode::Message);
    let failed = fixture.receive(message(Sender::user("user"), "failed turn"), true);
    fixture.store.start_turn(failed).unwrap();
    fixture.llm(failed, vec![ContentBlock::text("before failure")]);
    let id = fixture.request_llm(failed);
    let failure = LlmCallFailure {
        error: LlmCallError {
            kind: "timeout".into(),
            status: None,
            message: "timed out".into(),
            retryable: true,
            attempts: 4,
        },
    };
    fixture.store.fail_action(id, &to_value(failure).unwrap()).unwrap();
    fixture.store.end_turn(failed, TurnOutcome::Failed).unwrap();
    let next = fixture.receive(message(Sender::user("user"), "next turn"), true);
    fixture.store.start_turn(next).unwrap();
    fixture.llm(next, vec![ContentBlock::text("latest unrelated model call")]);

    let outputs = compose(fixture.init(), &[failed, next]);
    let failure = outputs[0].as_ref().unwrap();
    assert_eq!(failure.stop, Some(Stop::Fail));
    assert!(failure.messages.is_empty());
    let next = outputs[1].as_ref().unwrap();
    assert_eq!(next.stop, None);
    assert_eq!(
        next.messages[1..],
        [
            text(Role::User, "[message from user]\nfailed turn"),
            text(Role::Assistant, "before failure"),
            text(Role::User, "[message from user]\nnext turn"),
            text(Role::Assistant, "latest unrelated model call"),
        ]
    );
}
