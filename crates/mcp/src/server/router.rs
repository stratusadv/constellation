//! The tool surface: one `#[tool]` method per tool, and the `ServerHandler`
//! that advertises them.
//!
//! Each method carries an explicit `#[tool(name = "...")]` that drops the
//! `constellation_` prefix the method name keeps. Every client namespaces an
//! MCP tool by its server, so a prefixed method name reaches the agent
//! stuttered (`constellation_constellation_overview`), which agents then
//! "correct" to a name that does not exist. The method names stay prefixed
//! because they are what the rest of this crate, and its hint keys, refer to.
//!
//! Every method here is deliberately a shell. It reads its arguments, locks the
//! store, calls one renderer in [`crate::tools`], and wraps the result. Any
//! method that grows a second idea belongs in a renderer instead, so this file
//! stays a readable index of what the server can answer.

use std::sync::atomic::Ordering;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ServerCapabilities, ServerInfo};
use rmcp::{
    ErrorData, ServerHandler, tool, tool_handler, tool_router,
};

use super::ConstellationServer;
use crate::args::{
    AffectedFlowsArgs, AsOfArgs, AtArgs, ChangedArgs, ExploreArgs, FilesArgs, FlowsArgs,
    HistoryArgs, ImpactArgs, LinksArgs, OrphansArgs, OverviewArgs, PathArgs, RoutesArgs,
    SearchArgs, SubclassesArgs, SymbolArgs, SymbolHistoryArgs, WinnowArgs,
};
use crate::error::NO_INDEX_MESSAGE;
use crate::git::check_revision;
use crate::limits::{
    AFFECTED_FLOWS_LIMIT_DEFAULT, AS_OF_LIMIT_DEFAULT, EXPLORE_FILES_DEFAULT, FLOWS_LIMIT_DEFAULT,
    FLOWS_LIMIT_MAX, HISTORY_LIMIT_DEFAULT, IMPACT_DEPTH_DEFAULT, IMPACT_DEPTH_MAX,
    LINKS_LIMIT_DEFAULT, RELATED_LIMIT_DEFAULT, ROUTES_LIMIT_DEFAULT, SEARCH_LIMIT_DEFAULT,
};
use crate::render::{file_path_in_line, first_named_symbol, text_result, with_hint};
use crate::tools::changed::changed_text;
use crate::tools::feature::feature_text;
use crate::tools::flows::{affected_flows_text, any_flows_computed, flows_text};
use crate::tools::history::{as_of_text, history_text, symbol_history_text};
use crate::tools::impact::{impact_text, orphans_text, subclasses_text, tests_text};
use crate::tools::project::{files_text, links_text, overview_text, routes_text};
use crate::tools::search::search_text;
use crate::tools::status::status_text;
use crate::tools::symbol::{at_text, callees_text, callers_text, model_text, node_text};
use crate::tools::winnow::winnow_text;
use crate::{cursor, hints, winnow};

// `vis = "pub"` so the integration suite can enumerate the advertised tools and
// assert on their names. The stuttered-prefix bug this guards against is invisible
// from inside the crate: it only appears in what the router advertises.
#[tool_router(vis = "pub")]
impl ConstellationServer {
    #[tool(name = "status", description = "Index health: project, node, edge, and cross-project link counts, plus working-tree staleness.")]
    fn constellation_status(&self) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store_text(status_text)?;

