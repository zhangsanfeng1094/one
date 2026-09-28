use one_prompt::*;

fn run(spec: &PromptSpec, context: &CompileContext) -> Result<CompiledPrompt, PromptError> {
    compile(spec, &ComponentRegistry::with_builtins(), context)
}
fn rule(provider: &str, model: &str, operations: Vec<SlotOperation>) -> ModelRule {
    ModelRule {
        provider: provider.into(),
        model: model.into(),
        operations,
    }
}

#[test]
fn code_preserves_original_prose_and_order() {
    let mut context = CompileContext::default();
    context.capabilities.insert("monitor".into());
    let p = run(&PromptSpec::default(), &context).unwrap();
    assert_eq!(
        p.text,
        format!(
            "{}{}",
            builtin::DEFAULT_SYSTEM_PROMPT,
            builtin::ONE_OUTPUT_GUIDE
        )
    );
    assert_eq!(p.slots.first().unwrap().id, "role");
    assert!(p.slots.iter().all(|s| s.source.starts_with("builtin:")));
}

#[test]
fn general_has_no_coding_or_one_cli_requirements() {
    let spec = PromptSpec {
        preset: "general".into(),
        ..Default::default()
    };
    let p = run(&spec, &CompileContext::default()).unwrap();
    for text in [
        "Rust",
        "git",
        "coding",
        "One Output",
        "task`",
        "memory_write",
        "Plan mode",
    ] {
        assert!(!p.text.contains(text), "{text}");
    }
    assert!(p.text.contains("general-purpose"));
}

#[test]
fn custom_components_and_ordered_operations() {
    let mut spec = PromptSpec::role("role\n");
    spec.components.push(Component {
        id: "team".into(),
        slots: vec![Slot {
            id: "team.policy".into(),
            body: Body::text("base"),
            when: Condition::default(),
        }],
    });
    spec.operations.extend([
        SlotOperation::append("team.policy", " first"),
        SlotOperation::replace("team.policy", "new"),
        SlotOperation::append("team.policy", " last"),
    ]);
    let p = run(&spec, &CompileContext::default()).unwrap();
    assert_eq!(p.text, "role\nnew last");
    assert_eq!(p.slots.last().unwrap().operations.len(), 3);
}

#[test]
fn disable_append_errors_and_replace_reenables() {
    let mut spec = PromptSpec::role("base");
    spec.operations.push(SlotOperation {
        slot: "role".into(),
        op: Operation::Disable,
        body: None,
        source: String::new(),
    });
    assert!(!run(&spec, &CompileContext::default())
        .unwrap()
        .text
        .contains("base"));
    spec.operations.push(SlotOperation::append("role", "bad"));
    let e = run(&spec, &CompileContext::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidOperation);
    assert!(e.source_location.contains("operations[2]"));
    spec.operations.pop();
    spec.operations.push(SlotOperation::replace("role", "good"));
    assert_eq!(run(&spec, &CompileContext::default()).unwrap().text, "good");
}

#[test]
fn exact_wildcards_conjunction_and_declaration_order() {
    let mut spec = PromptSpec::role("base");
    spec.rules = vec![
        rule(
            "vendor",
            "model-a",
            vec![SlotOperation::append("role", " exact")],
        ),
        rule(
            "ven*",
            "model-*",
            vec![SlotOperation::append("role", " wildcard")],
        ),
        rule(
            "vendor",
            "*a",
            vec![SlotOperation::replace("role", "winner")],
        ),
        rule(
            "other",
            "*",
            vec![SlotOperation::append("role", " wrong-provider")],
        ),
        rule(
            "vendor",
            "b",
            vec![SlotOperation::append("role", " wrong-model")],
        ),
    ];
    let ctx = CompileContext {
        provider: "vendor".into(),
        model: "model-a".into(),
        ..Default::default()
    };
    let p = run(&spec, &ctx).unwrap();
    assert_eq!(p.text, "winner");
    assert_eq!(p.matched_rules, vec![0, 1, 2]);
    assert_eq!(p.slots[0].operations.last().unwrap().rule, Some(2));
    assert_eq!(run(&spec, &CompileContext::default()).unwrap().text, "base");
}

#[test]
fn a_b_a_is_deterministic_without_accumulation() {
    let mut spec = PromptSpec::role("base");
    spec.rules
        .push(rule("*", "B", vec![SlotOperation::append("role", " B")]));
    let a = CompileContext {
        model: "A".into(),
        ..Default::default()
    };
    let b = CompileContext {
        model: "B".into(),
        ..Default::default()
    };
    let before = run(&spec, &a).unwrap();
    for _ in 0..10 {
        assert_eq!(run(&spec, &b).unwrap().text, "base B");
        assert_eq!(run(&spec, &a).unwrap(), before);
    }
}

#[test]
fn model_cannot_enable_unavailable_capabilities_or_wrong_mode() {
    let mut spec = PromptSpec::default();
    spec.rules.push(rule(
        "*",
        "*",
        vec![
            SlotOperation::replace("subagent", "SPAWN"),
            SlotOperation::replace("memory_write", "WRITE_MEMORY"),
            SlotOperation::replace("planning", "PLAN"),
        ],
    ));
    let mut context = CompileContext::default();
    let p = run(&spec, &context).unwrap();
    for s in ["SPAWN", "WRITE_MEMORY", "PLAN"] {
        assert!(!p.text.contains(s));
    }
    context
        .capabilities
        .extend(["task", "memory", "memory_write", "plan"].map(String::from));
    let act = run(&spec, &context).unwrap();
    assert!(act.text.contains("SPAWN") && act.text.contains("WRITE_MEMORY"));
    assert!(!act.text.contains("PLAN"));
    context.mode = "plan".into();
    let plan = run(&spec, &context).unwrap();
    assert!(plan.text.contains("PLAN") && !plan.text.contains("WRITE_MEMORY"));
}

