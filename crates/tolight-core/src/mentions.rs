use crate::models::ApiError;
use std::collections::BTreeSet;

pub fn valid_agent_id(value: &str) -> bool {
    (3..=32).contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
pub fn validate_explicit(mentions: &[String]) -> Result<(), ApiError> {
    if mentions.len() > 16 || mentions.iter().any(|id| !valid_agent_id(id)) {
        return Err(ApiError::new("bad_request").with("field", "mentions"));
    }
    Ok(())
}
pub fn resolve(
    text: &str,
    explicit: &[String],
    sender: &str,
    active: &[String],
    max: usize,
) -> Result<Vec<String>, ApiError> {
    resolve_with_reply(text, explicit, sender, active, None, max)
}

/// A valid reply addresses its author through the same bounded notification set.
/// The caller resolves the target and ACTIVE snapshot while holding the room lock.
pub fn resolve_with_reply(
    text: &str,
    explicit: &[String],
    sender: &str,
    active: &[String],
    reply_author: Option<&str>,
    max: usize,
) -> Result<Vec<String>, ApiError> {
    validate_explicit(explicit)?;
    let active: BTreeSet<_> = active.iter().map(String::as_str).collect();
    let mut result = BTreeSet::new();
    for id in explicit {
        if !active.contains(id.as_str()) {
            return Err(ApiError::new("invalid_mention").with("agent_id", id.clone()));
        }
        if id != sender {
            result.insert(id.clone());
        }
    }
    let chars: Vec<_> = text.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        if *c != '@'
            || (index > 0 && (chars[index - 1].is_ascii_alphanumeric() || chars[index - 1] == '-'))
        {
            continue;
        }
        let id: String = chars[index + 1..]
            .iter()
            .copied()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .collect();
        if active.contains(id.as_str()) && id != sender {
            result.insert(id);
        }
    }
    if let Some(author) = reply_author
        && author != sender
        && active.contains(author)
    {
        result.insert(author.to_owned());
    }
    if result.len() > max {
        return Err(ApiError::new("too_many_mentions").with("max", max));
    }
    Ok(result.into_iter().collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ids() -> Vec<String> {
        ["alice", "alice-2", "bob", "carol", "david"]
            .map(String::from)
            .into()
    }
    #[test]
    fn mentions_boundaries_and_longest_token() {
        let result = resolve(
            "@alice，谢谢@bob！(@alice-2) bob@alice.com Bob@alice @Alice @nobody",
            &[],
            "carol",
            &ids(),
            3,
        )
        .unwrap();
        assert_eq!(result, ["alice", "alice-2", "bob"]);
        assert!(
            resolve("@alice-2z", &[], "carol", &ids(), 3)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn mentions_union_self_and_failures() {
        assert_eq!(
            resolve(
                "@bob @alice @alice",
                &["bob".into(), "alice-2".into()],
                "alice",
                &ids(),
                3
            )
            .unwrap(),
            ["alice-2", "bob"]
        );
        assert_eq!(
            resolve("", &["missing".into()], "alice", &ids(), 3)
                .unwrap_err()
                .code,
            "invalid_mention"
        );
        assert_eq!(
            resolve("@alice @bob @carol @david", &[], "alice-2", &ids(), 3)
                .unwrap_err()
                .code,
            "too_many_mentions"
        );
        for value in [
            vec!["x".into()],
            vec!["alice\n".into()],
            vec!["alice".into(); 17],
            vec!["a".repeat(33)],
        ] {
            assert_eq!(validate_explicit(&value).unwrap_err().code, "bad_request");
        }
    }

    #[test]
    fn replies_notify_active_authors_and_share_the_deduplicated_limit() {
        assert_eq!(
            resolve_with_reply("回应正文", &[], "alice", &ids(), Some("bob"), 3).unwrap(),
            ["bob"]
        );
        assert_eq!(
            resolve_with_reply(
                "@bob @carol",
                &["bob".into()],
                "alice",
                &ids(),
                Some("bob"),
                2,
            )
            .unwrap(),
            ["bob", "carol"]
        );
        let overflow =
            resolve_with_reply("@carol @david", &[], "alice", &ids(), Some("bob"), 2).unwrap_err();
        assert_eq!(overflow.code, "too_many_mentions");
        assert_eq!(overflow.fields["max"], 2);
        for author in [None, Some("alice"), Some("departed")] {
            assert!(
                resolve_with_reply("正文", &[], "alice", &ids(), author, 3)
                    .unwrap()
                    .is_empty()
            );
        }
        // Explicit departed targets retain their existing rejection semantics.
        assert_eq!(
            resolve_with_reply(
                "正文",
                &["departed".into()],
                "alice",
                &ids(),
                Some("bob"),
                3,
            )
            .unwrap_err()
            .code,
            "invalid_mention"
        );
    }
}
