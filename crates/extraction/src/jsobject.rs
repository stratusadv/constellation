//! The Alpine `x-data` object-literal reader: the members of a component's
//! object value and the bindable calls each method body makes. The expression
//! parse itself (wrapping, tag blanking, the shared parser) lives in
//! [`crate::jsexpr`]; this module only walks the parsed object.

use tree_sitter::Node as TsNode;

use crate::jsexpr::{
    CHILDREN_MAX, WALK_ITERATIONS_MAX, callee_name, is_js_builtin_constructor, is_js_non_handler,
    string_literal,
};
use crate::tsutil::{node_text, to_u32};

/// The receiver shape of one call an Alpine `x-data` method body makes, which
/// decides what the call can bind to: a bare call reaches shared scripts, a
/// `this.` call only its own component, and a `this.<property>.` call the class
/// the property was constructed from.
#[derive(PartialEq)]
pub(crate) enum AlpineReceiver {
    /// A bare call (`save()`).
    Bare,
    /// A call received through `this` (`this.save()`).
    This,
    /// A call received through a `this` data property (`this.rows.filter()`),
    /// carrying the property name.
    Property(String),
}

/// One call an Alpine `x-data` method's body makes that can bind to project
/// code, with the receiver shape that decides where it may bind.
pub(crate) struct AlpineCall {
    /// The called name.
    pub(crate) name: String,
    /// The 1-based line of the call.
    pub(crate) line: u32,
    /// The receiver the call is made through.
    pub(crate) receiver: AlpineReceiver,
}

/// One method of an Alpine `x-data` object literal: its name, the 1-based line
/// of its name, and the calls its body makes.
pub(crate) struct AlpineMethod {
    /// The method name.
    pub(crate) name: String,
    /// The 1-based line the method's name sits on.
    pub(crate) line: u32,
    /// The bindable calls the method's body makes.
    pub(crate) calls: Vec<AlpineCall>,
}

/// The members an Alpine `x-data` object literal defines: its methods and its
/// `new`-initialized data properties.
pub(crate) struct AlpineObject {
    /// The object's methods, with the calls each body makes.
    pub(crate) methods: Vec<AlpineMethod>,
    /// The `(property, class)` data properties initialized with a `new`
    /// expression (`rows: new QuerySetGlue('rows')` -> `("rows",
    /// "QuerySetGlue")`), the component state whose class types a
    /// `this.<property>.<method>()` call.
    pub(crate) typed_properties: Vec<(String, String)>,
}

impl AlpineObject {
    /// The empty component: no methods, no typed properties.
    pub(crate) fn empty() -> Self {
        Self { methods: Vec::new(), typed_properties: Vec::new() }
    }
}

/// The first `object` node in a tree, depth-first: the outermost object of a
/// wrapped `({ ... })` value.
pub(crate) fn first_object(root: TsNode<'_>) -> Option<TsNode<'_>> {
    let mut stack: Vec<TsNode> = vec![root];
    let mut iterations: u32 = 0;

    while let Some(node) = stack.pop() {
        iterations += 1;

        assert!(iterations <= WALK_ITERATIONS_MAX, "object search exceeded {WALK_ITERATIONS_MAX}");

        if node.kind() == "object" {
            return Some(node);
        }

        let mut cursor = node.walk();
        let mut count: u32 = 0;

        for child in node.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "object-search child fan-out exceeded {CHILDREN_MAX}");

            stack.push(child);
        }
    }

    None
}

/// The method a direct child of an object literal defines, with its body's
/// bindable calls: a `method_definition` (`save() {}`) or a `pair` whose value
/// is a function or arrow (`save: () => {}`). `None` for a non-method child.
pub(crate) fn method_member(
    bytes: &[u8],
    child: TsNode<'_>,
    base_line: u32,
) -> Option<AlpineMethod> {
    let (name, name_node) = method_name(bytes, child)?;

    let line = base_line.saturating_add(to_u32(name_node.start_position().row));

    Some(AlpineMethod {
        name: name.to_string(),
        line,
        calls: body_calls(bytes, child, base_line),
    })
}

