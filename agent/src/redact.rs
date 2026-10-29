//! Hides what looks like a secret in text that leaves the agent for Wren:
//! the log of a step is shown to every role there.
//!
//! The rules are patterns, so this is best effort: a secret that is printed
//! on its own line, without a name in front of it, is not recognised.
//!
//! * the value of a `name=value`, `name: value` or `"name": "value"` pair
//!   whose name holds a secret word (`password`, `token`, `api_key`, ...);
//! * a `Bearer` or `Basic` credential;
//! * the password in `scheme://user:password@host`;
//! * a PEM private key block, whole.

/// What replaces a secret.
const MASK: &str = "***";

/// Words that make the name of a pair a secret.
const SECRET_WORDS: [&str; 11] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "credential",
    "private_key",
    "access_key",
];

/// `text` with its secrets hidden (see the module docs).
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_key = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        if in_key {
            if body.contains("-----END") {
                in_key = false;
            }
            continue;
        }
        if body.contains("-----BEGIN") && body.contains("PRIVATE KEY") {
            in_key = !body.contains("-----END");
            out.push_str("[private key hidden]");
            out.push_str(ending);
            continue;
        }
        let line = hide_url_passwords(body);
        let line = hide_bearer(&line);
        out.push_str(&hide_pairs(&line));
        out.push_str(ending);
    }
    out
}

/// `scheme://user:password@host` becomes `scheme://user:***@host`.
fn hide_url_passwords(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(i) if authority[..i].contains(':') => {
                let user = authority[..i].split(':').next().unwrap_or("");
                out.push_str(user);
                out.push(':');
                out.push_str(MASK);
                out.push_str(&authority[i..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `Bearer abc` and `Basic abc` (any case) become `Bearer ***`.
fn hide_bearer(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        let hit = ["bearer ", "basic "]
            .iter()
            .filter_map(|w| lower[at..].find(w).map(|i| (at + i, w.len())))
            .min();
        let Some((start, len)) = hit else { break };
        let value_start = start + len;
        let value_end = line[value_start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ','))
            .map_or(line.len(), |i| value_start + i);
        out.push_str(&line[at..value_start]);
        if value_end > value_start {
            out.push_str(MASK);
        }
        at = value_end;
    }
    out.push_str(&line[at..]);
    out
}

/// The value of every pair whose name holds a secret word.
fn hide_pairs(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if (c == b'=' || c == b':') && !line[i + 1..].starts_with("//") {
            // The name: letters, digits and `_ - .` just before the sign,
            // past a closing quote and spaces.
            let mut end = i;
            while end > copied && matches!(bytes[end - 1], b' ' | b'\t' | b'"' | b'\'') {
                end -= 1;
            }
            let mut start = end;
            while start > copied
                && (bytes[start - 1].is_ascii_alphanumeric()
                    || matches!(bytes[start - 1], b'_' | b'-' | b'.'))
            {
                start -= 1;
            }
            let name = line[start..end].to_ascii_lowercase();
            if !name.is_empty() && SECRET_WORDS.iter().any(|w| name.contains(w)) {
                let mut v = i + 1;
                while v < bytes.len() && matches!(bytes[v], b' ' | b'\t') {
                    v += 1;
                }
                let quote = bytes.get(v).copied().filter(|b| matches!(b, b'"' | b'\''));
                let begin = v + usize::from(quote.is_some());
                let stop = match quote {
                    Some(q) => line[begin..].find(q as char),
                    None => line[begin..].find(|ch: char| {
                        ch.is_whitespace() || matches!(ch, ',' | ';' | '&' | '"' | '\'' | '}' | ')')
                    }),
                }
                .map_or(line.len(), |s| begin + s);
                if stop > begin {
                    out.push_str(&line[copied..begin]);
                    out.push_str(MASK);
                    copied = stop;
                    i = stop;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&line[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_of_secret_names_are_hidden() {
        for (text, want) in [
            ("DB_PASSWORD=hunter2", "DB_PASSWORD=***"),
            ("password: hunter2 and more", "password: *** and more"),
            (
                r#"{"api_key": "abc123", "name": "x"}"#,
                r#"{"api_key": "***", "name": "x"}"#,
            ),
            ("--token=abc&user=bob", "--token=***&user=bob"),
            (
                "Authorization: Bearer abc.def-ghi",
                "Authorization: Bearer ***",
            ),
            (
                "authorization: Basic dXNlcjpwdw==",
                "authorization: Basic ***",
            ),
            (
                "pull https://bob:s3cr3t@registry.example/x",
                "pull https://bob:***@registry.example/x",
            ),
            ("CLIENT_SECRET='a b'", "CLIENT_SECRET='***'"),
        ] {
            assert_eq!(redact(text), want, "{text}");
        }
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "pulling image nginx:1.27 done at 12:30:15",
            "https://example.com/path?a=1",
            "https://user@example.com/",
            "the author said: hello",
            "Container wren-test Started",
            "",
        ] {
            assert_eq!(redact(text), text, "{text:?}");
        }
        assert_eq!(redact("line one\nline two\r\n"), "line one\nline two\r\n");
    }

    #[test]
    fn a_private_key_block_is_hidden_whole() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        assert_eq!(redact(text), "before\n[private key hidden]\nafter\n");
    }

    #[test]
    fn a_secret_is_hidden_wherever_the_line_is_cut() {
        // Redaction runs on the whole text, before the end is cut off for
        // the log, so a cut can't leave half a name and its value.
        let long = format!("{}\nDB_PASSWORD=hunter2\n", "x".repeat(50));
        let shown = crate::snapshot::tail(&redact(&long), 24);
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}
