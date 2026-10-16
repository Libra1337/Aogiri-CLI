//! The filter every text a chat bot sends passes: chat history lives on the
//! platform's servers, so anything that looks like a credential, and paths
//! of credential files, are hidden (docs/chat-connectors.md, "Redaction").

/// What a hidden secret reads as.
pub(crate) const MASK: &str = "[已隐藏疑似密钥]";
/// What a hidden credential path reads as.
pub(crate) const PATH_MASK: &str = "[已隐藏凭据路径]";

/// Token prefixes and the shortest token (prefix included) they mark.
const PREFIXES: [(&str, usize); 14] = [
    ("sk-", 20),
    ("ghp_", 24),
    ("gho_", 24),
    ("ghu_", 24),
    ("ghs_", 24),
    ("ghr_", 24),
    ("github_pat_", 30),
    ("xoxb-", 15),
    ("xoxp-", 15),
    ("xoxa-", 15),
    ("xoxr-", 15),
    ("AKIA", 20),
    ("ASIA", 20),
    ("AIza", 35),
];

/// Parts of a setting's name that mark its value as a secret.
const SECRET_NAMES: [&str; 6] = [
    "secret",
    "token",
    "password",
    "passwd",
    "credential",
    "auth",
];

/// Paths that hold credentials (lowercase, `/` separators).
const CREDENTIAL_PATHS: [&str; 14] = [
    ".ssh/",
    ".aws/",
    ".gnupg/",
    ".lynshen/auth.json",
    ".lynshen/secret.key",
    ".lynshen/daemon",
    "connectors.json",
    ".netrc",
    ".git-credentials",
    ".docker/config.json",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "secret.key",
];

/// `text` with secrets hidden. `known` are exact secrets to hide wherever
/// they appear (the connectors' own credentials).
pub(crate) fn redact(text: &str, known: &[String]) -> String {
    let mut text = text.to_string();
    for secret in known.iter().filter(|secret| secret.len() >= 8) {
        if text.contains(secret.as_str()) {
            text = text.replace(secret.as_str(), MASK);
        }
    }
    let text = pem_blocks(&text);
    let text = tokens(&text);
    let text = bearer(&text);
    let text: Vec<String> = text.split('\n').map(assignments).collect();
    credential_paths(&text.join("\n"))
}

/// Private keys in PEM armor, whole.
fn pem_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let header_end = rest[start..]
            .find('\n')
            .map_or(rest.len(), |end| start + end);
        if !rest[start..header_end].contains("PRIVATE KEY") {
            out.push_str(&rest[..header_end]);
            rest = &rest[header_end..];
            continue;
        }
        let Some(footer) = rest[header_end..].find("-----END ") else {
            // No end: hide everything after the header.
            out.push_str(&rest[..start]);
            out.push_str(MASK);
            return out;
        };
        let footer = header_end + footer;
        // The footer line ends with its closing dashes.
        let after = rest[footer + 9..]
            .find("-----")
            .map_or(rest.len(), |end| footer + 9 + end + 5);
        out.push_str(&rest[..start]);
        out.push_str(MASK);
        rest = &rest[after..];
    }
    out.push_str(rest);
    out
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// API keys by their prefix, bot tokens (Telegram `<digits>:<secret>`,
/// Discord's three dotted parts) and JSON Web Tokens.
fn tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        if !is_token_byte(bytes[at]) || (at > 0 && is_token_byte(bytes[at - 1])) {
            at += 1;
            continue;
        }
        // A run of token bytes and dots, without the dots it ends with.
        let mut end = at;
        while end < bytes.len() && (is_token_byte(bytes[end]) || bytes[end] == b'.') {
            end += 1;
        }
        while end > at && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let word = &text[at..end];
        let secret = if is_prefixed(word) || is_dotted(word) {
            Some((at, end))
        } else {
            telegram_token(bytes, at, end)
        };
        match secret {
            Some((start, secret_end)) => {
                out.push_str(&text[copied..start]);
                out.push_str(MASK);
                copied = secret_end;
                at = secret_end;
            }
            None => at = end.max(at + 1),
        }
    }
    out.push_str(&text[copied..]);
    out
}

