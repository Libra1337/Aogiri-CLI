//! How replies look on each platform: Markdown from the engine split into
//! messages the platform accepts, and Telegram's HTML.

/// `text` in pieces of at most `limit` characters, cut between lines. A
/// code block cut in two is closed at the end of one piece and opened again
/// at the start of the next.
pub(crate) fn split(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(64);
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut size = 0;
    // The line that opened the code block the text is in.
    let mut fence: Option<String> = None;
    for line in text.trim_end().split('\n') {
        let mut line = line.to_string();
        // A single line longer than a piece goes in parts.
        while line.chars().count() > limit - 8 {
            let cut: String = line.chars().take(limit - 8).collect();
            line = line.chars().skip(limit - 8).collect();
            push_line(&mut pieces, &mut current, &mut size, &fence, &cut, limit);
        }
        push_line(&mut pieces, &mut current, &mut size, &fence, &line, limit);
        if line.trim_start().starts_with("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(line.trim().to_string()),
            };
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current.trim_end().to_string());
    }
    pieces
}

fn push_line(
    pieces: &mut Vec<String>,
    current: &mut String,
    size: &mut usize,
    fence: &Option<String>,
    line: &str,
    limit: usize,
) {
    let length = line.chars().count() + 1;
    // Room for the fence that closes this piece.
    let reserve = if fence.is_some() { 4 } else { 0 };
    if *size + length + reserve > limit && !current.trim().is_empty() {
        let mut piece = current.trim_end().to_string();
        if fence.is_some() {
            piece.push_str("\n```");
        }
        pieces.push(piece);
        current.clear();
        *size = 0;
        if let Some(open) = fence {
            current.push_str(open);
            current.push('\n');
            *size = open.chars().count() + 1;
        }
    }
    current.push_str(line);
    current.push('\n');
    *size += length;
}

/// Telegram HTML (`parse_mode: "HTML"`) for Markdown: code blocks, inline
/// code, bold and headings; everything else is escaped text.
pub(crate) fn telegram_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut code: Option<String> = None;
    for line in markdown.split('\n') {
        let trimmed = line.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            match code.take() {
                Some(block) => {
                    out.push_str(&block);
                    out.push_str("</code></pre>\n");
                }
                None => {
                    let language: String = language
                        .trim()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '#'))
                        .collect();
                    code = Some(if language.is_empty() {
                        "<pre><code>".to_string()
                    } else {
                        format!("<pre><code class=\"language-{language}\">")
                    });
                }
            }
            continue;
        }
        if let Some(block) = code.as_mut() {
            block.push_str(&escape(line));
            block.push('\n');
            continue;
        }
        let heading = trimmed.trim_start_matches('#');
        if trimmed.starts_with('#') && heading.starts_with(' ') {
            out.push_str("<b>");
            out.push_str(&inline(heading.trim()));
            out.push_str("</b>\n");
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("• ");
            out.push_str(&inline(item));
            out.push('\n');
        } else {
            out.push_str(&inline(line));
            out.push('\n');
        }
    }
    // A block the text never closed.
    if let Some(block) = code {
        out.push_str(&block);
        out.push_str("</code></pre>\n");
    }
    out.trim_end().to_string()
}

/// One line: `code` and **bold**, the rest escaped.
fn inline(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let tick = rest.find('`');
        let star = rest.find("**");
        match (tick, star) {
            (Some(at), star) if star.is_none_or(|star| at < star) => {
                match rest[at + 1..].find('`') {
                    Some(end) => {
                        out.push_str(&escape(&rest[..at]));
                        out.push_str("<code>");
                        out.push_str(&escape(&rest[at + 1..at + 1 + end]));
                        out.push_str("</code>");
                        rest = &rest[at + 2 + end..];
                    }
                    None => break,
                }
            }
            (_, Some(at)) => match rest[at + 2..].find("**") {
                Some(end) if end > 0 => {
                    out.push_str(&escape(&rest[..at]));
                    out.push_str("<b>");
                    out.push_str(&escape(&rest[at + 2..at + 2 + end]));
                    out.push_str("</b>");
                    rest = &rest[at + 4 + end..];
                }
                _ => break,
            },
            _ => break,
        }
    }
    out.push_str(&escape(rest));
    out
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `2 分 13 秒`, `45 秒`, `1 小时 5 分` (or English).
pub(crate) fn duration(seconds: u64, zh: bool) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (zh, hours, minutes) {
        (true, 0, 0) => format!("{seconds} 秒"),
        (true, 0, _) => format!("{minutes} 分 {seconds} 秒"),
        (true, _, _) => format!("{hours} 小时 {minutes} 分"),
        (false, 0, 0) => format!("{seconds}s"),
        (false, 0, _) => format!("{minutes}m {seconds}s"),
        (false, _, _) => format!("{hours}h {minutes}m"),
    }
}

/// The first `limit` characters of `text`, with `…` when cut.
pub(crate) fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(split("hello\n\nworld\n", 100), vec!["hello\n\nworld"]);
        assert!(split("  \n", 100).is_empty());
    }

    #[test]
    fn long_text_splits_between_lines_and_keeps_code_blocks_whole() {
        let mut text = String::from("intro\n```rust\n");
        for n in 0..40 {
            text.push_str(&format!("let value_{n} = {n};\n"));
        }
        text.push_str("```\nend");
        let pieces = split(&text, 200);
        assert!(pieces.len() > 2);
        for piece in &pieces {
            assert!(piece.chars().count() <= 200, "{piece}");
            // Every piece has its fences in pairs.
            assert_eq!(piece.matches("```").count() % 2, 0, "{piece}");
        }
        assert!(pieces[1].starts_with("```rust\n"));
        assert!(pieces.last().unwrap().ends_with("end"));
        let joined = pieces.join("\n");
        assert!(joined.contains("let value_39 = 39;"));
    }

    #[test]
    fn a_line_longer_than_a_piece_is_cut() {
        let line = "x".repeat(300);
        let pieces = split(&line, 100);
        assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), 300);
        assert!(pieces.iter().all(|p| p.len() <= 100));
    }

    #[test]
    fn telegram_gets_escaped_html_with_code_and_bold() {
        assert_eq!(
            telegram_html("# 结果\n用 `a<b>` 和 **很重要** & 其他\n- 一项\n```rust\nif a < b {}\n```"),
            "<b>结果</b>\n用 <code>a&lt;b&gt;</code> 和 <b>很重要</b> &amp; 其他\n• 一项\n<pre><code class=\"language-rust\">if a &lt; b {}\n</code></pre>"
        );
        // Unpaired marks stay as text.
        assert_eq!(telegram_html("a ` b ** c"), "a ` b ** c");
        assert_eq!(telegram_html("```\nopen"), "<pre><code>open\n</code></pre>");
    }

    #[test]
    fn durations_and_clips_read_naturally() {
        assert_eq!(duration(133, true), "2 分 13 秒");
        assert_eq!(duration(45, true), "45 秒");
        assert_eq!(duration(3900, true), "1 小时 5 分");
        assert_eq!(duration(133, false), "2m 13s");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }
}
