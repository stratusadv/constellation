/// A language constellation parses. The set is deliberately closed to the
/// target stack: Python plus the front-end surface a Django project renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Css,
    HtmlDjango,
    JavaScript,
    Python,
}

impl Language {
    /// The lowercase label for this language.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Css => "css",
            Language::HtmlDjango => "htmldjango",
            Language::JavaScript => "javascript",
            Language::Python => "python",
        }
    }

    /// The language a file extension (without the leading dot) maps to.
    pub fn from_extension(extension: &str) -> Option<Language> {
        debug_assert!(
            !extension.starts_with('.'),
            "pass the extension without its leading dot",
        );

        let language = match extension {
            "css" => Language::Css,
            "htm" | "html" => Language::HtmlDjango,
            "js" | "mjs" => Language::JavaScript,
            "py" | "pyi" => Language::Python,
            _ => return None,
        };

        Some(language)
    }

    /// The language parsed from its lowercase label, or `None` if unknown.
    pub fn from_str_label(label: &str) -> Option<Language> {
        let language = match label {
            "css" => Language::Css,
            "htmldjango" => Language::HtmlDjango,
            "javascript" => Language::JavaScript,
            "python" => Language::Python,
            _ => return None,
        };

        Some(language)
    }
}
