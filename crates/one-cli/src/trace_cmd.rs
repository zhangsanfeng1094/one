//! `one trace-stats` — summarize a JSONL agent execution trace.

use crate::cli::TraceStatsCli;

pub fn run_trace_stats(cli: TraceStatsCli) -> Result<(), Box<dyn std::error::Error>> {
    let events = one_core::load_trace_file(&cli.path)?;
    if events.is_empty() {
        return Err(format!("no events in {}", cli.path.display()).into());
    }
    let stats = one_core::TraceStats::from_events(&events);
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!("trace: {}", cli.path.display());
        println!("events: {}", events.len());
        println!("{}", stats.format_report());
    }
    Ok(())
}