#[test]
fn unknown_slots_and_bad_operations_are_validated_in_unmatched_rules() {
    let mut spec = PromptSpec::default();
    spec.source = "agent/prompt.toml".into();
    spec.rules.push(rule(
        "unused",
        "unused",
        vec![SlotOperation::replace("typo", "x")],
    ));
    let e = run(&spec, &CompileContext::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::UnknownSlot);
    assert_eq!(
        e.source_location,
        "agent/prompt.toml: rules[0].operations[0]"
    );
    spec.rules[0].operations[0].slot = "role".into();
    spec.rules[0].operations[0].op = Operation::Disable;
    assert_eq!(
        run(&spec, &CompileContext::default()).unwrap_err().kind,
        ErrorKind::InvalidOperation
    );
}

#[test]
fn duplicate_ids_are_errors() {
    let mut spec = PromptSpec::default();
    spec.components.push(Component {
        id: "code_role".into(),
        slots: vec![],
    });
    assert_eq!(
        run(&spec, &CompileContext::default()).unwrap_err().kind,
        ErrorKind::DuplicateComponent
    );
    spec.components[0] = Component {
        id: "custom".into(),
        slots: vec![Slot {
            id: "role".into(),
            body: Body::text("x"),
            when: Condition::default(),
        }],
    };
    assert_eq!(
        run(&spec, &CompileContext::default()).unwrap_err().kind,
        ErrorKind::DuplicateSlot
    );
}

#[test]
fn variables_are_required_and_values_are_not_recursively_parsed() {
    let spec = PromptSpec::role("{{required}}");
    let mut context = CompileContext::default();
    assert_eq!(
        run(&spec, &context).unwrap_err().kind,
        ErrorKind::MissingVariable
    );
    context.variables.insert(
        "required".into(),
        "{{not_a_variable}}\n[[rules]]\nversion = 9".into(),
    );
    assert_eq!(
        run(&spec, &context).unwrap().text,
        context.variables["required"]
    );
}

#[test]
fn file_loading_is_relative_and_separate_from_compilation() {
    let root = std::env::temp_dir().join(format!("one-prompt-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("prompt.toml");
    std::fs::write(&path, "version = 1\npreset = 'general'\n[[operations]]\nslot = 'role'\nop = 'replace'\nbody = {file = 'role.md'}\n").unwrap();
    let e = PromptSpec::load(&path).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Io);
    assert!(e.source_location.contains("role.md") && e.source_location.contains("prompt.toml"));
    std::fs::write(root.join("role.md"), "File {{name}}").unwrap();
    let spec = PromptSpec::load(&path).unwrap();
    let e = run(&spec, &CompileContext::default()).unwrap_err();
    assert!(e.source_location.contains("role.md"));
    std::fs::remove_dir_all(&root).unwrap();
    let mut ctx = CompileContext::default();
    ctx.variables.insert("name".into(), "value".into());
    assert_eq!(run(&spec, &ctx).unwrap().text, "File value");
}

#[test]
fn dsl_is_strictly_versioned_and_rejects_unknown_fields() {
    assert!(PromptSpec::parse("preset = 'code'", "version.toml").is_err());
    assert_eq!(
        run(
            &PromptSpec::parse("version = 2", "version.toml").unwrap(),
            &CompileContext::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Version
    );
    let e = PromptSpec::parse("version = 1\nunknown = true", "strict.toml").unwrap_err();
    assert_eq!(e.kind, ErrorKind::Parse);
    assert!(e.to_string().contains("strict.toml"));
}

#[test]
fn external_registry_presets_and_duplicate_registration() {
    let mut registry = ComponentRegistry::default();
    let c = Component {
        id: "external".into(),
        slots: vec![Slot {
            id: "external.role".into(),
            body: Body::text("External {{task}}"),
            when: Condition::default(),
        }],
    };
    registry.register(c.clone()).unwrap();
    assert_eq!(
        registry.register(c).unwrap_err().kind,
        ErrorKind::DuplicateComponent
    );
    registry
        .preset("external", vec!["external".into()])
        .unwrap();
    let mut context = CompileContext::default();
    context.variables.insert("task".into(), "agent".into());
    let spec = PromptSpec {
        preset: "external".into(),
        ..Default::default()
    };
    assert_eq!(
        compile(&spec, &registry, &context).unwrap().text,
        "External agent"
    );
}

#[test]
fn shipped_examples_load_and_compile() {
    for name in ["code", "general"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("examples/prompts/{name}.toml"));
        let spec = PromptSpec::load(path).unwrap();
        let context = CompileContext {
            provider: "example-provider".into(),
            model: "example-model-small".into(),
            ..Default::default()
        };
        let prompt = run(&spec, &context).unwrap();
        assert!(!prompt.text.is_empty());
        if name == "code" {
            assert_eq!(prompt.matched_rules, vec![0, 1]);
        }
    }
}
