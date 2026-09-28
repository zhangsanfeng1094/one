//! End-to-end host compilation without network requests.
use super::*;
use clap::Parser;
use one_prompt::{ModelRule, PromptSpec, SlotOperation};
use std::path::Path;

fn cli(root: &Path) -> crate::cli::Cli {
    let mut cli = crate::cli::Cli::try_parse_from([
        "one",
        "--provider",
        "mock",
        "--no-session",
        "--no-mcp",
        "--no-skills",
        "--no-memory",
        "--no-subagent",
    ])
    .unwrap();
    cli.cwd = root.into();
    cli
}
fn rule(model: &str, text: &str) -> ModelRule {
    ModelRule {
        provider: "mock".into(),
        model: model.into(),
        operations: vec![SlotOperation::append("extra", text)],
    }
}
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("one-host-prompt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn enhancer_file_reaches_live_main_agent_and_reloads_without_stale_model_text() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".one/prompts")).unwrap();
    let path = dir.join(".one/prompts/enhancers.json");
    let write = |enabled: bool, text: &str| {
        std::fs::write(
            &path,
            serde_json::json!({
                "version":1,"bindings":[{"provider":"mock","model":"mock-v1","enhancers":{
                    "stops_early":{"enabled":enabled,"prompt":text},
                    "over_planning":{"enabled":true,"prompt":"MAIN_OVER_PLANNING"}
                }}]
            })
            .to_string(),
        )
        .unwrap()
    };
    write(true, "MAIN_ENHANCER_ONE");
    let mut runtime = AppRuntime::build(&cli(&dir)).await.unwrap();
    assert!(runtime
        .agent
        .lock()
        .await
        .config
        .system_prompt
        .contains("MAIN_ENHANCER_ONE"));
    write(true, "MAIN_ENHANCER_TWO");
    runtime.reload_extensions().await.unwrap();
    let updated = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(updated.contains("MAIN_ENHANCER_TWO") && !updated.contains("MAIN_ENHANCER_ONE"));
    write(false, "MAIN_ENHANCER_TWO");
    runtime.reload_extensions().await.unwrap();
    let disabled = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(!disabled.contains("MAIN_ENHANCER_TWO") && disabled.contains("MAIN_OVER_PLANNING"));
    runtime.prompt_model = "another-model".into();
    runtime.rebuild_act_tools().await.unwrap();
    assert!(!runtime
        .agent
        .lock()
        .await
        .config
        .system_prompt
        .contains("MAIN_OVER_PLANNING"));
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn enhancer_file_reaches_task_harness_provider_request() {
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
    use one_core::message::{ContentBlock, StopReason};
    use one_core::tool::{Tool, ToolCall};
    use std::sync::{Arc, Mutex};
    struct Capture {
        model: &'static str,
        seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Capture {
        fn name(&self) -> &str {
            "enhancer-test"
        }
        fn model(&self) -> &str {
            self.model
        }
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> one_core::Result<CompletionResponse> {
            self.seen.lock().unwrap().push(request.system_prompt);
            Ok(CompletionResponse {
                provider: self.name().into(),
                model: self.model().into(),
                content: vec![ContentBlock::Text {
                    text: "The delegated inspection is verified.".into(),
                }],
                stop_reason: StopReason::Stop,
                usage: TokenUsage::default(),
                citations: vec![],
            })
        }
    }
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".one/prompts")).unwrap();
    std::fs::write(
        dir.join(".one/prompts/enhancers.json"),
        serde_json::json!({
            "version":1,"bindings":[{"provider":"enhancer-test","model":"bound-child","enhancers":{
                "stops_early":{"enabled":true,"prompt":"TASK_CHILD_ENHANCER"},
                "weak_verification":{"enabled":true,"prompt":"TASK_VERIFY_ENHANCER"}
            }}]
        })
        .to_string(),
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = task_tool::TaskToolHost::new(
        harness::HarnessOptions::from_cwd(&dir),
        crate::protocol::AgentSpec::builtin_main(),
        jobs::AgentJobRegistry::new(Arc::new(Mutex::new(Vec::new()))),
        one_tools::PathPolicy::workspace(dir.clone()),
    );
    let tool = task_tool::TaskTool::new(host.clone());
    for model in ["bound-child", "other-child"] {
        host.bind_provider(Arc::new(Capture {
            model,
            seen: seen.clone(),
        }))
        .await;
        let output = tool.execute(&ToolCall {
            id: format!("enhancer-{model}"), name: "task".into(),
            arguments: serde_json::json!({"prompt":"Inspect the task without calling tools", "agent":"explore", "background":false}),
        }).await.unwrap();
        assert!(
            !output.as_text().contains("status=runtime_error"),
            "{}",
            output.as_text()
        );
        // Foreground can yield to a job; wait for the actual provider request.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if seen.lock().unwrap().len() >= if model == "bound-child" { 1 } else { 2 } {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let requests = seen.lock().unwrap();
    assert!(
        requests[0].contains("TASK_CHILD_ENHANCER") && requests[0].contains("TASK_VERIFY_ENHANCER")
    );
    assert!(
        !requests[1].contains("TASK_CHILD_ENHANCER")
            && !requests[1].contains("TASK_VERIFY_ENHANCER")
    );
    drop(requests);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn runtime_model_mode_capabilities_reload_and_failure_rollback() {
    let dir = scratch();
    let args = cli(&dir);
    let mut runtime = AppRuntime::build(&args).await.unwrap();
    runtime.plan_path = Some(dir.join("plan.md"));
    let mut spec = PromptSpec::default();
    spec.rules = vec![rule("mock-v1", "MODEL_A"), rule("B", "MODEL_B")];
    runtime.main_agent.prompt.spec = Some(spec);
    runtime.rebuild_act_tools().await.unwrap();
    let a = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(a.contains("MODEL_A") && !a.contains("MODEL_B"));
    runtime.prompt_model = "B".into();
    runtime.rebuild_act_tools().await.unwrap();
    let b = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(b.contains("MODEL_B") && !b.contains("MODEL_A"));
    runtime.prompt_model = "mock-v1".into();
    runtime.rebuild_act_tools().await.unwrap();
    assert_eq!(a, runtime.agent.lock().await.config.system_prompt);
    assert!(!a.contains("`task` tool") && !a.contains("## Memory write"));

    runtime.enter_plan_mode().await.unwrap();
    let plan = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(
        plan.contains("Plan mode is active")
            && plan.contains(dir.join("plan.md").to_str().unwrap())
    );
    assert!(!runtime
        .agent
        .lock()
        .await
        .tool_definitions()
        .iter()
        .any(|t| t.name == "bash"));
    runtime.leave_plan_mode().await.unwrap();
    assert_eq!(a, runtime.agent.lock().await.config.system_prompt);

    // Reload reads original DSL and relative Markdown every time.
    let file = dir.join("prompt.toml");
    std::fs::write(
        &file,
        "version = 1\n[[operations]]\nslot = 'extra'\nop = 'replace'\nbody = {file = 'extra.md'}",
    )
    .unwrap();
    std::fs::write(dir.join("extra.md"), "RELOAD_1").unwrap();
    runtime.main_agent.prompt = crate::prompt_config::PromptConfig {
        file: Some(file.clone()),
        ..Default::default()
    };
    runtime.reload_extensions().await.unwrap();
    assert!(runtime
        .agent
        .lock()
        .await
        .config
        .system_prompt
        .contains("RELOAD_1"));
    std::fs::write(dir.join("extra.md"), "RELOAD_2").unwrap();
    runtime.reload_extensions().await.unwrap();
    let good = runtime.agent.lock().await.config.system_prompt.clone();
    assert!(good.contains("RELOAD_2") && !good.contains("RELOAD_1"));
    let good_trace = runtime.compiled_prompt.clone();
    std::fs::write(dir.join("extra.md"), "{{missing_required_value}}").unwrap();
    assert!(runtime.reload_extensions().await.is_err());
    assert_eq!(good, runtime.agent.lock().await.config.system_prompt);
    assert_eq!(good_trace, runtime.compiled_prompt);
    assert!(runtime.enter_plan_mode().await.is_err());
    assert_eq!(runtime.mode(), AgentMode::Act);
    assert_eq!(good, runtime.agent.lock().await.config.system_prompt);

    let providers = crate::provider::ProviderSet::build(&args).unwrap();
    runtime.prompt_model = "previous-model".into();
    assert!(runtime
        .refresh_web_search_backend(&providers)
        .await
        .is_err());
    assert_eq!(runtime.prompt_model, "previous-model");
    assert_eq!(good, runtime.agent.lock().await.config.system_prompt);
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn invalid_prompt_fails_initialization_instead_of_using_bootstrap() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".one/agents")).unwrap();
    std::fs::write(
        dir.join(".one/agents/main.json"),
        r#"{"prompt":{"file":"missing.toml"}}"#,
    )
    .unwrap();
    let error = AppRuntime::build(&cli(&dir))
        .await
        .err()
        .expect("initialization must fail");
    assert!(error.to_string().contains("missing.toml"));
    std::fs::write(
        dir.join(".one/agents/main.json"),
        r#"{"system_prompt":"legacy"}"#,
    )
    .unwrap();
    let error = AppRuntime::build(&cli(&dir))
        .await
        .err()
        .expect("migration error");
    assert!(error.to_string().contains("migrate"));
    std::fs::rename(
        dir.join(".one/agents/main.json"),
        dir.join(".one/agents/default.json"),
    )
    .unwrap();
    let error = AppRuntime::build(&cli(&dir))
        .await
        .err()
        .expect("default.json migration error");
    assert!(error.to_string().contains("migrate"));
    std::fs::remove_file(dir.join(".one/agents/default.json")).unwrap();
    std::fs::write(
        dir.join(".one/agents/worker.json"),
        r#"{"append_system_prompt":"legacy"}"#,
    )
    .unwrap();
    let error = AppRuntime::build(&cli(&dir))
        .await
        .err()
        .expect("discovered child migration error");
    assert!(error.to_string().contains("worker.json") && error.to_string().contains("migrate"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn agent_file_paths_and_nested_specs_use_declaring_directory() {
    let dir = scratch();
    let path = dir.join("main.json");
    std::fs::write(dir.join("p.toml"), "version = 1\npreset = 'general'\n[[operations]]\nslot = 'role'\nop = 'replace'\nbody = {file = 'r.md'}").unwrap();
    std::fs::write(dir.join("r.md"), "RELATIVE_BODY").unwrap();
    std::fs::write(
        &path,
        r#"{"prompt":{"file":"p.toml"},"agents":{"child":{"prompt":{"file":"p.toml"}}}}"#,
    )
    .unwrap();
    let spec = presets::load_spec_file(&path).unwrap();
    for prompt in [&spec.prompt, &spec.agents["child"].prompt] {
        let loaded = prompt.load(Path::new("/unrelated/worktree")).unwrap();
        let p = one_prompt::compile(
            &loaded,
            &one_prompt::ComponentRegistry::with_builtins(),
            &one_prompt::CompileContext::default(),
        )
        .unwrap();
        assert_eq!(p.text, "RELATIVE_BODY");
    }
    let mut config = spec.prompt.clone();
    config.operations.push(SlotOperation::append("typo", "x"));
    let loaded = config.load(Path::new("/unrelated")).unwrap();
    let error = one_prompt::compile(
        &loaded,
        &one_prompt::ComponentRegistry::with_builtins(),
        &one_prompt::CompileContext::default(),
    )
    .unwrap_err();
    assert!(
        error.source_location.contains("main.json")
            && error.source_location.contains("operations[0]")
    );
    for key in ["system_prompt", "append_system_prompt"] {
        for value in [serde_json::Value::Null, serde_json::json!("old")] {
            let error = serde_json::from_value::<crate::protocol::AgentSpec>(
                serde_json::json!({key: value}),
            )
            .unwrap_err();
            assert!(error.to_string().contains("migrate"));
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn harness_uses_explicit_child_model_and_its_actual_capabilities() {
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
    struct Parent;
    #[async_trait::async_trait]
    impl LlmProvider for Parent {
        fn name(&self) -> &str {
            "parent"
        }
        fn model(&self) -> &str {
            "parent-model"
        }
        async fn complete(&self, _: CompletionRequest) -> one_core::Result<CompletionResponse> {
            panic!("child must not call parent provider")
        }
    }
    let dir = scratch();
    let mut child = crate::protocol::AgentSpec::builtin_explore();
    child.model.provider = Some("mock".into());
    child.model.id = Some("mock-v1".into());
    child.model.inherit = false;
    // An unavailable capability can contain a missing variable without rendering.
    let mut spec = PromptSpec::default();
    spec.rules = vec![
        ModelRule {
            provider: "parent".into(),
            model: "*".into(),
            operations: vec![SlotOperation::replace("role", "{{wrong_parent_rule}}")],
        },
        ModelRule {
            provider: "mock".into(),
            model: "mock-v1".into(),
            operations: vec![SlotOperation::replace("subagent", "{{unavailable_task}}")],
        },
    ];
    child.prompt = crate::prompt_config::PromptConfig {
        spec: Some(spec),
        ..Default::default()
    };
    let mut req = crate::protocol::RunRequest::new(child, "hello");
    let result = harness::run(
        req.clone(),
        &Parent,
        &harness::HarnessOptions::from_cwd(&dir),
    )
    .await;
    assert!(result.ok, "{result:?}");
    let echo = result.agent.unwrap();
    assert_eq!(echo.model.unwrap().id.as_deref(), Some("mock-v1"));
    req.agent
        .prompt
        .spec
        .as_mut()
        .unwrap()
        .rules
        .push(rule("mock-v1", "{{selected_child_model}}"));
    let failed = harness::run(req, &Parent, &harness::HarnessOptions::from_cwd(&dir)).await;
    assert!(!failed.ok);
    assert!(failed
        .error
        .unwrap()
        .message
        .contains("selected_child_model"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn compaction_preserves_prefix_and_uses_cache_sharing_fork_request() {
    use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider, TokenUsage};
    use one_core::compaction::{messages_fingerprint, CompactRequest};
    use one_core::message::{
        AgentMessage, ContentBlock, StopReason, TextOrImage, ToolResultMessage,
    };
    use std::sync::{Arc, Mutex};

    struct CaptureCompactProvider {
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CaptureCompactProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-v1"
        }
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> one_core::Result<CompletionResponse> {
            self.seen.lock().unwrap().push(request);
            Ok(CompletionResponse {
                provider: self.name().into(),
                model: self.model().into(),
                content: vec![ContentBlock::text("cached summary bullet")],
                stop_reason: StopReason::Stop,
                usage: TokenUsage {
                    cache_read_tokens: 4_096,
                    ..TokenUsage::default()
                },
                citations: vec![],
            })
        }
    }

    let dir = scratch();
    let mut runtime = AppRuntime::build(&cli(&dir)).await.unwrap();
    let big_tool = "x".repeat(8_000);
    let initial_messages = vec![
        AgentMessage::user_text("turn 1"),
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: vec![TextOrImage::Text {
                text: big_tool.clone(),
            }],
            is_error: false,
            timestamp: 0,
        }),
        AgentMessage::assistant_text("mock", "mock-v1", "done 1"),
        AgentMessage::user_text("turn 2"),
        AgentMessage::assistant_text("mock", "mock-v1", "done 2"),
        AgentMessage::user_text("turn 3"),
        AgentMessage::assistant_text("mock", "mock-v1", "done 3"),
        AgentMessage::user_text("turn 4"),
        AgentMessage::assistant_text("mock", "mock-v1", "done 4"),
    ];
    let expected_older_fp = messages_fingerprint(&initial_messages[..5]);
    let expected_full_fp = messages_fingerprint(&initial_messages);
    let expected_sys = {
        let mut agent = runtime.agent.lock().await;
        agent.messages = initial_messages;
        agent.config.system_prompt.clone()
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = CaptureCompactProvider { seen: seen.clone() };

    // 1. Auto check below threshold must NOT mutate old tool results in place.
    let auto_out = runtime
        .maybe_compact(&provider, CompactRequest::auto())
        .await
        .unwrap();
    assert!(auto_out.is_none());
    assert_eq!(
        messages_fingerprint(&runtime.agent.lock().await.messages),
        expected_full_fp,
        "normal turn check must keep history byte-identical for prompt cache"
    );

    // 2. Manual/threshold compaction must reuse system_prompt + tools + older prefix.
    let applied = runtime
        .maybe_compact(
            &provider,
            CompactRequest::manual(Some("focus on read".into())),
        )
        .await
        .unwrap()
        .expect("manual compact should apply");
    assert_eq!(applied.kept_turns, 2);

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system_prompt, expected_sys);
    assert!(!requests[0].tools.is_empty());
    assert_eq!(
        messages_fingerprint(&requests[0].messages[..5]),
        expected_older_fp,
        "compaction request must share the exact older conversation prefix"
    );

    drop(requests);
    drop(runtime);
    std::fs::remove_dir_all(dir).unwrap();
}
