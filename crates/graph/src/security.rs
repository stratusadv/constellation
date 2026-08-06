/// The name fragments that mark a symbol as security-sensitive.
///
/// Kept tight and tunable on purpose: a list broad enough to match everything
/// scores everything alike, which is the same as scoring nothing. Measure any
/// addition against the eval harness before keeping it. Shared, because the
/// changed-symbol risk score and the flow criticality score must agree on what
/// counts as sensitive.
pub const SECURITY_KEYWORDS: &[&str] = &[
    "auth",
    "billing",
    "credential",
    "csrf",
    "decrypt",
    "encrypt",
    "escape",
    "invoice",
    "login",
    "logout",
    "passwd",
    "password",
    "payment",
    "permission",
    "pickle",
    "raw_sql",
    "sanitize",
    "secret",
    "session",
    "signature",
    "subprocess",
    "superuser",
    "token",
];

/// The [`SECURITY_KEYWORDS`] entry a symbol's name or qualified name contains,
/// or `None` when it matches none. Matching the qualified name is deliberate: a
/// helper inside an `auth/` module is security-adjacent even when its own name
/// is neutral, because the qualified name carries the file path.
pub fn security_keyword(name: &str, qualified_name: &str) -> Option<&'static str> {
    let name_lower = name.to_ascii_lowercase();
    let qualified_lower = qualified_name.to_ascii_lowercase();

    SECURITY_KEYWORDS
        .iter()
        .copied()
        .find(|keyword| name_lower.contains(keyword) || qualified_lower.contains(keyword))
}

/// Whether a symbol's name or qualified name matches any [`SECURITY_KEYWORDS`]
/// entry.
pub fn is_security_sensitive(name: &str, qualified_name: &str) -> bool {
    security_keyword(name, qualified_name).is_some()
}

#[cfg(test)]
mod tests {
    use super::{SECURITY_KEYWORDS, is_security_sensitive, security_keyword};

    #[test]
    fn the_keyword_list_is_sorted_and_unique() {
        let mut sorted = SECURITY_KEYWORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(sorted.as_slice(), SECURITY_KEYWORDS, "the list stays sorted and duplicate-free");
    }

    #[test]
    fn a_sensitive_name_reports_the_keyword_it_matched() {
        assert_eq!(security_keyword("verify_password", "a.py::verify_password"), Some("password"));
        assert_eq!(security_keyword("Rotate", "auth/keys.py::Rotate"), Some("auth"));
        assert_eq!(security_keyword("format_label", "text/labels.py::format_label"), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_security_sensitive("VerifyPassword", "a.py::VerifyPassword"));
        assert!(is_security_sensitive("CSRF_EXEMPT", "a.py::CSRF_EXEMPT"));
    }
}
