use constellation_extraction::{CssExtractor, ExtractionOutput, Extractor};
use constellation_graph::{EdgeKind, Language, NodeKind, ProjectId};

fn run(source: &str) -> ExtractionOutput {
    let extractor = CssExtractor::new();
    let project = ProjectId::new("shop");

    extractor.extract(&project, "shop/static/styles.css", source)
}

fn selectors(output: &ExtractionOutput) -> Vec<String> {
    let mut names: Vec<String> = output
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Selector)
        .map(|node| node.name.clone())
        .collect();

    names.sort();
    names
}

#[test]
fn extracts_class_and_id_selectors() {
    let output = run(".card { color: red; }\n#header { margin: 0; }\n");

    assert_eq!(selectors(&output), vec!["card".to_string(), "header".to_string()]);
    assert!(output.nodes.iter().any(|node| node.kind == NodeKind::File), "a file node is emitted");
}

#[test]
fn selector_strips_the_sigil_from_its_name_but_keeps_it_in_the_qualified_name() {
    let output = run(".card { color: red; }\n");

    let card = output
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::Selector && node.name == "card")
        .expect("a card selector node");

    assert_eq!(card.qualified_name, "shop/static/styles.css::.card", "the qualified name keeps the class sigil");
    assert_eq!(card.language, Language::Css, "a selector carries the css language");
}

#[test]
fn the_file_contains_each_selector() {
    let output = run(".alpha { color: red; }\n#beta { margin: 0; }\n");

    let file = output.nodes.iter().find(|node| node.kind == NodeKind::File).expect("a file node");

    for name in ["alpha", "beta"] {
        let selector = output
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Selector && node.name == name)
            .unwrap_or_else(|| panic!("a {name} selector"));

        assert!(
            output.edges.iter().any(|edge| {
                edge.kind == EdgeKind::Contains && edge.source == file.id && edge.target == selector.id
            }),
            "the file contains the {name} selector",
        );
    }
}

#[test]
fn compound_and_descendant_selectors_each_yield_a_selector() {
    let output = run(".btn.btn-primary a#main .nav { color: red; }\n");
    let names = selectors(&output);

    for expected in ["btn", "btn-primary", "main", "nav"] {
        assert!(names.contains(&expected.to_string()), "{expected} is its own selector, got {names:?}");
    }
}

#[test]
fn pseudo_classes_are_not_separate_selectors() {
    let output = run(".card:hover { color: blue; }\n");
    let names = selectors(&output);

    assert!(names.contains(&"card".to_string()), "the class is extracted, got {names:?}");
    assert!(!names.contains(&"hover".to_string()), "the pseudo-class is not a selector, got {names:?}");
}

#[test]
fn selectors_nested_in_a_media_query_are_found() {
    let output = run("@media (max-width: 600px) {\n  .responsive { display: none; }\n}\n");

    assert!(
        selectors(&output).contains(&"responsive".to_string()),
        "a selector nested inside @media is reached by the walk",
    );
}

#[test]
fn a_repeated_selector_is_deduplicated_per_file() {
    let output = run(".card { color: red; }\n.card { color: blue; }\n");

    assert_eq!(selectors(&output), vec!["card".to_string()], "the repeated selector yields a single node");
}

#[test]
fn element_and_universal_selectors_are_ignored() {
    let output = run("div { margin: 0; }\nbody * { padding: 0; }\n");

    assert!(
        selectors(&output).is_empty(),
        "only class and id selectors become nodes, got {:?}",
        selectors(&output),
    );

    assert!(output.nodes.iter().any(|node| node.kind == NodeKind::File), "the file node is still present");
}

#[test]
fn malformed_css_does_not_panic() {
    let output = run(".ok { color }\n} stray {{{ #partial");

    assert!(
        output.nodes.iter().any(|node| node.kind == NodeKind::File),
        "even malformed css yields a file node and does not panic",
    );
}