/// The name and name-node of a direct child of an object literal that defines a
/// method. `None` for a non-method child.
fn method_name<'bytes, 'tree>(
    bytes: &'bytes [u8],
    child: TsNode<'tree>,
) -> Option<(&'bytes str, TsNode<'tree>)> {
    match child.kind() {
        "method_definition" => {
            let name_node = child.child_by_field_name("name")?;

            Some((node_text(bytes, name_node), name_node))
        }
        "pair" => {
            let value = child.child_by_field_name("value")?;

            let is_function = matches!(
                value.kind(),
                "function" | "function_expression" | "arrow_function" | "generator_function",
            );

            if !is_function {
                return None;
            }

            let name_node = child.child_by_field_name("key")?;

            let name = match name_node.kind() {
                "property_identifier" => node_text(bytes, name_node),
                "string" => string_literal(bytes, name_node)?,
                _ => return None,
            };

            if name.is_empty() {
                return None;
            }

            Some((name, name_node))
        }
        _ => None,
    }
}

/// The `(property, class)` a direct child of an object literal binds when it is
/// a data property initialized with a `new` expression
/// (`rows: new QuerySetGlue('rows')`). `None` for any other child, or a builtin
/// constructor, which types nothing a project defines.
pub(crate) fn typed_property(bytes: &[u8], child: TsNode<'_>) -> Option<(String, String)> {
    if child.kind() != "pair" {
        return None;
    }

    let value = child.child_by_field_name("value")?;

    if value.kind() != "new_expression" {
        return None;
    }

    let class = callee_name(bytes, value.child_by_field_name("constructor")?)?;

    if class.is_empty() || is_js_builtin_constructor(class) {
        return None;
    }

    let name_node = child.child_by_field_name("key")?;

    if name_node.kind() != "property_identifier" {
        return None;
    }

    let name = node_text(bytes, name_node);

    if name.is_empty() {
        return None;
    }

    Some((name.to_string(), class.to_string()))
}

/// The bindable calls under one object member's subtree, each once with the
/// line of its first call: a bare `save()`, a same-component `this.save()`,
/// and a data-property `this.rows.filter()`. Any deeper receiver
/// (`this.a.b.save()`) is skipped, because the bare name alone cannot say what
/// it binds to.
fn body_calls(bytes: &[u8], member: TsNode<'_>, base_line: u32) -> Vec<AlpineCall> {
    let mut calls: Vec<AlpineCall> = Vec::new();
    let mut stack: Vec<TsNode> = vec![member];
    let mut iterations: u32 = 0;

    while let Some(node) = stack.pop() {
        iterations += 1;

        assert!(iterations <= WALK_ITERATIONS_MAX, "body walk exceeded {WALK_ITERATIONS_MAX}");

        if node.kind() == "call_expression"
            && let Some((name, receiver)) = bindable_callee(bytes, node)
        {
            let line = base_line.saturating_add(to_u32(node.start_position().row));

            let seen = calls
                .iter_mut()
                .find(|call| call.name == name && call.receiver == receiver);

            // The walk visits nodes in reverse source order, so an already-seen
            // call keeps the smallest line rather than the first visited.
            match seen {
                Some(call) => call.line = call.line.min(line),
                None => calls.push(AlpineCall { name: name.to_string(), line, receiver }),
            }
        }

        let mut cursor = node.walk();
        let mut count: u32 = 0;

        for child in node.named_children(&mut cursor) {
            count += 1;

            assert!(count <= CHILDREN_MAX, "body child fan-out exceeded {CHILDREN_MAX}");

            stack.push(child);
        }
    }

    calls.sort_by_key(|call| call.line);

    calls
}

/// The callee name of a call an `x-data` method body can bind, and the receiver
/// shape it is made through: a bare identifier (`save()`), a `this.` method
/// (`this.save()`), or a `this.` data property's method (`this.rows.filter()`).
/// `None` for a builtin, an Alpine magic, or a deeper receiver, whose bare name
/// proves nothing.
fn bindable_callee<'bytes>(
    bytes: &'bytes [u8],
    call: TsNode<'_>,
) -> Option<(&'bytes str, AlpineReceiver)> {
    let function = call.child_by_field_name("function")?;

    let (name, receiver) = match function.kind() {
        "identifier" => (node_text(bytes, function), AlpineReceiver::Bare),
        "member_expression" => {
            let object = function.child_by_field_name("object")?;
            let name = node_text(bytes, function.child_by_field_name("property")?);

            (name, member_receiver(bytes, object)?)
        }
        _ => return None,
    };

    if name.is_empty() || is_js_non_handler(name) {
        return None;
    }

    Some((name, receiver))
}

