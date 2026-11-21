use anyhow::{bail, Result};

pub(crate) fn is_allowed(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')')
}

pub(crate) fn normalize(name: &str) -> Result<&str> {
    let name = name.trim_matches(' ');
    if name.is_empty() {
        bail!("Name is required");
    }
    if !name.chars().all(is_allowed) {
        bail!("Use only English letters, digits, spaces, and . - _ ( )");
    }
    Ok(name)
}

pub(crate) fn normalize_optional(name: &str) -> Result<&str> {
    if name.trim_matches(' ').is_empty() {
        Ok("")
    } else {
        normalize(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_display_name_and_rejects_other_characters() {
        assert_eq!(
            normalize("  US-West_01  (v2.1)  ").unwrap(),
            "US-West_01  (v2.1)"
        );
        assert_eq!(normalize("-_.()").unwrap(), "-_.()");
        assert!(normalize("   ").is_err());
        assert_eq!(normalize_optional("   ").unwrap(), "");
        for invalid in [
            "A:B",
            "A/B",
            "A\\B",
            "A\n",
            "\tA",
            "A\0",
            "A\u{00a0}B",
            "\u{4e2d}1",
        ] {
            assert!(normalize_optional(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
