//! Disk → Config API → real host compiler contract, isolated from user config.
use one_cli::config_studio::{adapters::StudioPaths, ConfigStudio};
use one_web::HttpRequest;
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    studio: ConfigStudio,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("one-enhancer-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let studio = ConfigStudio::with_paths(StudioPaths {
            cwd: root.clone(),
            agent_dir: root.join("agent"),
            home: root.clone(),
            project_chain: vec![root.clone()],
        });
        Self { root, studio }
    }
    fn get(&self) -> Value {
        self.call(
            "GET",
            "scope=project&provider=gemini&model=gemini-2.5-pro&preset=code",
            Value::Null,
            200,
        )
    }
    fn call(&self, method: &str, query: &str, value: Value, expected: u16) -> Value {
        let result = self.studio.dispatch(&HttpRequest {
            method: method.into(),
            path: "/api/config/prompts/enhancers".into(),
            raw_query: query.into(),
            headers: Default::default(),
            body: serde_json::to_vec(&value).unwrap(),
        });
        assert_eq!(
            result.status,
            expected,
            "{}",
            String::from_utf8_lossy(&result.body)
        );
        serde_json::from_slice(&result.body).unwrap()
    }
    fn save(&self, id: &str, enabled: bool, prompt: &str) {
        self.call("POST", "", json!({"scope":"project", "provider":"gemini", "model":"gemini-2.5-pro", "id":id, "enabled":enabled, "prompt":prompt, "version":self.get()["version"]}), 200);
    }
    fn preview(&self, model: &str) -> Value {
        self.studio
            .preview_effective_prompt("code", "gemini", model)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

#[test]
fn global_studio_excludes_project_sources_without_changing_legacy_runtime() {
    let f = Fixture::new();
    f.save("stops_early", true, "PROJECT_ONLY_BEHAVIOR");
    let global = f.call(
        "GET",
        "scope=global&provider=gemini&model=gemini-2.5-pro&preset=code&studio_global=1",
        Value::Null,
        200,
    );
    assert!(!global["enhancers"]
        .to_string()
        .contains("PROJECT_ONLY_BEHAVIOR"));
    let version = global["version"].clone();
    f.call("POST", "studio_global=1", json!({"scope":"global", "provider":"gemini", "model":"gemini-2.5-pro", "id":"stops_early", "enabled":true, "prompt":"GLOBAL_ONLY_BEHAVIOR", "version":version}), 200);
    let request = |path: &str, query: &str| {
        let response = f.studio.dispatch(&HttpRequest {
            method: "GET".into(),
            path: path.into(),
            raw_query: query.into(),
            headers: Default::default(),
            body: vec![],
        });
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        serde_json::from_slice::<Value>(&response.body).unwrap()
    };
    let preview = request(
        "/api/config/prompts/preview",
        "studio_global=1&preset=code&provider=gemini&model=gemini-2.5-pro",
    );
    let text = preview["compiled_prompt"].as_str().unwrap();
    assert!(text.contains("GLOBAL_ONLY_BEHAVIOR"));
    assert!(!text.contains("PROJECT_ONLY_BEHAVIOR"));
    assert!(f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("PROJECT_ONLY_BEHAVIOR"));
    let catalog = request("/api/config/catalog", "studio_global=1");
    assert!(catalog["documents"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["scope"] != "project"
            || d["id"].as_str().unwrap_or("").starts_with("prompts.ref.")));
    let hidden = f.studio.dispatch(&HttpRequest {
        method: "GET".into(),
        path: "/api/config/documents/enhancers.project".into(),
        raw_query: "studio_global=1".into(),
        headers: Default::default(),
        body: vec![],
    });
    assert_eq!(hidden.status, 404);
}

#[test]
fn global_effective_mcp_ignores_project_servers() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.root.join("agent")).unwrap();
    std::fs::create_dir_all(f.root.join(".one")).unwrap();
    std::fs::write(
        f.root.join("agent/mcp.json"),
        r#"{"mcpServers":{"global-server":{"command":"global-command"}}}"#,
    )
    .unwrap();
    std::fs::write(
        f.root.join(".one/mcp.json"),
        r#"{"mcpServers":{"project-server":{"command":"project-command"}}}"#,
    )
    .unwrap();
    let response = f.studio.dispatch(&HttpRequest {
        method: "GET".into(),
        path: "/api/config/effective".into(),
        raw_query: "module=mcp&studio_global=1".into(),
        headers: Default::default(),
        body: vec![],
    });
    assert_eq!(response.status, 200);
    let text = String::from_utf8(response.body).unwrap();
    assert!(text.contains("global-server"));
    assert!(!text.contains("project-server"));
}

