pub mod agents;
pub mod builtin_skills;
pub mod error;
pub mod loader;
pub mod prompts;
pub mod skills;

pub use agents::AgentsFile;
pub use builtin_skills::builtin_skill_names;
pub use error::{ResourceError, Result};
pub use loader::ResourceLoader;
pub use prompts::PromptTemplate;
pub use skills::{skills_catalog_xml, Skill};