pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod sandbox;
pub mod write;
#[cfg(feature = "network")]
pub mod web_fetch;
#[cfg(feature = "network")]
pub mod web_search;

use std::sync::Arc;

use one_core::tool::Tool;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;
#[cfg(feature = "network")]
pub use web_fetch::WebFetchTool;
#[cfg(feature = "network")]
pub use web_search::WebSearchTool;

pub fn default_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools(cwd)
}

pub fn coding_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    coding_tools_with_approve(cwd, true)
}

pub fn coding_tools_with_approve(cwd: std::path::PathBuf, auto_approve: bool) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(WriteTool::new(cwd.clone())),
        Arc::new(EditTool::new(cwd.clone())),
        Arc::new(BashTool::with_auto_approve(cwd.clone(), auto_approve)),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(WebSearchTool::new()));
        tools.push(Arc::new(WebFetchTool::new()));
    }
    tools
}

pub fn read_only_tools(cwd: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(FindTool::new(cwd.clone())),
        Arc::new(LsTool::new(cwd)),
    ];
    #[cfg(feature = "network")]
    {
        tools.push(Arc::new(WebSearchTool::new()));
        tools.push(Arc::new(WebFetchTool::new()));
    }
    tools
}
