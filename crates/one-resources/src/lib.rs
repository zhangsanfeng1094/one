pub mod agents;
pub mod builtin_skills;
pub mod error;
pub mod intent_graph;
mod intent_match;
pub mod loader;
pub mod memory;
pub mod prompts;
pub mod skills;

pub use agents::AgentsFile;
pub use builtin_skills::builtin_skill_names;
pub use error::{ResourceError, Result};
pub use intent_graph::{
    is_conceptual_clarification, strip_xml_and_meta, ActiveReminder, EdgeEntry, GraphEdge,
    GraphInferenceResult, GraphNode, InferOptions, IntentGraph, LearnedRuleSummary, MatchMode,
    MatchedIntent, ReminderLevel, SuggestedTool,
};
pub use loader::{skill_allowlist_roots, skill_discovery_dirs, ResourceLoader};
pub use memory::{
    archive_session_summary, ensure_memory_dirs, format_index_entry_line, load_memory_catalog,
    load_memory_catalog_sync, match_tool_intent_rules, memory_readable_roots, memory_root,
    memory_writable_roots, project_memory_dir, project_slug, scaffold_memory_body,
    search_memory_index, sessions_memory_dir, strip_memory_frontmatter, upsert_memory_entry,
    validate_memory_id, MemoryCatalog, MemoryIndexEntry, MemoryLoadOptions, MemorySearchHit,
    MemorySearchSource, MemoryUpsertInput, MemoryUpsertResult, ToolIntentHit,
    DEFAULT_INDEX_MAX_LINES, DEFAULT_MAX_LOOKUPS_PER_TURN,
};
pub use prompts::PromptTemplate;
pub use skills::{
    apply_skills_config, discover_skills, set_skill_enabled, skills_catalog_xml, Skill,
    SkillConfigEntry,
};