        Ok(text_result(text))
    }

    #[tool(name = "history", description = "How a file or app changed over time, from indexed git history: the commits that touched a path (newest first) with per-commit churn (+lines/-lines, files changed) and author. Pass target=<path substring> (a file like \"orders/models.py\", or an app like \"orders/\"); omit target for recent activity across the constellation. project=<id or name> scopes it. Requires `constellation history` to have been run; empty otherwise.")]
    fn constellation_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(HISTORY_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            history_text(
                store,
                args.target.as_deref(),
                args.project.as_deref(),
                limit,
                &page,
                generation,
            )
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "symbol_history", description = "How a symbol changed over time, from indexed git history: each commit where the symbol (a function, method, class, Django model/view/route, or model field) was added, modified (signature changed), or removed, newest first, with date and signature. Pass symbol=<name or qualified name>. project=<id or name> scopes it. Requires `constellation history --symbols` to have been run; empty otherwise.")]
    fn constellation_symbol_history(
        &self,
        Parameters(args): Parameters<SymbolHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(HISTORY_LIMIT_DEFAULT);
        let text = self.with_store_text(|store| {
            symbol_history_text(store, &args.symbol, args.project.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "as_of", description = "The symbols that existed at a point in the past, reconstructed from indexed symbol history: pass at=<commit hash or YYYY-MM-DD> to list the functions, methods, classes, Django models/views/routes, and fields alive then (with their signatures at that time), grouped by file. project=<id or name> scopes it (recommended for a commit hash); path=<substring> narrows to a file or app. Requires `constellation history --symbols`; answers \"what did this look like at version X\".")]
    fn constellation_as_of(
        &self,
        Parameters(args): Parameters<AsOfArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(AS_OF_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            as_of_text(
                store,
                &args.at,
                args.project.as_deref(),
                args.path.as_deref(),
                limit,
                &page,
                generation,
            )
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "search", description = "Find symbols by name across all projects (substring/fuzzy; exact then prefix matches first, definitions before references). Returns locations only: use `explore` to read the code.")]
    fn constellation_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(SEARCH_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text =
            self.with_store_text(|store| search_text(store, &args.query, limit, &page, generation))?;

        let facts = hints::HintFacts {
            named_symbol: Some(args.query.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_search", &facts))))
    }

    #[tool(name = "node", description = "One symbol's detail: kind, location, signature, docstring, and caller/callee counts.")]
    fn constellation_node(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store_text(|store| node_text(store, &args.symbol))?;

        let facts = hints::HintFacts {
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_node", &facts))))
    }

    #[tool(name = "model", description = "A Django model's effective schema in one call: its own fields plus those inherited up the base-class chain (abstract bases, mixins, cross-project bases), its bases, and its relations (foreign keys / M2M to other models). Django scatters these across the MRO; this assembles them. Pass a model name (Owner.field form not needed).")]
    fn constellation_model(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store_text(|store| model_text(store, &args.symbol))?;

        let facts = hints::HintFacts {
            named_model: true,
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_model", &facts))))
    }

    #[tool(name = "callers", description = "What references a symbol: callers, imports, route->view, view->template, model relations, cross-project imports (edges grep cannot see), each call/instantiation with the source line of the call site, so you see how it is used, not just who uses it.")]
    fn constellation_callers(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store_text(|store| callers_text(store, &args.symbol, limit))?;

        self.record_session_files(text.lines().filter_map(file_path_in_line));

        let facts = hints::HintFacts {
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_callers", &facts))))
    }

    #[tool(name = "callees", description = "What a symbol references: its callees, imports, bases, and Django relations (a model's related models, a view's template).")]
    fn constellation_callees(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store_text(|store| callees_text(store, &args.symbol, limit))?;

        let facts = hints::HintFacts {
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_callees", &facts))))
    }

    #[tool(name = "tests", description = "The tests that cover a symbol: TestCase classes bound to it by the XTestCase->X naming convention, plus test functions/methods that call it. '(no covering tests)' when none, so before a change you know what to run and whether the symbol is guarded. Pass a symbol name or Owner.member.")]
    fn constellation_tests(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let text = self.with_store_text(|store| tests_text(store, &args.symbol, limit))?;

        let facts = hints::HintFacts {
            has_uncovered_symbol: text.contains("(no covering tests)"),
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_tests", &facts))))
    }

    #[tool(name = "subclasses", description = "The transitive subclasses of a base class or mixin: every type that extends it, directly or through intermediate bases, across projects (e.g. every model using HistoryModelMixin, every BaseDjangoModelService subclass). Pass the base name.")]
    fn constellation_subclasses(
        &self,
        Parameters(args): Parameters<SubclassesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            subclasses_text(store, &args.symbol, limit, &page, generation)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "orphans", description = "Candidate dead code in one project: definitions (functions, methods, classes, models) nothing calls, imports, instantiates, tests, relates to, or extends. Framework-reached symbols (tests, migrations, __init__, dunder methods, app configs) are filtered out, but verify each before deleting - a symbol reached only by a runtime/string convention can still surface. Pass project=<id or name>.")]
    fn constellation_orphans(
        &self,
        Parameters(args): Parameters<OrphansArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            orphans_text(store, args.project.as_deref(), limit, &page, generation)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "changed", description = "What changed and what to review first: the symbols overlapping the working-tree (plus staged) diff against a base (default HEAD; pass base=<ref> like base=main for a whole-branch diff), RISK-RANKED highest first, each with a 0.00-1.00 score and the two or three reasons behind it. The score blends missing test coverage, a security-sensitive name (auth/password/token/payment/...), participation in a high-criticality Django flow, direct caller count, callers in other apps, callers in other repositories, commits to the file in the last 90 days, and the number of changed lines inside the symbol. Factors the index cannot supply are dropped and the rest renormalized, with a note saying which and how to populate it (`constellation history`, `constellation flows`). The edit-impact view git diff alone cannot give. Runs git in each indexed repo.")]
    fn constellation_changed(
        &self,
        Parameters(args): Parameters<ChangedArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(RELATED_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        if let Some(refused) = refused_base(args.base.as_deref()) {
            return Ok(text_result(refused));
        }

        let (flows_available, text) = self.with_store(
            || (false, NO_INDEX_MESSAGE.to_string()),
            |store| {
                let available = any_flows_computed(store)?;
                let text = changed_text(store, args.base.as_deref(), limit, &page, generation)?;

                Ok((available, text))
            },
        )?;

        let facts = hints::HintFacts {
            flows_available,
            has_uncovered_symbol: text.contains("no tests"),
            named_symbol: first_named_symbol(&text),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_changed", &facts))))
    }

    #[tool(name = "impact", description = "Transitive callers of a symbol: its blast radius before a change, breadth-first to a depth.")]
    fn constellation_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let depth = args.depth.unwrap_or(IMPACT_DEPTH_DEFAULT).min(IMPACT_DEPTH_MAX);

        assert!(depth <= IMPACT_DEPTH_MAX, "traversal depth is capped");

        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            impact_text(store, &args.symbol, depth, &page, generation)
        })?;

        let facts = hints::HintFacts {
            named_symbol: Some(args.symbol.clone()),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_impact", &facts))))
    }

    #[tool(name = "explore", description = "PRIMARY: try first. Give ONE or TWO rare, specific identifiers (e.g. \"ArticleForm subtotal_amount\"); avoid generic words like \"inventory\"/\"form_views\" that match dozens of files. Matches names, docstrings, AND source bodies (porter-stemmed); ranks exact symbol-name matches first, then rare tokens (IDF) over common ones, then graph structure. Returns the relevant source grouped by file (Read-equivalent), line-numbered; the top files come back in full, the rest as signature-only outlines. Name TWO symbols (\"order_summary_view Comment\") to also trace the call path between them (how X reaches Y across files). Pass outline=true for a signature-only survey of every matched file (no bodies), cheap when mapping breadth before drilling in.")]
    fn constellation_explore(
        &self,
        Parameters(args): Parameters<ExploreArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let max_files = args.max_files.unwrap_or(EXPLORE_FILES_DEFAULT);
        let outline = args.outline.unwrap_or(false);

        self.explore(&args.query, max_files, outline)
    }

    #[tool(name = "files", description = "Project file layout. No argument → each project summarized by top-level package with file + symbol counts (aggregated, so a large repo doesn't flood the response). project=<id or name> → that project's package breakdown. pattern=<text> → list the files whose path contains that substring (e.g. \"models.py\", \"billing/\"), source files first. Faster than globbing.")]
    fn constellation_files(
        &self,
        Parameters(args): Parameters<FilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            files_text(store, args.project.as_deref(), args.pattern.as_deref(), &page, generation)
        })?;

        let hint = self.hint_for("constellation_files", &hints::HintFacts::default());

        Ok(text_result(with_hint(text, &hint)))
    }

    #[tool(name = "overview", description = "Orient in one call: per project, the file and symbol counts, the Django surface (models, views, routes, templates), the largest packages, and the cross-project link total. Read this first when unfamiliar with the constellation, before explore/files. project=<id or name> focuses one project.")]
    fn constellation_overview(
        &self,
        Parameters(args): Parameters<OverviewArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let (flows_available, text) = self.with_store(
            || (false, NO_INDEX_MESSAGE.to_string()),
            |store| {
                let available = any_flows_computed(store)?;
                let text = overview_text(store, args.project.as_deref())?;

                Ok((available, text))
            },
        )?;

        let facts = hints::HintFacts {
            flows_available,
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_overview", &facts))))
    }

    #[tool(name = "feature", description = "The vertical slice of a feature: from a route, view, template, or model, assemble the whole Django path (route->view->template(s)->includes, model relations, service/queryset instantiation, base mixins, signal handlers) as one grouped digest, instead of chaining callers/callees by hand. Pass a route name, view, model, or template.")]
    fn constellation_feature(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store_text(|store| feature_text(store, &args.symbol))?;

        Ok(text_result(text))
    }

    #[tool(name = "routes", description = "The URL map: every route's pattern -> its view -> the template it renders, grouped by project: the app's external surface as one table, the orientation a pile of urls.py files cannot give at a glance. project=<id or name> restricts it (recommended for a large constellation). pattern=<text> filters to routes whose pattern, view, template, or full name contains that substring (e.g. \"detail\"), so a single-route question need not dump the whole map.")]
    fn constellation_routes(
        &self,
        Parameters(args): Parameters<RoutesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            routes_text(
                store,
                args.project.as_deref(),
                args.pattern.as_deref(),
                ROUTES_LIMIT_DEFAULT,
                &page,
                generation,
            )
        })?;

        let hint = self.hint_for("constellation_routes", &hints::HintFacts::default());

        Ok(text_result(with_hint(text, &hint)))
    }

    #[tool(name = "links", description = "The cross-project links: imports in one repo resolved to a definition in another (the edges that make this a constellation rather than separate indexes). Grouped by repo pair. project=<id or name> filters to links touching that project. Empty when only one repo is indexed.")]
    fn constellation_links(
        &self,
        Parameters(args): Parameters<LinksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(LINKS_LIMIT_DEFAULT);
        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let text = self.with_store_text(|store| {
            links_text(store, args.project.as_deref(), limit, &page, generation)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "path", description = "The shortest call/flow path between two symbols: how `from` reaches `to` across files (calls, route->view, view->template, instantiation, inheritance), as one chain instead of manual callers/callees spelunking. Names accept `Owner.member`; both directions are searched.")]
    fn constellation_path(
        &self,
        Parameters(args): Parameters<PathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.path(&args.from, &args.to)
    }

    #[tool(name = "at", description = "The innermost symbol at a file:line: map a traceback frame, a stack line, or a grep hit back to its enclosing function/method/class. Pass the path as constellation prints it (a suffix like \"views.py\" is enough) and the 1-based line.")]
    fn constellation_at(
        &self,
        Parameters(args): Parameters<AtArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = self.with_store_text(|store| at_text(store, &args.file, args.line))?;

        self.record_session_files(text.lines().filter_map(file_path_in_line));

        let facts = hints::HintFacts {
            named_symbol: first_named_symbol(&text),
            ..hints::HintFacts::default()
        };

        Ok(text_result(with_hint(text, &self.hint_for("constellation_at", &facts))))
    }

    #[tool(name = "flows", description = "Every Django execution flow in the codebase, ranked by criticality, with NO symbol named first: the question \"what are the user-facing paths here, and which matter most\". A flow is one framework entry point (a URL route, a DRF view, a management command, a Celery task, a signal receiver, an admin action, an AppConfig.ready hook, a model save/delete/clean override, or a true root) plus the bounded set of symbols reachable from it through calls, route->view, view->template, template extends/includes, resolves, handles, receives, and instantiation. Criticality blends the entry kind, how many apps the reach set spans, how much of it is security-sensitive, how much is untested, how many repositories it crosses, how often it leaves the graph (external or dynamically dispatched), and its depth. project=<id or name>, pattern=<text> (name or entry kind), sort=criticality|size|name. Requires `constellation flows` to have been run; returns an explicit empty otherwise.")]
    fn constellation_flows(
        &self,
        Parameters(args): Parameters<FlowsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(FLOWS_LIMIT_DEFAULT).min(FLOWS_LIMIT_MAX);

        assert!(limit <= FLOWS_LIMIT_MAX, "a flow listing limit is capped");

        let text = self.with_store_text(|store| {
            flows_text(store, args.project.as_deref(), args.pattern.as_deref(), args.sort.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "winnow", description = "Compose several filters into one query, instead of running four tools and intersecting by hand. Takes criteria=[{axis, op, value}], ANDed; the `axis` and `op` argument schemas list every valid value, and an unknown one is rejected with them named, never silently ignored. Axes cover structure (kind, language, project, name, file, decorator), edges (calls, called_by, extends, relates_to, renders), size and use (lines, callers), history (churn, changed_since), and review state (tested, in_flow, risk). `matches` is a GLOB (* and ?), not a regular expression. `lines` is the honest proxy for how much a symbol does; there is no complexity axis. rank= risk (default), churn, callers, lines, criticality, name. Example: models with a foreign key to Order, changed recently, with no covering tests -> criteria=[{axis:kind,op:eq,value:model},{axis:relates_to,op:contains,value:Order},{axis:changed_since,op:>=,value:2026-06-01},{axis:tested,op:eq,value:false}].")]
    fn constellation_winnow(
        &self,
        Parameters(args): Parameters<WinnowArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(winnow::WINNOW_RESULTS_DEFAULT).min(winnow::WINNOW_RESULTS_MAX);

        assert!(limit <= winnow::WINNOW_RESULTS_MAX, "a winnow limit is capped");

        let generation = self.generation.load(Ordering::Relaxed);
        let page = cursor::resolve(args.cursor.as_deref(), generation);

        let raw: Vec<winnow::RawCriterion<'_>> = args
            .criteria
            .iter()
            .map(|criterion| winnow::RawCriterion {
                axis: criterion.axis.as_str(),
                op: criterion.op.as_str(),
                value: criterion.value.as_str(),
                window_days: criterion.window_days,
            })
            .collect();

        let text = self.with_store_text(|store| {
            winnow_text(store, &raw, args.rank.as_deref(), limit, &page, generation)
        })?;

        Ok(text_result(text))
    }

    #[tool(name = "affected_flows", description = "Which user-facing flows a change touches: takes the working-tree (plus staged) diff against a base (default HEAD; pass base=<ref> like base=main for a whole-branch diff), or an explicit files=[...] list, and returns the Django execution flows whose reach set contains the changed symbols, ranked by criticality. The review question \"what can this diff break for a user\", answered from the graph instead of by reading every caller. Requires `constellation flows` to have been run; returns an explicit empty otherwise.")]
    fn constellation_affected_flows(
        &self,
        Parameters(args): Parameters<AffectedFlowsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = args.limit.unwrap_or(AFFECTED_FLOWS_LIMIT_DEFAULT).min(FLOWS_LIMIT_MAX);

        assert!(limit <= FLOWS_LIMIT_MAX, "a flow listing limit is capped");

        if let Some(refused) = refused_base(args.base.as_deref()) {
            return Ok(text_result(refused));
        }

        let text = self.with_store_text(|store| {
            affected_flows_text(store, args.base.as_deref(), args.files.as_deref(), limit)
        })?;

        Ok(text_result(text))
    }
}

/// The refusal text for a `base` git will not read as a revision, or `None`
/// when it is fine (or absent, which means `HEAD`).
///
/// Refusing out loud rather than falling back to `HEAD`: a silent fallback
/// answers a question about `main` with the answer for `HEAD`, which reads as
/// "your branch changed nothing".
fn refused_base(base: Option<&str>) -> Option<String> {
    let error = check_revision(base?).err()?;

    Some(format!(
        "base {:?} is not usable as a git revision: {error}. \
         Pass a branch, tag, commit hash, or an expression like HEAD~3.",
        base.unwrap_or_default(),
    ))
}

#[tool_handler]
impl ServerHandler for ConstellationServer {
    fn get_info(&self) -> ServerInfo {
        // rmcp calls this from the async handshake, so the count goes through
        // the same off-runtime path every tool uses. It is one query, but a
        // query on the event loop is a query on the event loop, and the rule
        // this crate is built on does not have a size exemption.
        let links = self.with_store(|| 0, |store| store.count_links()).unwrap_or(0);

        let mut instructions = String::from(
            "FIRST: before any Grep, Glob, Read, or other file search, for ANY question about \
             this codebase (where a symbol is, how it works, what calls / renders / extends what, \
             a model's schema, or the blast radius of a change), call a constellation tool. The \
             graph is pre-built and sub-millisecond; reach for grep/read only for literal text it \
             cannot index (string contents, comments, log lines), or to confirm one detail a \
             constellation call already located.\n\n\
             Constellation is a pre-built, sub-millisecond code-intelligence graph of these \
             Django projects: every symbol, call, and import, plus Django structure grep can't \
             give (routes->views, views->templates (render() and template= kwargs), template \
             extends/includes, model fields and foreign keys (relates_to), return and attribute \
             types (returns/type_of), signal handlers, and inheritance from third-party bases (an \
             <external> mixin resolves)). Consult it BEFORE grepping or reading files.\n\n\
             Tool names below are written bare. Your client namespaces them with a prefix of its \
             own (constellation_explore, mcp__constellation__explore, and so on), so call each one \
             exactly as it appears in your own tool list, not as it is spelled here.\n\n\
             Tools by intent:\n\
             - overview: orientation. Per project: file/symbol counts, the Django \
             surface (models/views/routes/templates), largest packages, cross-project link total. \
             Read FIRST when unfamiliar, before explore/files.\n\
             - explore: PRIMARY, try first. Give symbol/file names or concrete domain \
             words (e.g. \"Order order_number generate\"), matched against names, docstrings, \
             AND source bodies (stemmed), then ranked by graph structure (exact name/file matches \
             first). Returns their source grouped by file (Read-equivalent), line-numbered. Use \
             real code identifiers, not abstract prose.\n\
             - search: find a symbol by name (substring/fuzzy) when you only need its \
             location.\n\
             - node: one symbol's kind, signature, docstring, caller/callee counts; \
             pass Owner.member, or the printed file::name, to disambiguate an overloaded name.\n\
             - model: a Django model's effective schema (own + inherited fields \
             across its base chain (abstract bases, mixins), bases, and relations). One call for what \
             Django spreads over the MRO.\n\
             - callers / callees: what references a symbol / what it \
             references; Django edges grep cannot follow, deduped (xN for repeats).\n\
             - impact: transitive non-test callers (blast radius) before a change.\n\
             - path: the shortest call/flow path between two symbols, i.e. how one \
             reaches the other across files (give from + to); the answer to \"how does X get to \
             Y\".\n\
             - at: the symbol at a file:line; map a traceback frame or grep hit to \
             its enclosing function/method/class.\n\
             - files: project layout, packages with symbol counts (project=<id> for \
             a directory breakdown).\n\
             - links: the cross-project links themselves, imports in one repo \
             resolved to a definition in another, grouped by repo pair.\n\
             - status: index health and working-tree staleness.\n\
             - history: how a file or app changed over time from git \
             history (the commits touching a path, newest first, with +/- line churn); \
             run `constellation history` first to populate it.\n\
             - symbol_history: how one symbol (function, method, class, \
             Django model/view/route, or model field) changed over time, the commits \
             that added, modified (signature change), or removed it; run \
             `constellation history --symbols` first.\n\
             - as_of: the symbols that existed at a past point \
             (at=<commit hash or YYYY-MM-DD>), grouped by file, with their \
             signatures as they were then: \"what did this look like at version \
             X\". Needs `constellation history --symbols`.\n\n\
             Recall caveat: edges come from a static parse, scoped to imports (a cross-file call to \
             a symbol the file does not import is dropped, not guessed). Several dynamic patterns \
             are KNOWN-DARK: a low caller/impact count on these is NOT 'safe to change': (1) a \
             custom QuerySet/Manager method reached only through a CHAINED queryset \
             (`.objects.active().by_year()`; the first hop resolves, later hops do not) or via \
             `self.get_queryset()`; a direct `Model.objects.by_year()` DOES resolve; (2) \
             function-local imports and calls to external module.attr() helpers; (3) a method \
             reached only via a template ({{ obj }}), str()/__str__, the admin, or a \
             string-reference FK. Treat these layers as 'edges may be missing', not 'no edges'.",
        );

        if links > 0 {
            instructions.push_str(
                "\n\nThis index spans multiple repos: imports crossing repository boundaries are \
                 linked, and callers/callees/explore follow those cross-project edges.",
            );
        }

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }
}
