//! Intelligent message chunker for IM platforms (Hermes-aligned).
//!
//! Handles splitting long LLM responses that exceed platform character limits
//! (e.g., Telegram 4096 chars, Discord 2000 chars) while preserving markdown
//! code blocks and paragraph boundaries.

/// Split a message into chunks that do not exceed `max_len`.
/// Preserves markdown code block markers across chunk boundaries.
pub fn chunk_markdown(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len || max_len == 0 {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    let mut inside_code_block = false;
    let mut code_block_lang = String::new();

    let lines: Vec<&str> = text.split('\n').collect();

    for line in lines {
        let is_code_fence = line.trim_start().starts_with("```");

        // A generated response can contain a single enormous line (JSON,
        // minified source, or a long URL). Line-oriented splitting alone
        // would otherwise emit an over-limit platform message.
        if line.len() > max_len {
            if !current_chunk.is_empty() {
                chunks.push(current_chunk);
                current_chunk = String::new();
            }

            let mut remainder = line;
            while !remainder.is_empty() {
                let mut end = remainder.len().min(max_len);
                while end > 0 && !remainder.is_char_boundary(end) {
                    end -= 1;
                }
                // `max_len > 0` and UTF-8 always has a character boundary at
                // the end of its first scalar value, so this cannot loop.
                if end == 0 {
                    end = remainder
                        .char_indices()
                        .nth(1)
                        .map_or(remainder.len(), |(index, _)| index);
                }
                chunks.push(remainder[..end].to_string());
                remainder = &remainder[end..];
            }
            continue;
        }

        // If adding this line would exceed max_len
        if !current_chunk.is_empty() && current_chunk.len() + line.len() + 1 > max_len {
            if inside_code_block {
                // Close current code block before splitting
                current_chunk.push_str("\n```");
                chunks.push(current_chunk);
                // Re-open code block in next chunk
                current_chunk = format!("```{code_block_lang}\n{line}");
            } else {
                chunks.push(current_chunk);
                current_chunk = line.to_string();
            }
        } else {
            if !current_chunk.is_empty() {
                current_chunk.push('\n');
            }
            current_chunk.push_str(line);
        }

        if is_code_fence {
            if inside_code_block {
                inside_code_block = false;
                code_block_lang.clear();
            } else {
                inside_code_block = true;
                code_block_lang = line.trim_start().trim_start_matches('`').trim().to_string();
            }
        }
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_short_text() {
        let text = "Hello world";
        let chunks = chunk_markdown(text, 100);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn test_chunk_paragraphs() {
        let text = "Paragraph 1\nParagraph 2\nParagraph 3";
        let chunks = chunk_markdown(text, 25);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Paragraph 1\nParagraph 2");
        assert_eq!(chunks[1], "Paragraph 3");
    }

    #[test]
    fn test_chunk_single_long_line() {
        let chunks = chunk_markdown(&"你".repeat(5_000), 4_000);
        assert_eq!(chunks.len(), 4);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4_000));
        assert_eq!(chunks.concat(), "你".repeat(5_000));
    }

    #[test]
    fn test_chunk_inside_code_block() {
        let text = "```rust\nfn main() {\n    println!(\"hello\");\n}\n```";
        let chunks = chunk_markdown(text, 30);
        assert!(chunks.len() >= 2);
        // First chunk ends with ```
        assert!(chunks[0].ends_with("```"));
        // Second chunk starts with ```rust
        assert!(chunks[1].starts_with("```rust"));
    }
}
