use constellation_extraction::{Extractor, TemplateExtractor};
use constellation_graph::{EdgeKind, Language, ProjectId};
use constellation_resolution::{STORE_DISPATCH, TYPED_RECEIVER};

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
fn emits_uses_tag_for_custom_tags_and_filters_only() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    let source = "{% load my_tags %}\n\
                  {% if record %}{{ record.total|money }}{% endif %}\n\
                  {% quick_filter_button label='x' %}\n\
                  {{ value|truncatewords:30 }}\n";

    let output = extractor.extract(&project, "templates/inventory/card.html", source);

    let tags: Vec<&str> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::UsesTag)
        .map(|reference| reference.reference_name.as_str())
        .collect();

    assert!(tags.contains(&"quick_filter_button"), "a custom tag emits UsesTag, got {tags:?}");
    assert!(tags.contains(&"money"), "a custom filter emits UsesTag, got {tags:?}");
    assert!(!tags.contains(&"if"), "the builtin tag if must not emit, got {tags:?}");
    assert!(!tags.contains(&"load"), "the builtin tag load must not emit, got {tags:?}");
    assert!(!tags.contains(&"truncatewords"), "the builtin filter truncatewords must not emit, got {tags:?}");
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

/// The `(field, glue_name)` member accesses a template yields, the shape every
/// django-glue test below asserts on.
fn glue_accesses(source: &str) -> Vec<(String, Option<String>)> {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("blog");

    let output = extractor.extract(&project, "templates/t.html", source);

    output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::AccessesMember)
        .map(|reference| (reference.reference_name.clone(), reference.candidates.first().cloned()))
        .collect()
}

#[test]
fn emits_member_access_ref_for_either_versions_field_map() {
    // 0.x spells the field map `glue_fields`, 1.x spells it `$fields`; both are
    // installed across these projects, so both read.
    let inline = "{ init() { this.sales_order.glue_fields.ship_to.required = false } }";

    let source = format!(
        "<div x-text=\"record.glue_fields.quantity\"></div>\n\
         <div x-text=\"gorilla.$fields.name\"></div>\n\
         <div x-text=\"Glue.model.task.$fields.title.label\"></div>\n\
         <div x-data=\"{inline}\"></div>\n\
         <span x-text=\"other.plain.value\"></span>\n"
    );

    let access = glue_accesses(&source);
    let has = |field: &str, name: &str| {
        access.iter().any(|(f, n)| f == field && n.as_deref() == Some(name))
    };

    assert!(has("quantity", "record"), "0.x record.glue_fields.quantity binds, got {access:?}");
    assert!(has("name", "gorilla"), "1.x gorilla.$fields.name binds, got {access:?}");

    assert!(
        has("title", "task"),
        "1.x metadata off a proxy (Glue.model.task.$fields.title) names the field, got {access:?}",
    );

    assert!(
        has("ship_to", "sales_order"),
        "an x-data method reaching through `this.` still names the glue object, got {access:?}",
    );

    assert!(
        !access.iter().any(|(_, name)| name.as_deref() == Some("plain")),
        "a member access with no field map is not a glue access, got {access:?}",
    );
}

#[test]
fn emits_instantiates_refs_for_alpine_new_expressions() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("portal");

    let source = "<div x-data=\"{\n\
                      contract: new ModelObjectGlue('contract'),\n\
                      rows: new QuerySetGlue('rows'),\n\
                      when: new Date(),\n\
                  }\"></div>\n";

    let output = extractor.extract(&project, "templates/contract/form_modal.html", source);

    let instantiated: Vec<(&str, u32)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Instantiates)
        .map(|reference| (reference.reference_name.as_str(), reference.line))
        .collect();

    assert_eq!(
        instantiated,
        vec![("ModelObjectGlue", 2), ("QuerySetGlue", 3)],
        "each project class an x-data property instantiates emits one Instantiates reference at \
         its line; a builtin (Date) emits none, got {instantiated:?}",
    );
}

/// The extraction of a template whose `x-data` component calls a sibling
/// method through `this`, a shared script, and a member no sibling defines,
/// the output both body-call tests below assert on.
fn x_data_body_call_output() -> constellation_extraction::ExtractionOutput {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("portal");

    let source = "<div x-data=\"{\n\
                      async init() {\n\
                          await this.refresh_fields()\n\
                          toggleSpinner()\n\
                          this.missing()\n\
                      },\n\
                      refresh_fields() {},\n\
                  }\"></div>\n";

    extractor.extract(&project, "templates/sample/form_modal.html", source)
}

