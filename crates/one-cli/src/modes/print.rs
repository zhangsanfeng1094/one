use one_core::agent::LlmProvider;

use crate::runtime::AppRuntime;

pub async fn run_print(
    runtime: &mut AppRuntime,
    provider: &dyn LlmProvider,
    prompt: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.subscribe_printer(json).await;
    let output = runtime.prompt(provider, prompt).await?;
    if !json {
        if !output.is_empty() {
            println!("{output}");
        }
    } else {
        let line = serde_json::json!({"type":"final","text": output});
        println!("{line}");
    }
    Ok(())
}
