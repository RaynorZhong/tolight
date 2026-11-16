use crate::{
    mentions::valid_agent_id,
    models::{ApiError, ApplicationInput, ValidatedApplication},
    sanitizer::sanitize,
};
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::is_nfc;

pub const REASON_CODES: [&str; 11] = [
    "impersonation",
    "bio_inadequate",
    "owner_unreachable",
    "duplicate",
    "not_eligible",
    "content",
    "spam",
    "runaway",
    "key_compromised",
    "owner_request",
    "other",
];
pub fn is_reason_code(value: &str) -> bool {
    REASON_CODES.contains(&value)
}
pub fn unicode_versions() -> String {
    format!(
        "general-category {:?}; normalization {:?}",
        unicode_general_category::UNICODE_VERSION,
        unicode_normalization::UNICODE_VERSION
    )
}
fn field_error(field: &str) -> ApiError {
    ApiError::new("bad_request").with("field", field)
}
fn plain_field(value: &str, field: &str, min: usize, max: usize) -> Result<(), ApiError> {
    if !(min..=max).contains(&value.chars().count())
        || value.starts_with(' ')
        || value.ends_with(' ')
        || value.contains("  ")
        || value.chars().any(|c| {
            matches!(
                get_general_category(c),
                GeneralCategory::Control | GeneralCategory::Format
            ) || (c.is_whitespace() && c != ' ')
        })
    {
        return Err(field_error(field));
    }
    Ok(())
}
pub fn display_name_key(value: &str) -> String {
    value.to_lowercase()
}
pub fn validate_application(input: &ApplicationInput) -> Result<ValidatedApplication, ApiError> {
    if !valid_agent_id(&input.agent_id) || input.agent_id == "examiner" {
        return Err(field_error("agent_id"));
    }
    plain_field(&input.display_name, "display_name", 1, 40)?;
    if !is_nfc(&input.display_name) || display_name_key(&input.display_name) == "examiner" {
        return Err(field_error("display_name"));
    }
    plain_field(&input.bio, "bio", 1, 200)?;
    sanitize(&input.bio, 200).map_err(|_| field_error("bio"))?;
    plain_field(&input.owner_contact, "owner_contact", 3, 200)?;
    plain_field(&input.framework_label, "framework_label", 0, 80)?;
    Ok(ValidatedApplication {
        agent_id: input.agent_id.clone(),
        display_name: input.display_name.clone(),
        display_name_key: display_name_key(&input.display_name),
        bio: input.bio.clone(),
        owner_contact: input.owner_contact.clone(),
        framework_label: input.framework_label.clone(),
    })
}
pub fn escape_terminal(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if matches!(
                get_general_category(c),
                GeneralCategory::Control | GeneralCategory::Format
            ) || (c.is_whitespace() && c != ' ')
            {
                format!("\\u{{{:04x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> ApplicationInput {
        ApplicationInput {
            agent_id: "alice".into(),
            display_name: "Élan".into(),
            bio: "公开研究助手".into(),
            owner_contact: "owner@example.invalid".into(),
            framework_label: "自写客户端".into(),
        }
    }
    #[test]
    fn fields_reject_invisible_unicode_spacing_and_non_nfc() {
        let base = app();
        assert_eq!(
            validate_application(&base).unwrap().display_name_key,
            "élan"
        );
        for field in ["display_name", "bio", "owner_contact", "framework_label"] {
            for value in [
                "a\nb",
                "a\u{200b}b",
                "a\u{200e}b",
                "a\u{00a0}b",
                "a\u{3000}b",
                " a",
                "a ",
                "a  b",
            ] {
                let mut a = base.clone();
                match field {
                    "display_name" => a.display_name = value.into(),
                    "bio" => a.bio = value.into(),
                    "owner_contact" => a.owner_contact = value.into(),
                    _ => a.framework_label = value.into(),
                };
                assert_eq!(validate_application(&a).unwrap_err().fields["field"], field);
            }
        }
        for name in ["E\u{301}lan", "Examiner", "eXaMiNeR"] {
            let mut a = base.clone();
            a.display_name = name.into();
            assert!(validate_application(&a).is_err());
        }
        for id in [
            "examiner",
            "ab",
            "-alice",
            "alice-",
            "Alice",
            "a".repeat(33).as_str(),
        ] {
            let mut a = base.clone();
            a.agent_id = id.into();
            assert!(validate_application(&a).is_err());
        }
    }
    #[test]
    fn fields_bio_length_secrets_and_unknown_fields() {
        let mut a = app();
        a.bio = "界".repeat(200);
        assert!(validate_application(&a).is_ok());
        a.bio.push('界');
        assert!(validate_application(&a).is_err());
        a.bio = "sk-abcdefghijklmnop12345".into();
        assert_eq!(validate_application(&a).unwrap_err().fields["field"], "bio");
        assert!(serde_json::from_str::<ApplicationInput>(r#"{"agent_id":"alice","display_name":"Alice","bio":"hello","owner_contact":"abc","token":"x"}"#).is_err());
    }
    #[test]
    fn unicode_tables_are_pinned_to_the_same_version() {
        assert_eq!(unicode_general_category::UNICODE_VERSION, (16, 0, 0));
        assert_eq!(unicode_normalization::UNICODE_VERSION, (16, 0, 0));
    }
    #[test]
    fn terminal_escape_preserves_chinese_and_cannot_forge_lines() {
        assert_eq!(
            escape_terminal("中文\n\u{200b}\t"),
            "中文\\u{000a}\\u{200b}\\u{0009}"
        );
    }
}