#[test]
fn emits_a_direct_edge_for_a_this_call_to_a_sibling_method() {
    let output = x_data_body_call_output();

    let sibling_calls: Vec<(&str, &str)> = output
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .map(|edge| (edge.source.as_str(), edge.target.as_str()))
        .collect();

    assert_eq!(
        sibling_calls.len(),
        1,
        "only the this. call to a sibling method becomes a direct edge, got {sibling_calls:?}",
    );

    let (source_id, target_id) = sibling_calls[0];

    assert!(
        source_id.ends_with("form_modal.html::alpine::init"),
        "the call is attributed to the calling method's own node, got {source_id}",
    );

    assert!(
        target_id.ends_with("form_modal.html::alpine::refresh_fields"),
        "the this. call binds to the sibling method's node, got {target_id}",
    );
}

#[test]
fn emits_a_javascript_call_ref_for_a_bare_call_in_an_x_data_body() {
    let output = x_data_body_call_output();

    let bare_calls: Vec<(&str, &str, Language)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Calls)
        .map(|reference| {
            (
                reference.from_node_id.as_str(),
                reference.reference_name.as_str(),
                reference.language,
            )
        })
        .collect();

    assert_eq!(
        bare_calls.len(),
        1,
        "a bare call leaves one reference; a this. call to no sibling none, got {bare_calls:?}",
    );

    let (from_id, callee, language) = bare_calls[0];

    assert!(
        from_id.ends_with("::alpine::init"),
        "the bare call comes from the method node, got {from_id}",
    );

    assert_eq!(callee, "toggleSpinner", "and names the called function");

    assert_eq!(
        language,
        Language::JavaScript,
        "a body call is JavaScript, so script-global scoping applies",
    );
}

#[test]
fn emits_a_typed_receiver_call_ref_for_a_property_call() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("portal");

    let source = "<div x-data=\"{\n\
                      rows: new QuerySetGlue('rows'),\n\
                      untyped: [],\n\
                      async refresh() {\n\
                          this.choices = await this.rows.to_choices()\n\
                          this.untyped.push(1)\n\
                      },\n\
                  }\"></div>\n";

    let output = extractor.extract(&project, "templates/sample/form_modal.html", source);

    let typed_calls: Vec<(&str, Option<&str>, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Calls)
        .map(|reference| {
            (
                reference.reference_name.as_str(),
                reference.candidates.first().map(String::as_str),
                reference.candidates.get(1).map(String::as_str),
            )
        })
        .collect();

    assert_eq!(
        typed_calls,
        vec![("to_choices", Some(TYPED_RECEIVER), Some("QuerySetGlue"))],
        "a call through a new-initialized property emits one typed-receiver reference carrying \
         the property's class; a call through an untyped property emits nothing, got \
         {typed_calls:?}",
    );
}

#[test]
fn emits_a_store_dispatch_handles_ref_for_store_calls() {
    let extractor = TemplateExtractor::new();
    let project = ProjectId::new("portal");

    let source = "<div x-init=\"$store.theme.families().then(f => families = f)\"></div>\n";

    let output = extractor.extract(&project, "templates/theme/selector.html", source);

    let stores: Vec<(&str, Option<&str>, Option<&str>)> = output
        .unresolved_refs
        .iter()
        .filter(|reference| reference.reference_kind == EdgeKind::Handles)
        .map(|reference| {
            (
                reference.reference_name.as_str(),
                reference.candidates.first().map(String::as_str),
                reference.candidates.get(1).map(String::as_str),
            )
        })
        .collect();

    assert_eq!(
        stores,
        vec![("families", Some(STORE_DISPATCH), Some("theme"))],
        "a $store.<name>.<member>() call emits one Handles reference carrying the store-dispatch \
         sentinel and the store name, got {stores:?}",
    );
}

#[test]
fn emits_member_access_ref_for_a_field_map_include_binding() {
    // django-spire (1.x) binds through the field map in the include argument
    // itself; django-glue's own `glue_field_value_path` filter drops the same
    // segment before the frontend reads it.
    let source = "{% include 'f.html' with glue_field='glue_form.$fields.purpose' %}\n\
                  {% include 'f.html' with glue_field='gorilla.name' %}\n\
                  {% include 'f.html' with glue_model_field='inventory.estimate_cost' %}\n\
                  {% include 'f.html' with glue_field='search_field' %}\n\
                  {% include 'f.html' with title='hi' %}\n";

    let access = glue_accesses(source);

    assert!(
        access.contains(&("purpose".to_string(), Some("glue_form".to_string()))),
        "1.x glue_field='glue_form.$fields.purpose' binds past the field map, got {access:?}",
    );

    assert!(
        access.contains(&("name".to_string(), Some("gorilla".to_string()))),
        "1.x glue_field='gorilla.name' binds, got {access:?}",
    );

    assert!(
        access.contains(&("estimate_cost".to_string(), Some("inventory".to_string()))),
        "0.x glue_model_field still binds, got {access:?}",
    );

    assert_eq!(access.len(), 3, "a bare name and a non-glue binding emit nothing, got {access:?}");
}