/// The receiver a member call is made through: `this` itself, or a
/// `this.<property>` access. `None` for anything else (a bare object, a deeper
/// chain), which types nothing.
fn member_receiver(bytes: &[u8], object: TsNode<'_>) -> Option<AlpineReceiver> {
    if object.kind() == "this" {
        return Some(AlpineReceiver::This);
    }

    if object.kind() != "member_expression" {
        return None;
    }

    let owner = object.child_by_field_name("object")?;

    if owner.kind() != "this" {
        return None;
    }

    let property = object.child_by_field_name("property")?;

    if property.kind() != "property_identifier" {
        return None;
    }

    let name = node_text(bytes, property);

    if name.is_empty() {
        return None;
    }

    Some(AlpineReceiver::Property(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::AlpineReceiver;
    use crate::jsexpr::AlpineExpr;

    /// The `(name, line)` pairs of an `x-data` object's methods, the member
    /// half of `x_data_object` the method tests below assert on.
    fn object_methods(value: &str, base_line: u32) -> Vec<(String, u32)> {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        expressions
            .x_data_object(value, base_line)
            .methods
            .into_iter()
            .map(|method| (method.name, method.line))
            .collect()
    }

    #[test]
    fn object_methods_finds_shorthand_and_function_valued_properties() {
        assert_eq!(
            object_methods("{ save() {}, count: 0, load: () => {} }", 1),
            vec![("save".to_string(), 1), ("load".to_string(), 1)],
            "method shorthand and arrow-valued properties are methods; a data field is not",
        );
    }

    #[test]
    fn object_methods_offsets_each_method_line_by_the_base() {
        assert_eq!(
            object_methods("{\n  alpha() {},\n  beta() {}\n}", 10),
            vec![("alpha".to_string(), 11), ("beta".to_string(), 12)],
            "each method's line is the base line plus its row within the value",
        );
    }

    #[test]
    fn object_methods_ignores_a_non_object_value() {
        assert!(
            object_methods("save()", 1).is_empty(),
            "an expression that is not an object literal has no methods",
        );
    }

    #[test]
    fn x_data_object_reads_bare_this_and_property_calls_from_a_body() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        let value = "{\n  async init() {\n    await this.refresh_fields()\n    load()\n    \
                     this.rows.filter()\n    this.a.b.save()\n  },\n  refresh_fields() {}\n}";

        let component = expressions.x_data_object(value, 10);

        assert_eq!(component.methods.len(), 2, "both members are methods");

        let init = &component.methods[0];

        assert_eq!(init.name, "init", "the first method is init");

        let described: Vec<String> = init
            .calls
            .iter()
            .map(|call| match &call.receiver {
                AlpineReceiver::Bare => call.name.clone(),
                AlpineReceiver::This => format!("this.{}", call.name),
                AlpineReceiver::Property(property) => format!("this.{property}.{}", call.name),
            })
            .collect();

        assert_eq!(
            described,
            vec!["this.refresh_fields", "load", "this.rows.filter"],
            "a this. call, a bare call, and a property call are read; a deeper receiver \
             (this.a.b.save) is not",
        );

        assert!(component.methods[1].calls.is_empty(), "an empty body makes no calls");
    }

    #[test]
    fn x_data_object_reads_new_initialized_properties() {
        let mut expressions = AlpineExpr::new().expect("the javascript grammar loads");

        let value = "{\n  rows: new QuerySetGlue('rows'),\n  when: new Date(),\n  \
                     count: 0,\n  save() {}\n}";

        assert_eq!(
            expressions.x_data_object(value, 1).typed_properties,
            vec![("rows".to_string(), "QuerySetGlue".to_string())],
            "a new-initialized property yields (property, class); a builtin constructor and a \
             plain field yield nothing",
        );
    }
}
