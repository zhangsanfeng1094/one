//! Embedded assets for One Web UI (Vite + React SPA).

pub const INDEX_HTML: &str = include_str!("../web/dist/index.html");
pub const APP_JS: &str = include_str!("../web/dist/assets/app.js");
pub const INDEX_CSS: &str = include_str!("../web/dist/assets/index.css");

pub enum AssetFile {
    Html(&'static str),
    Js(&'static str),
    Css(&'static str),
}

impl AssetFile {
    pub fn content_type(&self) -> &'static str {
        match self {
            AssetFile::Html(_) => "text/html; charset=utf-8",
            AssetFile::Js(_) => "application/javascript; charset=utf-8",
            AssetFile::Css(_) => "text/css; charset=utf-8",
        }
    }

    pub fn body(&self) -> &'static str {
        match self {
            AssetFile::Html(s) | AssetFile::Js(s) | AssetFile::Css(s) => s,
        }
    }
}

pub fn get_asset(path: &str) -> Option<AssetFile> {
    match path {
        "/" | "/index.html" => Some(AssetFile::Html(INDEX_HTML)),
        "/assets/app.js" => Some(AssetFile::Js(APP_JS)),
        "/assets/index.css" => Some(AssetFile::Css(INDEX_CSS)),
        _ => {
            // For SPA routing fallback, serve index.html
            if !path.starts_with("/api/") && !path.starts_with("/ws") && !path.contains('.') {
                Some(AssetFile::Html(INDEX_HTML))
            } else {
                None
            }
        }
    }
}
