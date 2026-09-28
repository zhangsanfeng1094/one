//! `cargo run -p one-prompt --example external_agent`
//! No One runtime, tool implementation, or provider client is required.
use one_prompt::{compile, CompileContext, ComponentRegistry, PromptSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/prompts/general.toml");
    let spec = PromptSpec::load(path)?;
    let mut context = CompileContext {
        provider: "my-provider".into(),
        model: "my-model".into(),
        mode: "act".into(),
        ..Default::default()
    };
    // Derive capabilities from the tools actually exposed by your own agent.
    context.capabilities.insert("web_search".into());
    context.variables.insert(
        "environment".into(),
        "\nResearch workspace: example\n".into(),
    );
    let compiled = compile(&spec, &ComponentRegistry::with_builtins(), &context)?;
    // Pass compiled.text as the system message to your existing provider SDK.
    println!("{}", compiled.text);
    eprintln!(
        "{} slots; matching rules: {:?}",
        compiled.slots.len(),
        compiled.matched_rules
    );
    Ok(())
}
