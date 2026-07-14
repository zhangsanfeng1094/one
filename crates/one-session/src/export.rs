use crate::entries::SessionEntry;
use crate::manager::SessionManager;

pub fn export_html(session: &SessionManager) -> String {
    let title = session
        .session_name()
        .unwrap_or_else(|| "One Session".to_string());
    let mut body = String::new();

    for entry in session.entries() {
        match entry {
            SessionEntry::Message { message, base, .. } => {
                body.push_str(&format!(
                    "<div class=\"entry\"><small>{}</small><pre>{}</pre></div>\n",
                    base.id,
                    html_escape(&format!("{message:?}"))
                ));
            }
            SessionEntry::Compaction { summary, base, .. } => {
                body.push_str(&format!(
                    "<div class=\"compaction\"><small>{}</small><p>{}</p></div>\n",
                    base.id,
                    html_escape(summary)
                ));
            }
            _ => {}
        }
    }

    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 900px; margin: 2rem auto; }}
.entry, .compaction {{ margin: 1rem 0; padding: 1rem; border: 1px solid #ddd; border-radius: 8px; }}
pre {{ white-space: pre-wrap; word-break: break-word; }}
small {{ color: #666; }}
</style></head><body>
<h1>{title}</h1>
{body}
</body></html>"#
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}