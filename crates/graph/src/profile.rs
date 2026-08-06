//! The profile boundary: where constellation names a company convention, and
//! the only place it does.
//!
//! Everything else in this tree is generic Python and Django. A profile gathers
//! what is not: the framework hook names a company's own base classes add, and
//! the companion packages its projects install. The conventions are data, so
//! they live here rather than being scattered through the layers that read them,
//! and a workspace selects a profile in `.constellation/config.toml`.
//!
//! `graph` depends on nothing and is reachable from every layer, which is what
//! makes it the one legal home for a default the whole pipeline shares.

/// The method and class names Django itself invokes with no static call site:
/// lifecycle hooks, protocol methods, and app configuration entry points. Every
/// profile carries these; a profile's own [`Profile::hook_names_extra`] adds to
/// them rather than replacing them.
pub const DJANGO_HOOK_NAMES: &[&str] = &[
    "clean",
    "delete",
    "get_absolute_url",
    "handle",
    "Meta",
    "ready",
    "save",
];

/// The name of the built-in profile a workspace gets when its config selects
/// none. Making constellation generic for another company is a change to this
/// one constant, or a config file that names a different profile.
pub const PROFILE_NAME_DEFAULT: &str = "stratus";

/// The names [`Profile::named`] recognizes, for the message a bad config value
/// gets.
pub const PROFILE_NAMES: &[&str] = &["generic", "stratus"];

/// A named set of conventions layered over generic Django: the framework hook
/// names a company's base classes add, and the companion packages its projects
/// install alongside their own code.
///
/// Every field is additive and inert when empty, so [`Profile::generic`] is
/// constellation with no company in it at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    /// The companion packages discovery looks for, by project id (hyphenated).
    pub companion_packages: Vec<String>,
    /// The git repository each companion's history is read from, as
    /// (package, url) pairs.
    pub companion_repositories: Vec<(String, String)>,
    /// The hook names this profile adds to [`DJANGO_HOOK_NAMES`].
    pub hook_names_extra: Vec<String>,
}

impl Profile {
    /// The built-in profile named `name`, or `None` when no built-in carries
    /// that name. The recognized names are [`PROFILE_NAMES`].
    pub fn named(name: &str) -> Option<Self> {
        assert!(!name.is_empty(), "a profile name must not be empty");

        let profile = match name {
            "generic" => Self::generic(),
            "stratus" => Self::stratus(),
            _ => return None,
        };

        assert!(
            PROFILE_NAMES.contains(&name),
            "a resolved profile name must be listed in PROFILE_NAMES: {name:?}",
        );

        Some(profile)
    }

    /// The profile that adds nothing to generic Django: no extra hook names and
    /// no companion packages. What constellation is outside a company codebase.
    pub fn generic() -> Self {
        Self {
            companion_packages: Vec::new(),
            companion_repositories: Vec::new(),
            hook_names_extra: Vec::new(),
        }
    }

    /// The stratusadv profile: the `django_spire` breadcrumb hooks, and the four
    /// company packages a portal installs, with the repositories their history is
    /// read from.
    ///
    /// This function is the whole company surface of constellation's data. A
    /// workspace that selects [`Profile::generic`] gets none of it.
    ///
    /// The package list is derived from the repository list rather than written
    /// twice, so the two cannot come to disagree about which packages exist.
    pub fn stratus() -> Self {
        let repositories = [
            ("django-spire", "https://github.com/stratusadv/django-spire"),
            ("django-glue", "https://github.com/stratusadv/django-glue"),
            ("robit", "https://github.com/stratusadv/robit"),
            ("dandy", "https://github.com/stratusadv/dandy"),
        ];

        let hook_names = [
            "base_breadcrumb",
            "breadcrumbs",
        ];

        let profile = Self {
            companion_packages: repositories
                .iter()
                .map(|(package, _url)| (*package).to_string())
                .collect(),
            companion_repositories: repositories
                .iter()
                .map(|(package, url)| ((*package).to_string(), (*url).to_string()))
                .collect(),
            hook_names_extra: hook_names.iter().map(|name| (*name).to_string()).collect(),
        };

        assert!(!profile.companion_packages.is_empty(), "the stratus profile names packages");

        assert_eq!(
            profile.companion_packages.len(),
            profile.companion_repositories.len(),
            "every stratus companion has a repository",
        );

        profile
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self::named(PROFILE_NAME_DEFAULT).expect("the default profile name names a built-in")
    }
}

#[cfg(test)]
mod tests {
    use super::{DJANGO_HOOK_NAMES, PROFILE_NAME_DEFAULT, PROFILE_NAMES, Profile};

    #[test]
    fn the_generic_profile_carries_no_company_surface() {
        let profile = Profile::generic();

        assert!(profile.companion_packages.is_empty(), "no companions");
        assert!(profile.companion_repositories.is_empty(), "no repositories");
        assert!(profile.hook_names_extra.is_empty(), "no extra hook names");
    }

    #[test]
    fn the_stratus_profile_names_the_company_packages_and_hooks() {
        let profile = Profile::stratus();

        assert_eq!(profile.companion_packages.len(), 4, "four company packages");

        assert!(
            profile.companion_packages.contains(&"django-spire".to_string()),
            "django-spire is a companion",
        );

        assert!(
            profile.hook_names_extra.contains(&"breadcrumbs".to_string()),
            "the breadcrumb hooks are profile extras, not Django's own",
        );

        for (_package, url) in &profile.companion_repositories {
            assert!(url.starts_with("https://"), "a repository is an https url, got {url:?}");
        }
    }

    #[test]
    fn the_django_hook_names_hold_no_company_name() {
        assert!(
            !DJANGO_HOOK_NAMES.contains(&"breadcrumbs"),
            "a company hook name belongs to a profile, not to the Django set",
        );

        assert!(DJANGO_HOOK_NAMES.contains(&"save"), "the lifecycle hooks are Django's own");
    }

    /// The drift [`Profile::named`]'s own assertion cannot catch: a name listed
    /// in `PROFILE_NAMES` that no match arm answers resolves to `None` silently,
    /// so a config naming it would fall back to the default with a message that
    /// calls the name unknown while the list advertises it.
    #[test]
    fn every_listed_profile_name_resolves() {
        for name in PROFILE_NAMES {
            assert!(Profile::named(name).is_some(), "{name:?} is listed but does not resolve");
        }
    }

    #[test]
    fn the_default_profile_resolves_and_unknown_names_do_not() {
        assert_eq!(Profile::default(), Profile::named(PROFILE_NAME_DEFAULT).unwrap());
        assert_eq!(Profile::named("generic"), Some(Profile::generic()));
        assert_eq!(Profile::named("nonesuch"), None, "an unknown name resolves to nothing");
    }
}