fn is_prefixed(word: &str) -> bool {
    PREFIXES.iter().any(|(prefix, min)| {
        word.starts_with(prefix)
            && word.len() >= *min
            && !word.contains('.')
            // A word of letters only after the prefix is prose ("sk-learn").
            && word[prefix.len()..].bytes().any(|b| b.is_ascii_digit())
    })
}

/// Three dot-separated base64url parts, long enough for a JSON Web Token or
/// a Discord bot token.
fn is_dotted(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3
        && parts[0].len() >= 16
        && parts[1].len() >= 6
        && parts[2].len() >= 20
        && word.bytes().any(|b| b.is_ascii_digit())
        && word.bytes().any(|b| b.is_ascii_uppercase())
}

/// A Telegram bot token at the end of the run `start..end` (`bot` may stand
/// before its digits, as in an API URL): where it starts and ends.
fn telegram_token(bytes: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    if end >= bytes.len() || bytes[end] != b':' {
        return None;
    }
    let digits = bytes[start..end]
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if !(6..=12).contains(&digits) {
        return None;
    }
    let mut secret = end + 1;
    while secret < bytes.len() && is_token_byte(bytes[secret]) {
        secret += 1;
    }
    (secret - end > 30).then_some((end - digits, secret))
}

/// The credential after `Bearer ` or `Basic `.
fn bearer(text: &str) -> String {
    let mut out = text.to_string();
    for scheme in ["Bearer ", "bearer ", "Basic ", "basic "] {
        let mut from = 0;
        while let Some(found) = out[from..].find(scheme) {
            let start = from + found + scheme.len();
            let len = out[start..]
                .bytes()
                .take_while(|b| is_token_byte(*b) || matches!(b, b'.' | b'=' | b'+' | b'/'))
                .count();
            if len >= 16 {
                out.replace_range(start..start + len, MASK);
                from = start + MASK.len();
            } else {
                from = start;
            }
        }
    }
    out
}

/// One line with the values of secret-like settings hidden: `NAME=value`,
/// `NAME: value`, `"name": "value"`.
fn assignments(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut ranges = Vec::new();
    for (at, byte) in bytes.iter().enumerate() {
        if *byte != b'=' && *byte != b':' {
            continue;
        }
        // `:` separates a setting only before a space or a quote (not
        // `https://`, `C:\`, `12:30`).
        if *byte == b':' && !matches!(bytes.get(at + 1), Some(b' ' | b'"' | b'\'')) {
            continue;
        }
        let mut name_end = at;
        while name_end > 0 && matches!(bytes[name_end - 1], b'"' | b'\'' | b' ') {
            name_end -= 1;
        }
        let mut name_start = name_end;
        while name_start > 0
            && (is_token_byte(bytes[name_start - 1]) || bytes[name_start - 1] == b'.')
        {
            name_start -= 1;
        }
        if !is_secret_name(&line[name_start..name_end]) {
            continue;
        }
        let mut value_start = at + 1;
        while value_start < bytes.len() && matches!(bytes[value_start], b' ' | b'"' | b'\'') {
            value_start += 1;
        }
        let mut value_end = value_start;
        while value_end < bytes.len()
            && !matches!(bytes[value_end], b' ' | b'"' | b'\'' | b',' | b';' | b'\t')
        {
            value_end += 1;
        }
        if looks_secret(&line[value_start..value_end]) {
            ranges.push((value_start, value_end));
        }
    }
    let mut out = line.to_string();
    for (start, end) in ranges.into_iter().rev() {
        out.replace_range(start..end, MASK);
    }
    out
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !name.is_empty()
        && (SECRET_NAMES.iter().any(|part| name.contains(part))
            || name.ends_with("key")
            || name.ends_with("keys"))
}

