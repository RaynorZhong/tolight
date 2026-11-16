//! The ordered public-text gate. Rejected text is never returned with an error.
use regex::Regex;
use std::sync::LazyLock;

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:",
        r"(?:sk-(?:proj-)?|gh[pousr]_|github_pat_|xox[baprs]-)[A-Za-z0-9_-]{16,}",
        r"|AKIA[A-Z0-9]{16}",
        r"|eyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
        r"|-----BEGIN[ ](?:[A-Z]+[ ])*PRIVATE[ ]KEY-----",
        r")"
    ))
    .expect("constant secret-shape regex")
});

pub fn sanitize(text: &str, max_scalars: usize) -> Result<String, String> {
    let text = text.replace("\r\n", "\n");
    if text.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{e0000}'..='\u{e007f}')) {
        return Err("invalid_chars".into());
    }
    if text.trim().is_empty() {
        return Err("empty".into());
    }
    if text.chars().count() > max_scalars {
        return Err("too_long".into());
    }
    if SECRET.is_match(&text) {
        return Err("secret_shape".into());
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizer_order_and_scalars() {
        assert_eq!(
            sanitize(" \r\n你好\t世界\n ", 2000).unwrap(),
            " \n你好\t世界\n "
        );
        assert_eq!(sanitize("\n\t ", 2000).unwrap_err(), "empty");
        assert!(sanitize(&"🦀".repeat(2000), 2000).is_ok());
        assert_eq!(sanitize(&"🦀".repeat(2001), 2000).unwrap_err(), "too_long");
        assert_eq!(sanitize("sk-abcdefghijklmnop", 4).unwrap_err(), "too_long");
        assert_eq!(
            sanitize(&format!("a{}", " ".repeat(2000)), 2000).unwrap_err(),
            "too_long"
        );
    }
    #[test]
    fn sanitizer_characters_and_literal_markup() {
        for value in [
            "<script>alert(1)</script>",
            "<img onerror='x'>",
            "**<b>hi</b>**",
            "方向\u{200e}\u{200f}\u{061c}",
            "a\nb\tc",
        ] {
            assert!(sanitize(value, 2000).is_ok(), "{value:?}");
        }
        for c in [
            '\0',
            '\r',
            '\u{1}',
            '\u{7f}',
            '\u{85}',
            '\u{202a}',
            '\u{202e}',
            '\u{2066}',
            '\u{2069}',
            '\u{e0041}',
        ] {
            assert_eq!(
                sanitize(&format!("a{c}b"), 2000).unwrap_err(),
                "invalid_chars"
            );
        }
    }
    #[test]
    fn sanitizer_credentials_but_not_public_identifiers() {
        for value in [
            "sk-abcdefghijklmnop123456",
            "ghp_abcdefghijklmnopqrstuvwxyz",
            "AKIA1234567890ABCDEF",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.ABCDEFGH",
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
        ] {
            assert_eq!(
                sanitize(value, 2000).unwrap_err(),
                "secret_shape",
                "{value}"
            );
        }
        for value in [
            "user@example.org",
            &"a".repeat(64),
            "SHA256: 47DEQpj8HBSa",
            "非秘密的长期计划",
        ] {
            assert!(sanitize(value, 2000).is_ok());
        }
    }
}
