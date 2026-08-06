//! The class hierarchy an inheritance or receiver walk reads.
//!
//! Built once per link from the whole constellation's classes, methods, bases,
//! callables, and return types, then borrowed by every lookup, so no pass
//! rebuilds it. Every lookup here answers `None` at any ambiguity rather than
//! pick: a name several classes claim, a callee defined twice, a diamond whose
//! branches disagree. A dropped edge is recoverable; a confidently wrong one is
//! not.

use constellation_resolution::{MANAGER_SUFFIXES, RETURNS_OF};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::limits::OVERRIDE_WALK_MAX;
use crate::synthesize::overrides::method_owner_id;

/// The class hierarchy and callable pool an inheritance or receiver walk reads,
/// built once per link and borrowed by every lookup so no pass rebuilds it.
pub(crate) struct ClassIndex<'graph> {
    /// The base class ids of each subclass id.
    pub(crate) bases: FxHashMap<&'graph str, Vec<&'graph str>>,
    /// The class ids for each class simple name.
    pub(crate) by_name: FxHashMap<&'graph str, Vec<&'graph str>>,
    /// The method id for each (owning class id, method name) pair.
    pub(crate) by_owner: FxHashMap<(&'graph str, &'graph str), &'graph str>,
    /// The class id for each class qualified name.
    pub(crate) by_qualified: FxHashMap<&'graph str, &'graph str>,
    /// The (id, defining file path) for each callable simple name.
    pub(crate) callables: FxHashMap<&'graph str, Vec<(&'graph str, &'graph str)>>,
    /// The id of every class, which is [`ClassIndex::by_qualified`]'s values
    /// again as a set: a typed receiver tests one id against it per reference,
    /// and scanning the map for each was the pass's one quadratic term.
    pub(crate) class_ids: FxHashSet<&'graph str>,
    /// The model each (owning model id, field name) pair relates to.
    pub(crate) field_types: FxHashMap<(&'graph str, &'graph str), &'graph str>,
    /// The node each callable's annotated return type names, by callable id.
    pub(crate) returns: FxHashMap<&'graph str, &'graph str>,
}

/// The id of the sole class named `name`, or `None` when the constellation holds
/// none or several: an ambiguous class name types no receiver.
pub(crate) fn sole_class_named<'graph>(
    name: &str,
    graph: &ClassIndex<'graph>,
) -> Option<&'graph str> {
    let classes = graph.by_name.get(name)?;

    match classes.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// The sole method named `name` reachable from the companion classes a naming
/// convention gives `model`: its querysets and managers under [`MANAGER_SUFFIXES`],
/// or its services under [`constellation_resolution::SERVICE_SUFFIXES`]. A class's
/// own definition wins over an inherited one; the walk reaches a base in another
/// project, which is how a call to a method the upstream base service or queryset
/// defines binds at all.
/// `None` when the model names no such class, or when two of them reach different
/// definitions.
pub(crate) fn sole_companion_method<'graph>(
    model: &str,
    name: &str,
    suffixes: &[&str],
    graph: &ClassIndex<'graph>,
) -> Option<&'graph str> {
    assert!(!model.is_empty(), "a dispatch reference names a model");
    assert!(!suffixes.is_empty(), "at least one companion suffix");

    let (bases, by_owner, by_name) = (&graph.bases, &graph.by_owner, &graph.by_name);

    let mut owner_name = String::new();
    let mut found: Option<&'graph str> = None;

    for suffix in suffixes {
        owner_name.clear();
        owner_name.push_str(model);
        owner_name.push_str(suffix);

        for owner in by_name.get(owner_name.as_str()).into_iter().flatten() {
            let target = by_owner
                .get(&(*owner, name))
                .copied()
                .or_else(|| sole_inherited_method(owner, name, bases, by_owner));

            match (target, found) {
                (Some(target), None) => found = Some(target),
                (Some(target), Some(previous)) if target != previous => return None,
                _ => {}
            }
        }
    }

    found
}

/// The id of the method named `name` on the shallowest ancestor depth of `class`
/// that defines it. `None` when no ancestor defines it, or when that depth holds
/// more than one definition: an ambiguous diamond binds to neither branch rather
/// than to whichever the walk reached first. A visited set and a hard hop bound
/// make a cyclic hierarchy terminate.
pub(crate) fn sole_inherited_method<'graph>(
    class: &str,
    name: &str,
    bases: &FxHashMap<&'graph str, Vec<&'graph str>>,
    by_owner: &FxHashMap<(&'graph str, &'graph str), &'graph str>,
) -> Option<&'graph str> {
    let mut level: Vec<&'graph str> = bases.get(class).cloned()?;
    let mut visited: FxHashSet<&str> = FxHashSet::default();
    let mut next: Vec<&'graph str> = Vec::new();
    let mut hops: u32 = 0;

    visited.insert(class);

    while !level.is_empty() {
        hops += 1;

        assert!(hops <= OVERRIDE_WALK_MAX, "inheritance walk exceeded {OVERRIDE_WALK_MAX} hops");

        let mut found: Option<&'graph str> = None;

        for ancestor in &level {
            if !visited.insert(ancestor) {
                continue;
            }

            match (by_owner.get(&(*ancestor, name)).copied(), found) {
                (Some(method), None) => found = Some(method),
                (Some(method), Some(previous)) if method != previous => return None,
                _ => {}
            }

            if let Some(above) = bases.get(ancestor) {
                next.extend_from_slice(above);
            }
        }

        if found.is_some() {
            return found;
        }

        std::mem::swap(&mut level, &mut next);
        next.clear();
    }

    None
}