/// A value that may be a credential: long enough, one word with letters
/// and digits, not code (`config.key`, `$VAR`, `${…}`, `env(…)`), not
/// already hidden.
fn looks_secret(value: &str) -> bool {
    value.chars().count() >= 8
        && !value.contains(MASK)
        && !value.starts_with(['$', '<', '{', '[', '%', '*'])
        && !value.contains(['.', '(', ')'])
        && !value.to_ascii_lowercase().contains("env")
        && value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_alphabetic())
}

/// Words that name a credentials file or directory.
fn credential_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let lower = word.to_ascii_lowercase().replace('\\', "/");
        if CREDENTIAL_PATHS.iter().any(|path| lower.contains(path)) {
            out.push_str(PATH_MASK);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_whitespace() || "`'\"()[]<>,;，。、：（）".contains(c) {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(text: &str) -> String {
        redact(text, &[])
    }

    #[test]
    fn api_keys_and_tokens_are_hidden() {
        // Made-up samples, put together here so that secret scanners do
        // not take this file for one that leaks credentials.
        for secret in [
            concat!("sk-", "proj-4f9a8b7c6d5e4f3a2b1c0d9e8f7a"),
            concat!("ghp", "_1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f"),
            concat!("github", "_pat_11ABCDEFG0123456789_abcdefghijklmnop"),
            concat!("xoxb", "-123456789012-abcdefABCDEF"),
            "AKIAIOSFODNN7EXAMPLE",
            concat!("AIza", "SyD4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d"),
            concat!(
                "eyJhbGciOiJIUzI1NiJ9.",
                "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
                "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
            ),
            concat!(
                "MTA5ODc2NTQz",
                "MjEwOTg3NjU0.",
                "GaBcDe.",
                "f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2"
            ),
            concat!("123456789:", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1"),
        ] {
            let text = format!("用这个 {secret} 试试。");
            let out = hidden(&text);
            assert_eq!(out, format!("用这个 {MASK} 试试。"), "{secret}");
        }
        // In a URL, the bot token goes; the rest stays.
        assert_eq!(
            hidden(
                "https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1/getMe"
            ),
            format!("https://api.telegram.org/bot{MASK}/getMe")
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "运行 cargo test，3 个测试通过。",
            "see https://example.com/a/b and sk-learn",
            "version 1.2.3, time 12:30, C:\\Users\\me",
            "token: string; password: String",
            "let key = config.key;",
            "fn auth(user: &User) -> bool",
        ] {
            assert_eq!(hidden(text), text);
        }
    }

    #[test]
    fn secret_settings_lose_their_values() {
        assert_eq!(
            hidden("OPENAI_API_KEY=sk_live_9f8e7d6c5b4a"),
            format!("OPENAI_API_KEY={MASK}")
        );
        assert_eq!(
            hidden("  \"client_secret\": \"Zx9-qQ81-aBcD\","),
            format!("  \"client_secret\": \"{MASK}\",")
        );
        assert_eq!(
            hidden("DB_PASSWORD: hunter2Hunter2"),
            format!("DB_PASSWORD: {MASK}")
        );
        assert_eq!(
            hidden("Authorization: Bearer abcdefABCDEF0123456789"),
            format!("Authorization: Bearer {MASK}")
        );
        // A reference to a secret is not one.
        assert_eq!(
            hidden("API_KEY=${OPENAI_API_KEY}"),
            "API_KEY=${OPENAI_API_KEY}"
        );
    }

    #[test]
    fn private_keys_and_credential_paths_are_hidden() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        assert_eq!(hidden(pem), format!("before\n{MASK}\nafter"));
        let public = "-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----";
        assert_eq!(hidden(public), public);
        assert_eq!(
            hidden("读了 ~/.ssh/id_ed25519 和 /home/a/.lynshen/auth.json。"),
            format!("读了 {PATH_MASK} 和 {PATH_MASK}。")
        );
        assert_eq!(hidden("C:\\Users\\a\\.aws\\credentials"), PATH_MASK);
    }

    #[test]
    fn known_secrets_are_hidden_wherever_they_are() {
        let token = "999:short-but-known-token".to_string();
        assert_eq!(redact(&format!("x{token}y"), &[token]), format!("x{MASK}y"));
    }
}
