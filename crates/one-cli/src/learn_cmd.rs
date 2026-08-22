//! Handler for `one learn` CLI subcommand.

use crate::cli::LearnCli;
use one_resources::IntentGraph;
use std::path::Path;

pub async fn run_learn(cli: LearnCli, cwd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let agent_dir = one_session::agent_dir();
    let mut graph = IntentGraph::load_merged(cwd, &agent_dir);
    let custom_path = agent_dir.join("intent_graph").join("custom.json");

    if let Some(query) = cli.test.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let result = graph.infer(query, &std::collections::HashMap::new());
        println!("Query: {query}");
        if result.matched_intents.is_empty()
            && result.active_reminders.is_empty()
            && result.suggested_tools.is_empty()
        {
            println!("(no intent matched)");
            return Ok(());
        }
        if !result.matched_intents.is_empty() {
            println!("\nMatched intents:");
            for intent in &result.matched_intents {
                println!(
                    "  · {} ({})  confidence={:.2}  evidence={}",
                    intent.intent_name,
                    intent.intent_id,
                    intent.confidence,
                    intent.evidence_path.join(" -> ")
                );
            }
        }
        if !result.active_entities.is_empty() {
            println!("\nEntities: {}", result.active_entities.join(", "));
        }
        if !result.active_reminders.is_empty() {
            println!("\nReminders:");
            for rem in &result.active_reminders {
                println!(
                    "  · {} [{}] ({})\n    {}",
                    rem.level.badge(),
                    rem.title,
                    rem.reminder_id,
                    rem.rendered_content
                );
            }
        }
        if !result.suggested_tools.is_empty() {
            println!("\nSuggested tools:");
            for tool in &result.suggested_tools {
                println!("  · {} (from {})", tool.tool_name, tool.source_intent_id);
            }
        }
        return Ok(());
    }

    if cli.reset {
        graph.clear_custom_rules();
        if custom_path.exists() {
            let _ = std::fs::remove_file(&custom_path);
        }
        println!("✅ 已重置意图图谱自定义规则，恢复为内置基础图谱。");
        return Ok(());
    }

    if cli.status {
        let total_nodes = graph.nodes.len();
        let total_edges = graph.edges.len();
        let custom_rules = graph.list_custom_rules().len();
        let triggers = graph
            .nodes
            .values()
            .filter(|n| matches!(n, one_resources::GraphNode::Trigger { .. }))
            .count();

        println!("Intent Graph 状态统计:");
        println!("  · 总节点数:   {total_nodes}");
        println!("  · 总边数:     {total_edges}");
        println!("  · 触发器数:   {triggers}");
        println!("  · 自定义规则: {custom_rules}");
        println!("  · 持久化路径: {}", custom_path.display());
        return Ok(());
    }

    if cli.list {
        let rules = graph.list_custom_rules();
        if rules.is_empty() {
            println!("暂无自定义学习规则（当前使用内置基础图谱）。");
            println!("使用示例: one learn \"当用户询问架构时，建议优先使用 find 和 deepwiki\"");
        } else {
            println!("已学习的自定义意图规则 (共 {} 条):", rules.len());
            println!("{}", "-".repeat(60));
            for (i, r) in rules.iter().enumerate() {
                let tools_str = if r.suggested_tools.is_empty() {
                    "-".to_string()
                } else {
                    r.suggested_tools.join(", ")
                };
                println!(
                    "{}. [{}] {} ({})",
                    i + 1,
                    r.reminder_level.badge(),
                    r.intent_name,
                    r.intent_id
                );
                println!("   触发: {}", r.triggers.join(", "));
                println!("   提醒: {}", r.reminder_content);
                println!("   工具: {}", tools_str);
                println!();
            }
        }
        return Ok(());
    }

    match cli.rule.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(rule_text) => {
            let summary = graph
                .learn_from_text(rule_text)
                .map_err(|e| format!("学习规则失败: {e}"))?;
            graph.save_custom_to_file(&custom_path)?;

            println!("🎓 意图图谱已录入新规则！");
            println!(
                "  · 意图名称: {} ({})",
                summary.intent_name, summary.intent_id
            );
            println!("  · 约束等级: {}", summary.reminder_level.badge());
            println!("  · 触发特征: {}", summary.triggers.join(", "));
            println!("  · 提醒指引: {}", summary.reminder_content);
            if !summary.suggested_tools.is_empty() {
                println!("  · 推荐工具: {}", summary.suggested_tools.join(", "));
            }
            println!("  · 持久化至: {}", custom_path.display());
        }
        None => {
            println!("用法: one learn <规则文本> [选项]");
            println!("选项:");
            println!("  -l, --list    列出所有自定义学习规则");
            println!("  -s, --status  查看意图图谱统计信息");
            println!("      --reset   重置为内置图谱");
            println!("      --test Q  对问句做一次意图推理（不写入规则）");
            println!();
            println!("示例:");
            println!("  one learn \"当用户询问架构或源码实现时，建议优先使用 find 和 deepwiki 定位与查阅\"");
            println!("  one learn \"意图: 数据库迁移 | 触发: 迁移,执行 + 数据库,db | 提醒: 务必先备份数据 | 级别: 强制 | 工具: bash\"");
            println!("  one learn --test \"帮我查一下 reqwest 的用法\"");
        }
    }

    Ok(())
}