impl<'graph> ClassIndex<'graph> {
    /// The index built from one link pass's classes, methods, bases, callables,
    /// fields, and return edges.
    ///
    /// Every parameter carries the name of the store query that produced it, so
    /// the rows going in stay distinguishable from the maps they become.
    pub(crate) fn build(
        extends_edges: &'graph [(String, String)],
        class_methods: &'graph [(String, String)],
        class_identities: &'graph [(String, String, String)],
        callable_identities: &'graph [(String, String, String)],
        field_relations: &'graph [(String, String)],
        returns_edges: &'graph [(String, String)],
    ) -> Self {
        let mut bases: FxHashMap<&str, Vec<&str>> = FxHashMap::default();

        for (subclass, base) in extends_edges {
            bases.entry(subclass.as_str()).or_default().push(base.as_str());
        }

        let mut by_owner: FxHashMap<(&str, &str), &str> = FxHashMap::default();

        for (id, name) in class_methods {
            if let Some(owner) = method_owner_id(id) {
                by_owner.insert((owner, name.as_str()), id.as_str());
            }
        }

        let mut by_qualified: FxHashMap<&str, &str> = FxHashMap::default();
        let mut by_name: FxHashMap<&str, Vec<&str>> = FxHashMap::default();
        let mut class_ids: FxHashSet<&str> = FxHashSet::default();

        for (id, qualified_name, name) in class_identities {
            by_qualified.insert(qualified_name.as_str(), id.as_str());
            by_name.entry(name.as_str()).or_default().push(id.as_str());
            class_ids.insert(id.as_str());
        }

        assert!(by_qualified.len() <= class_identities.len(), "no more names than classes");
        assert!(class_ids.len() <= class_identities.len(), "no more ids than classes");

        let mut callables: FxHashMap<&str, Vec<(&str, &str)>> = FxHashMap::default();

        for (id, name, file_path) in callable_identities {
            callables.entry(name.as_str()).or_default().push((id.as_str(), file_path.as_str()));
        }

        let mut field_types: FxHashMap<(&str, &str), &str> = FxHashMap::default();

        for (id, related) in field_relations {
            if let Some((owner, field)) = id.rsplit_once('.') {
                field_types.insert((owner, field), related.as_str());
            }
        }

        let returns: FxHashMap<&str, &str> =
            returns_edges.iter().map(|(id, target)| (id.as_str(), target.as_str())).collect();

        Self {
            bases,
            by_name,
            by_owner,
            by_qualified,
            callables,
            class_ids,
            field_types,
            returns,
        }
    }

    /// The class id a typed receiver stands for. A plain type name is the class of
    /// that name, when the constellation holds exactly one. A [`RETURNS_OF`]
    /// candidate instead names the callee that produced the receiver
    /// (`demo = Demo.start(...)`, `crumbs = build_crumbs()`), so the class is
    /// whatever that callee's `returns` edge points at: the annotation is on the
    /// callee, which usually sits in another file, and this is the one pass that
    /// can read across.
    ///
    /// `None` at every ambiguity, as everywhere else in this pass: an owner naming
    /// several classes, a callee defined more than once, or a callee with no
    /// annotated return all bind nothing rather than pick.
    pub(crate) fn receiver_class(&self, receiver: &str) -> Option<&'graph str> {
        assert!(!receiver.is_empty(), "a typed receiver names its type");

        let Some(callee) = receiver.strip_prefix(RETURNS_OF) else {
            return sole_class_named(receiver, self);
        };

        let callable = match callee.split_once('.') {
            Some((owner, method)) => {
                let class = sole_class_named(owner, self)?;

                self.by_owner.get(&(class, method)).copied()?
            }
            None => self.sole_callable_named(callee)?,
        };

        let returned = self.returns.get(callable).copied()?;

        self.class_ids.contains(returned).then_some(returned)
    }

    /// The id of the sole callable named `name` across the constellation, or `None`
    /// when several define it. A bare function name carries no owner to
    /// disambiguate with, so uniqueness is the only evidence available and a shared
    /// name (`build`, `create`) types nothing.
    fn sole_callable_named(&self, name: &str) -> Option<&'graph str> {
        let mut matched = self.callables.get(name)?.iter();

        match (matched.next(), matched.next()) {
            (Some((id, _)), None) => Some(id),
            _ => None,
        }
    }

    /// The sole method named `name` callable on `model`: the model's own method,
    /// or one on the single queryset or manager class Django's naming convention
    /// gives it (its own or inherited). `None` when nothing or more than one
    /// definition answers.
    pub(crate) fn model_method(&self, model: &str, name: &str) -> Option<&'graph str> {
        assert!(!model.is_empty(), "a model method lookup names a model");
        assert!(!name.is_empty(), "a model method lookup names a method");

        let class = sole_class_named(model, self)?;

        if let Some(method) = self.by_owner.get(&(class, name)) {
            return Some(method);
        }

        if let Some(method) = sole_inherited_method(class, name, &self.bases, &self.by_owner) {
            return Some(method);
        }

        sole_companion_method(model, name, MANAGER_SUFFIXES, self)
    }
}
