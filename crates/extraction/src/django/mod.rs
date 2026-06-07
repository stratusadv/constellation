//! A small, proper Django template front end: a borrowing lexer feeding an
//! iterative parser that yields a nesting syntax tree. It replaces the ad-hoc
//! byte scanning the template extractor used to do, giving the graph a real
//! AST to walk for `{% extends %}` / `{% include %}` / `{% url %}` links.

mod ast;
mod lexer;
mod parser;

pub use ast::AstNode;

use lexer::Lexer;

/// The top-level nodes parsed from one template's source. The returned tree
/// borrows from `source`.
pub fn parse(source: &str) -> Vec<AstNode<'_>> {
    let tokens = Lexer::new(source).tokenize();

    parser::parse(&tokens)
}

#[cfg(test)]
mod tests {
    use super::lexer::TokenKind;
    use super::*;

    #[test]
    fn lexes_text_variable_and_tags_with_lines() {
        let source = "<h1>{{ title }}</h1>\n{% extends 'base.html' %}\n{# note #}";
        let tokens = Lexer::new(source).tokenize();

        assert!(matches!(tokens[0].kind, TokenKind::Text("<h1>")));

        assert!(
            matches!(tokens[1].kind, TokenKind::Variable { expression } if expression == "title"),
        );

        assert!(
            matches!(tokens[3].kind, TokenKind::BlockStart { tag, arguments }
                if tag == "extends" && arguments == "'base.html'"),
        );

        let extends = tokens.iter().find(|token| {
            matches!(token.kind, TokenKind::BlockStart { tag, .. } if tag == "extends")
        });

        assert_eq!(extends.map(|token| token.line), Some(2), "extends sits on line 2");
    }

    #[test]
    fn parses_extends_include_and_url() {
        let source =
            "{% extends 'base.html' %}\n{% include 'page/_card.html' %}\n<a href=\"{% url 'home' %}\">x</a>";
        let nodes = parse(source);

        let extends = nodes.iter().any(|node| {
            matches!(node, AstNode::Extends { path, is_literal, line } if *path == "base.html" && *is_literal && *line == 1)
        });
        let include = nodes.iter().any(|node| {
            matches!(node, AstNode::Include { path, line, .. } if *path == "page/_card.html" && *line == 2)
        });

        assert!(extends, "extends parsed with literal path on line 1");
        assert!(include, "include parsed on line 2");
    }

    #[test]
    fn nests_block_if_for_bodies() {
        let source = "{% block body %}{% for item in items %}{% if item %}{{ item }}{% else %}{% include 'empty.html' %}{% endif %}{% empty %}none{% endfor %}{% endblock %}";
        let nodes = parse(source);

        assert_eq!(nodes.len(), 1, "a single top-level block");

        let mut includes = 0;
        let mut stack: Vec<&[AstNode]> = vec![&nodes];
        let mut guard = 0;

        while let Some(slice) = stack.pop() {
            guard += 1;

            assert!(guard < 1_000, "walk terminates");

            for node in slice {
                if matches!(node, AstNode::Include { path, .. } if *path == "empty.html") {
                    includes += 1;
                }

                node.push_child_slices(&mut stack);
            }
        }

        assert_eq!(includes, 1, "the include nested in if/for/block is reachable");
    }

    #[test]
    fn url_captures_route_name_and_as_variable() {
        let nodes = parse("{% url 'article-detail' article.pk as link %}");

        let captured = nodes.iter().any(|node| {
            matches!(node, AstNode::Url { name, as_variable, .. }
                if *name == "article-detail" && *as_variable == Some("link"))
        });

        assert!(captured, "url route name and `as` capture both parsed");
    }

    #[test]
    fn verbatim_body_is_not_parsed_as_tags() {
        let nodes = parse("{% verbatim %}{% extends 'x' %}{% endverbatim %}");

        let has_extends = nodes.iter().any(|node| matches!(node, AstNode::Extends { .. }));
        let has_verbatim = nodes.iter().any(|node| matches!(node, AstNode::Verbatim { .. }));

        assert!(!has_extends, "tags inside verbatim are literal");
        assert!(has_verbatim, "verbatim body is preserved");
    }

    #[test]
    fn unterminated_tag_degrades_to_text() {
        let nodes = parse("before {% extends 'x'");

        assert!(!nodes.is_empty(), "lexing never fails on a half-written tag");

        assert!(
            nodes.iter().all(|node| !matches!(node, AstNode::Extends { .. })),
            "an unterminated tag yields no extends node",
        );
    }
}