#[test]
fn agent_document_preview_uses_its_prompt_and_profile_binding() {
    let f = Fixture::new();
    let path = f.root.join(".one/agents/research.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, include_str!("../../../.one/agents/research.json")).unwrap();
    let doc = f
        .studio
        .documents()
        .into_iter()
        .find(|d| d.doc.id.starts_with("prompts.ref."))
        .unwrap();
    f.call("POST", "", json!({"scope":"project", "provider":"gemini", "model":"gemini-2.5-pro", "preset":"general", "id":"stops_early", "enabled":true, "prompt":"GENERAL_ONLY_OVERRIDE", "version":f.get()["version"]}), 200);
    let request = |document: &str| {
        f.studio.dispatch(&HttpRequest {
            method: "GET".into(),
            path: "/api/config/prompts/preview".into(),
            raw_query: format!(
                "preset=code&provider=gemini&model=gemini-2.5-pro&document={document}"
            ),
            headers: Default::default(),
            body: vec![],
        })
    };
    let response = request(&doc.doc.id);
    assert_eq!(response.status, 200);
    let preview: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(preview["preset"], "general");
    let text = preview["compiled_prompt"].as_str().unwrap();
    assert!(text.contains("You are a read-only research sub-agent of One."));
    assert!(text.contains("GENERAL_ONLY_OVERRIDE"));
    assert!(preview["slots"]
        .to_string()
        .contains("ModelEnhancer::stops_early"));
    assert!(!f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GENERAL_ONLY_OVERRIDE"));
    assert_eq!(request("prompts.builtin.preset.code").status, 400);
    assert_eq!(request("../../outside.json").status, 400);
}

#[test]
fn edit_restart_preview_disable_switch_manual_edit_restore() {
    let f = Fixture::new();
    let catalog = f.get();
    assert!(catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "gemini-2.5-pro"));
    assert_eq!(catalog["enhancers"].as_array().unwrap().len(), 4);
    f.save("stops_early", true, "GEMINI_STOP_OVERRIDE_SENTINEL");
    f.save("over_planning", true, "GEMINI_PLAN_SENTINEL");
    let restarted = ConfigStudio::with_paths(f.studio.paths().clone());
    let preview = restarted.preview_effective_prompt("code", "gemini", "gemini-2.5-pro");
    let text = preview["compiled_prompt"].as_str().unwrap();
    assert!(
        text.contains("GEMINI_STOP_OVERRIDE_SENTINEL") && text.contains("GEMINI_PLAN_SENTINEL")
    );
    let trace = preview["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["slot"] == "behavior_hooks")
        .unwrap();
    assert!(trace
        .to_string()
        .contains("ModelEnhancer::stops_early [user override:"));
    assert!(trace.to_string().contains("enhancers.json"));
    assert_eq!(preview["matched_rules"], json!([]));
    // Same exact compiler, inputs and disk configuration as the API.
    let prompt = one_cli::prompt_config::PromptConfig::default();
    let env = one_cli::runtime::env_context::build_env_context(&f.root);
    let compiled = one_cli::runtime::prompt_compose::compile_host_with_agent_dir(
        one_cli::runtime::prompt_compose::HostPromptInput {
            prompt: &prompt,
            cwd: &f.root,
            provider: "gemini",
            model: "gemini-2.5-pro",
            mode: one_cli::runtime::AgentMode::Act,
            plan_path: None,
            tool_names: one_cli::runtime::harness::preview_tool_names(
                &one_cli::protocol::AgentSpec::builtin_main(),
            ),
            resources: "",
            env_context: Some(&env),
            memory_catalog: None,
            quirks: vec![],
        },
        &f.studio.paths().agent_dir,
    )
    .unwrap();
    assert_eq!(text, compiled.text);
    f.save("stops_early", false, "GEMINI_STOP_OVERRIDE_SENTINEL");
    let off = f.preview("gemini-2.5-pro");
    assert!(!off["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GEMINI_STOP_OVERRIDE_SENTINEL"));
    assert!(off["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GEMINI_PLAN_SENTINEL"));
    assert!(!f.preview("gemini-2.5-flash")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GEMINI_"));
    let path = f.root.join(".one/prompts/enhancers.json");
    let mut manual: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manual["bindings"][0]["enhancers"]["stops_early"] =
        json!({"enabled":true,"prompt":"MANUAL_EDIT_SENTINEL"});
    std::fs::write(&path, manual.to_string()).unwrap();
    assert!(f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("MANUAL_EDIT_SENTINEL"));
    f.call("POST", "", json!({"scope":"project","provider":"gemini","model":"gemini-2.5-pro","id":"stops_early","reset":true,"version":f.get()["version"]}), 200);
    assert!(!f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("MANUAL_EDIT_SENTINEL"));
    assert!(f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GEMINI_PLAN_SENTINEL"));
}

#[test]
fn conflict_invalid_config_and_registry_credentials() {
    let f = Fixture::new();
    let stale = f.get()["version"].clone();
    f.save("stops_early", true, "FIRST");
    let mut body = json!({"scope":"project","provider":"gemini","model":"gemini-2.5-pro","id":"stops_early","enabled":true,"prompt":"SECOND","version":stale});
    f.call("POST", "", body.clone(), 409);
    body["version"] = f.get()["version"].clone();
    body["prompt"] = json!("  ");
    f.call("POST", "", body.clone(), 422);
    body["id"] = json!("wire_compat");
    f.call("POST", "", body.clone(), 422);
    assert!(f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("FIRST"));
    std::fs::create_dir_all(&f.studio.paths().agent_dir).unwrap();
    std::fs::write(f.studio.paths().agent_dir.join("models.json"), r#"{"providers":{"gemini":{"apiKey":"DO_NOT_EXPOSE_THIS","models":[{"id":"gemini-2.5-pro","name":"My Gemini"}]}}}"#).unwrap();
    let catalog = f.get().to_string();
    assert!(catalog.contains("My Gemini"));
    assert!(!catalog.contains("DO_NOT_EXPOSE_THIS"));
    std::fs::write(f.root.join(".one/prompts/enhancers.json"), "invalid").unwrap();
    assert!(f.preview("gemini-2.5-pro")["error"]
        .as_str()
        .unwrap()
        .contains("enhancers.json"));
}

#[test]
fn global_project_profile_precedence_and_independent_fields() {
    let f = Fixture::new();
    let global = f.studio.paths().agent_dir.join("prompts/enhancers.json");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, json!({"version":1,"bindings":[{"provider":"gemini","model":"gemini-2.5-pro","enhancers":{"stops_early":{"enabled":true,"prompt":"GLOBAL_TEXT"}}}]}).to_string()).unwrap();
    let project = f.root.join(".one/prompts/enhancers.json");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, json!({"version":1,"bindings":[{"provider":"gemini","model":"gemini-2.5-pro","preset":"code","enhancers":{"stops_early":{"enabled":false}}}]}).to_string()).unwrap();
    assert!(!f.preview("gemini-2.5-pro")["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GLOBAL_TEXT"));
    let general = f
        .studio
        .preview_effective_prompt("general", "gemini", "gemini-2.5-pro");
    assert!(general["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("GLOBAL_TEXT"));
    let states = f.get();
    let stop = states["enhancers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "stops_early")
        .unwrap();
    assert!(stop["prompt_source"]
        .as_str()
        .unwrap()
        .contains("agent/prompts"));
    assert!(stop["binding_source"]
        .as_str()
        .unwrap()
        .contains(".one/prompts"));
}

#[test]
fn dedicated_behavior_hook_is_added_to_custom_profiles_without_extra_fallback() {
    use one_prompt::{Body, CompileContext, Component, ComponentRegistry, PromptSpec, Slot};
    let mut registry = ComponentRegistry::default();
    let mut spec = PromptSpec {
        preset: "custom".into(),
        components: vec![Component {
            id: "business".into(),
            slots: vec![Slot {
                id: "domain_role".into(),
                body: Body::text("CUSTOM_ROLE"),
                when: Default::default(),
            }],
        }],
        ..Default::default()
    };
    registry.preset("custom", vec![]).unwrap();
    one_cli::runtime::prompt_enhancer::PromptEnhancer::enhance(
        &mut spec,
        &registry,
        &[one_ai::registry::ModelQuirk::StopsEarly],
    );
    let compiled = one_prompt::compile(&spec, &registry, &CompileContext::default()).unwrap();
    assert!(
        compiled.text.contains("CUSTOM_ROLE")
            && compiled.text.contains("Persistence: Do not terminate")
    );
    assert_eq!(
        compiled
            .slots
            .iter()
            .find(|s| !s.operations.is_empty())
            .unwrap()
            .id,
        "behavior_hooks"
    );
    assert!(!compiled.slots.iter().any(|s| s.id == "extra"));
}

#[test]
fn custom_enhancer_apply_edit_disable_delete_and_effective_match() {
    let f = Fixture::new();
    let version =
        f.call("GET", "scope=global&studio_global=1", Value::Null, 200)["version"].clone();
    let apply = |version: &Value, body: Value, expected: u16| {
        let mut value = body;
        value["version"] = version.clone();
        f.studio
            .dispatch(&HttpRequest {
                method: "POST".into(),
                path: "/api/config/prompts/enhancers/apply".into(),
                raw_query: "studio_global=1".into(),
                headers: Default::default(),
                body: serde_json::to_vec(&value).unwrap(),
            })
            .status
            == expected
    };
    let create = json!({
        "id": "ParallelToolCalls",
        "name": "并行工具调用",
        "description": "多个工具调用彼此独立时，优先并行执行，减少无意义等待。",
        "prompt": "Prefer parallel tool calls when operations are independent.\nDo not serialize independent reads, searches, or inspections unnecessarily.",
        "enabled": true,
        "models": [{"provider": "gemini", "model": "gemini-2.5-pro"}],
        "presets": null
    });
    assert!(apply(&version, create.clone(), 200));
    let view = f.call(
        "GET",
        "scope=global&provider=gemini&model=gemini-2.5-pro&preset=code&studio_global=1",
        Value::Null,
        200,
    );
    assert!(view["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == "ParallelToolCalls" && e["enabled"] == true));
    let preview = f.studio.dispatch(&HttpRequest {
        method: "GET".into(),
        path: "/api/config/prompts/preview".into(),
        raw_query: "studio_global=1&preset=code&provider=gemini&model=gemini-2.5-pro".into(),
        headers: Default::default(),
        body: vec![],
    });
    assert_eq!(preview.status, 200);
    let preview: Value = serde_json::from_slice(&preview.body).unwrap();
    assert!(preview["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("Prefer parallel tool calls"));
    assert!(preview["slots"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["slot"] == "role" && s["text"].as_str().unwrap().contains("You are One")));
    let hit = preview["matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ParallelToolCalls")
        .unwrap();
    assert_eq!(hit["active"], true);
    let miss = f.studio.dispatch(&HttpRequest {
        method: "GET".into(),
        path: "/api/config/prompts/preview".into(),
        raw_query: "studio_global=1&preset=code&provider=gemini&model=gemini-2.5-flash".into(),
        headers: Default::default(),
        body: vec![],
    });
    let miss: Value = serde_json::from_slice(&miss.body).unwrap();
    let miss_row = miss["matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ParallelToolCalls")
        .unwrap();
    assert_eq!(miss_row["active"], false);
    assert!(miss_row["reasons"].to_string().contains("未绑定"));
    let builtin_delete = f.studio.dispatch(&HttpRequest {
        method: "POST".into(),
        path: "/api/config/prompts/enhancers/apply".into(),
        raw_query: "studio_global=1".into(),
        headers: Default::default(),
        body: serde_json::to_vec(&json!({"id":"stops_early","delete":true,"version":f.call("GET", "scope=global&studio_global=1", Value::Null, 200)["version"]})).unwrap(),
    });
    assert_eq!(builtin_delete.status, 422);
    let version =
        f.call("GET", "scope=global&studio_global=1", Value::Null, 200)["version"].clone();
    assert!(apply(
        &version,
        json!({"id":"ParallelToolCalls","name":"并行工具调用","description":"独立工具优先并行。","prompt":"Prefer parallel tool calls when operations are independent.","enabled":false,"models":[{"provider":"gemini","model":"gemini-2.5-pro"}]}),
        200
    ));
    let off = f.studio.dispatch(&HttpRequest {
        method: "GET".into(),
        path: "/api/config/prompts/preview".into(),
        raw_query: "studio_global=1&preset=code&provider=gemini&model=gemini-2.5-pro".into(),
        headers: Default::default(),
        body: vec![],
    });
    let off: Value = serde_json::from_slice(&off.body).unwrap();
    assert!(!off["compiled_prompt"]
        .as_str()
        .unwrap()
        .contains("Prefer parallel tool calls"));
    let version =
        f.call("GET", "scope=global&studio_global=1", Value::Null, 200)["version"].clone();
    assert!(apply(
        &version,
        json!({"id":"ParallelToolCalls","delete":true}),
        200
    ));
    let gone = f.call("GET", "scope=global&studio_global=1", Value::Null, 200);
    assert!(!gone["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == "ParallelToolCalls"));
}
