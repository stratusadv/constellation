//! Django template syntax tree. The node set models Django's template language
//! faithfully, but every node borrows from the source: the tree is built,
//! walked once to collect the cross-template references constellation links, and
//! dropped within a single extraction pass, so owning the strings would only
//! add allocations.
//!
//! Structural tags carry their semantics as typed variants (`{% block %}`,
//! `{% if %}`, `{% for %}`, `{% with %}`, `{% autoescape %}`, `{% filter %}`,
//! `{% spaceless %}`) alongside the link- and text-bearing leaves the graph
//! consumes (`extends`/`include`/`url`/`static`/`load`). Every other tag is
//! preserved generically so nesting stays correct without a variant per tag:
//! [`AstNode::Container`] for any other block-with-end tag, [`AstNode::Tag`]
//! for any other leaf tag.

// This is a complete Django template syntax tree. constellation currently
// reads only the link- and text-bearing fields (`extends`/`include`/`url`
// paths, lines, bodies), but the structural fields (an `if` condition, a
// `for` target, `with` bindings) are modeled so the tree is faithful and ready
// for future edges. Those not-yet-read fields are intentionally retained.
#![allow(dead_code)]

/// A `name=value` binding from `{% include ... with x=y %}` or `{% with %}`.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding<'source> {
    pub name: &'source str,
    pub value: &'source str,
}

/// A single condition arm of an `{% if %}` / `{% elif %}` chain.
#[derive(Clone, Debug)]
pub struct IfArm<'source> {
    pub condition: &'source str,
    pub body: Vec<AstNode<'source>>,
}

/// A node in a parsed Django template.
#[derive(Clone, Debug)]
pub enum AstNode<'source> {
    /// The literal text between template constructs (including raw HTML).
    Text(&'source str),

    /// A variable expression: `{{ article.title|upper }}`. Holds the trimmed
    /// inner expression with its filters still attached, and the 1-based line it
    /// begins on, for the member-access reference a `var.attr` access emits.
    Variable { expression: &'source str, line: u32 },

    /// A `{# ... #}` comment's inner text.
    Comment(&'source str),

    /// An `{% extends "base.html" %}` node. `is_literal` is true when the path is a
    /// quoted string rather than a variable.
    Extends {
        path: &'source str,
        is_literal: bool,
        line: u32,
    },

    /// An `{% include "card.html" with x=y only %}` node.
    Include {
        path: &'source str,
        is_literal: bool,
        bindings: Vec<Binding<'source>>,
        only: bool,
        line: u32,
    },

    /// A `{% url "article-detail" article.pk as link %}` node. `name` is the route
    /// name, `as_variable` the optional capture target.
    Url {
        name: &'source str,
        is_literal: bool,
        as_variable: Option<&'source str>,
        line: u32,
    },

    /// A `{% static "css/site.css" %}` node.
    Static {
        path: &'source str,
        is_literal: bool,
        as_variable: Option<&'source str>,
        line: u32,
    },

    /// A `{% load static i18n %}` node.
    Load { libraries: Vec<&'source str> },

    /// A `{% block content %}...{% endblock %}` node.
    Block {
        name: &'source str,
        body: Vec<AstNode<'source>>,
    },

    /// An `{% if %}...{% elif %}...{% else %}...{% endif %}` node. `arms` always holds
    /// the leading `if` arm first, then any `elif` arms.
    If {
        arms: Vec<IfArm<'source>>,
        else_body: Option<Vec<AstNode<'source>>>,
    },

    /// A `{% for x in xs %}...{% empty %}...{% endfor %}` node.
    For {
        variable: &'source str,
        iterable: &'source str,
        body: Vec<AstNode<'source>>,
        empty_body: Option<Vec<AstNode<'source>>>,
    },

    /// A `{% with x=y %}...{% endwith %}` node.
    With {
        bindings: Vec<Binding<'source>>,
        body: Vec<AstNode<'source>>,
    },

    /// An `{% autoescape on %}...{% endautoescape %}` node.
    Autoescape {
        argument: &'source str,
        body: Vec<AstNode<'source>>,
    },

    /// A `{% filter upper %}...{% endfilter %}` node.
    Filter {
        specification: &'source str,
        body: Vec<AstNode<'source>>,
    },

    /// A `{% spaceless %}...{% endspaceless %}` node.
    Spaceless { body: Vec<AstNode<'source>> },

    /// A `{% verbatim %}...{% endverbatim %}` node. The inner text is unparsed; the
    /// lexer hands it back as a single literal.
    Verbatim { content: &'source str },

    /// A block-with-end tag not modeled above (`{% cache %}`,
    /// `{% blocktranslate %}`, `{% language %}`, ...). Its body is kept so
    /// nested links inside it remain reachable.
    Container {
        name: &'source str,
        arguments: &'source str,
        body: Vec<AstNode<'source>>,
    },

    /// A leaf tag not modeled above (`{% csrf_token %}`, `{% now %}`,
    /// `{% cycle %}`, ...). `arguments` is the raw text after the tag word.
    Tag {
        name: &'source str,
        arguments: &'source str,
    },
}

impl<'source> AstNode<'source> {
    /// The push of the direct child node slices onto a traversal stack, covering
    /// every variant that holds a nested body. Lets callers walk the tree with
    /// an explicit stack instead of recursion.
    pub fn push_child_slices<'tree>(&'tree self, stack: &mut Vec<&'tree [AstNode<'source>]>) {
        match self {
            AstNode::Block { body, .. } => stack.push(body),
            AstNode::If { arms, else_body } => {
                for arm in arms {
                    stack.push(&arm.body);
                }

                if let Some(body) = else_body {
                    stack.push(body);
                }
            }
            AstNode::For { body, empty_body, .. } => {
                stack.push(body);

                if let Some(body) = empty_body {
                    stack.push(body);
                }
            }
            AstNode::With { body, .. } => stack.push(body),
            AstNode::Autoescape { body, .. } => stack.push(body),
            AstNode::Filter { body, .. } => stack.push(body),
            AstNode::Spaceless { body } => stack.push(body),
            AstNode::Container { body, .. } => stack.push(body),
            AstNode::Text(_)
            | AstNode::Variable { .. }
            | AstNode::Comment(_)
            | AstNode::Extends { .. }
            | AstNode::Include { .. }
            | AstNode::Url { .. }
            | AstNode::Static { .. }
            | AstNode::Load { .. }
            | AstNode::Verbatim { .. }
            | AstNode::Tag { .. } => {}
        }
    }
}
