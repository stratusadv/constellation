use constellation_extraction::{Extractor, TemplateExtractor};
use constellation_graph::{EdgeKind, ProjectId};

#[test]
fn emits_member_access_ref_for_variable_attribute() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    // A var.attr access, a numeric list index, and a bare variable: only the
    // first names a model member.
    let source = "<div>{{ record.available_quantity|default:'N/A' }}</div>\n\
                  <span>{{ row.0 }}</span>\n\
                  <p>{{ plain }}</p>\n";

    let output = extractor.extract(&project, "templates/inventory/row.html", source);

    let accesses: Vec<(&str, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::AccessesMember)
        .map(|reference| (reference.reference_name.as_str(), reference.candidates.first().map(String::as_str)))
        .collect();

    assert!(
        accesses.contains(&("available_quantity", Some("record"))),
        "var.attr emits (attr, [var]), filters stripped, got {accesses:?}",
    );

    assert_eq!(
        accesses.len(),
        1,
        "a numeric index (row.0) and a bare variable name no member, got {accesses:?}",
    );
}

#[test]
fn emits_loop_binding_for_for_tag() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    let source = "{% for record in records %}{{ record.color }}{% endfor %}\n\
                  {% for k, v in items %}{{ v }}{% endfor %}\n\
                  {% for line in order.lines %}{{ line }}{% endfor %}\n";

    let output = extractor.extract(&project, "templates/t.html", source);

    let loops: Vec<(&str, Option<&str>, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::LoopBinding)
        .map(|reference| {
            (
                reference.reference_name.as_str(),
                reference.candidates.first().map(String::as_str),
                reference.candidates.get(1).map(String::as_str),
            )
        })
        .collect();

    assert_eq!(
        loops,
        vec![("records", Some("record"), None), ("order", Some("line"), Some("lines"))],
        "a bare source binds (source, [var], None); `obj.accessor` binds (obj, [var], accessor); a \
         tuple target is skipped, got {loops:?}",
    );
}

#[test]
fn emits_member_access_ref_for_glue_model_field() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    let source = "{% include 'django_glue/form/field/char_field.html' with glue_model_field='inventory.estimate_cost' %}\n\
                  {% include 'card.html' with title='hi' %}\n";

    let output = extractor.extract(&project, "templates/inventory/form/form.html", source);

    let access: Vec<(&str, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::AccessesMember)
        .map(|reference| (reference.reference_name.as_str(), reference.candidates.first().map(String::as_str)))
        .collect();

    assert_eq!(
        access,
        vec![("estimate_cost", Some("inventory"))],
        "a django-glue `glue_model_field='name.field'` binding emits (field, [glue_name]); a plain \
         `title='hi'` binding emits nothing, got {access:?}",
    );
}

#[test]
fn emits_member_access_ref_for_glue_js_field() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    // The rewrite reads model fields in JS via `Glue.<kind>.<name>.<field>`,
    // inside Alpine attribute values.
    let source = "<div x-text=\"Glue.model.task.title\"></div>\n\
                  <input :value=\"Glue.form.contact.email\">\n\
                  <span x-text=\"other.value\"></span>\n";

    let output = extractor.extract(&project, "templates/t.html", source);

    let access: Vec<(&str, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::AccessesMember)
        .map(|reference| (reference.reference_name.as_str(), reference.candidates.first().map(String::as_str)))
        .collect();

    assert!(
        access.contains(&("title", Some("task"))),
        "Glue.model.task.title emits (field, [glue_name]), got {access:?}",
    );

    assert!(
        access.contains(&("email", Some("contact"))),
        "Glue.form.contact.email (a form proxy) emits (field, [glue_name]), got {access:?}",
    );

    assert!(
        !access.iter().any(|(_, name)| *name == Some("other")),
        "a non-glue attribute expression (other.value) emits nothing, got {access:?}",
    );
}
