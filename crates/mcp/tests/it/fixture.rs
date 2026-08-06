//! The indexed constellation the tool snapshots are rendered against.
//!
//! The other modules in this suite hand-build their graphs, node by node, which
//! is the right thing when a test is about one edge and wants nothing else in
//! the store to distract from it. A snapshot wants the opposite: text that
//! looks like what an agent actually receives, which means a graph that came
//! out of the real pipeline rather than one assembled to a renderer's
//! convenience. So this fixture writes source to disk and runs the indexer over
//! it, and the snapshots read whatever that produces.
//!
//! Two projects, not one. Cross-project linking is the feature constellation
//! exists for, and a single-project fixture would render every tool with the
//! interesting half missing: `links` would have nothing to list, `overview`
//! would count zero links, and an impact traversal would stop at the repository
//! boundary it is supposed to cross.
//!
//! Everything volatile is normalized before it reaches a snapshot. See
//! [`Fixture::render`], which is the only way text should leave this module.

use std::path::Path;
use std::process::Command;

use constellation_graph::ProjectId;
use constellation_index::{
    FlowOptions, compute_flows, index_project, ingest_history, ingest_symbol_revisions,
    link_constellation,
};
use constellation_mcp::ConstellationServer;
use constellation_store::Store;

/// The shared library project: an abstract base and a model the app imports.
const PLATFORM_MODELS: &str = "from django.db import models


class TimeStamped(models.Model):
    created_at = models.DateTimeField(auto_now_add=True)

    class Meta:
        abstract = True


class AuditLog(TimeStamped):
    action = models.CharField(max_length=64)

    def record(self, action):
        self.action = action

        return self.save()
";

/// The app's models: a model inheriting the shared base across the project
/// boundary, a relation, and a method the views and tests both reach.
const ORDER_MODELS: &str = "from django.db import models

from platform_core.models import AuditLog, TimeStamped


class Order(TimeStamped):
    reference = models.CharField(max_length=32)
    total = models.DecimalField(max_digits=10, decimal_places=2)

    def recalculate_totals(self):
        return self.total

    def audit(self):
        return AuditLog().record('order.saved')
";

/// The app's views: a class-based view and a function-based view, each naming a
/// template a different way, so the template edges have both shapes to render.
const ORDER_VIEWS: &str = "from django.shortcuts import render
from django.views.generic import DetailView

from orders.models import Order


class OrderDetailView(DetailView):
    model = Order
    template_name = 'orders/detail.html'


def checkout_view(request):
    order = Order.objects.first()
    order.recalculate_totals()

    return render(request, 'orders/checkout.html', {'order': order})
";

/// The app's URLconf, so `routes` has routes and the flow tracer has entry
/// points to start from.
const ORDER_URLS: &str = "from django.urls import path

from orders import views

app_name = 'orders'

urlpatterns = [
    path('checkout/', views.checkout_view, name='checkout'),
    path('<int:pk>/', views.OrderDetailView.as_view(), name='detail'),
]
";

/// A test module, so `tests` has coverage to report and `orphans` has a reason
/// not to call a covered symbol unreachable.
const ORDER_TESTS: &str = "from django.test import TestCase

from orders.models import Order


class OrderTestCase(TestCase):
    def test_recalculate_totals(self):
        return Order().recalculate_totals()
";

/// The detail template, extending a base and styling an element the stylesheet
/// also names, so the template and style edges both have two ends.
const DETAIL_TEMPLATE: &str = "{% extends 'orders/base.html' %}

{% block content %}
  <div class=\"order-card\">{{ order.total }}</div>
{% endblock %}
";

/// The base template the detail template extends.
const BASE_TEMPLATE: &str = "<html>{% block content %}{% endblock %}</html>\n";

/// The checkout template the function-based view renders.
const CHECKOUT_TEMPLATE: &str = "{% extends 'orders/base.html' %}

{% block content %}
  <form class=\"order-card\">{{ order.reference }}</form>
{% endblock %}
";

/// The stylesheet naming the class the templates carry.
const STYLES: &str = ".order-card { padding: 1rem; }\n";

/// A two-project constellation, indexed, linked, and traced.
///
/// The temporary directories are held so they outlive the store: dropping a
/// `Fixture` removes the trees the graph was built from.
pub struct Fixture {
    pub store: Store,
    pub shop: ProjectId,
    pub platform: ProjectId,
    shop_directory: tempfile::TempDir,
    platform_directory: tempfile::TempDir,
}

impl Fixture {
    /// The whole fixture built from scratch: two trees written to disk, both
    /// indexed into one in-memory store, linked across, and flow-traced.
    ///
    /// Every stage runs because every stage is a thing a tool renders. Skipping
    /// the link pass would snapshot a constellation that is not connected, and
    /// skipping the flow pass would snapshot the "run `constellation flows`"
    /// notice instead of any flow.
    pub fn build() -> Self {
        let platform_directory = tempfile::tempdir().expect("a temporary directory");
        let shop_directory = tempfile::tempdir().expect("a temporary directory");

        let platform_root = platform_directory.path();
        let shop_root = shop_directory.path();

        write(platform_root, "platform_core/__init__.py", "");
        write(platform_root, "platform_core/models.py", PLATFORM_MODELS);

        write(shop_root, "orders/__init__.py", "");
        write(shop_root, "orders/models.py", ORDER_MODELS);
        write(shop_root, "orders/views.py", ORDER_VIEWS);
        write(shop_root, "orders/urls.py", ORDER_URLS);
        write(shop_root, "orders/tests/test_orders.py", ORDER_TESTS);
        write(shop_root, "templates/orders/base.html", BASE_TEMPLATE);
        write(shop_root, "templates/orders/detail.html", DETAIL_TEMPLATE);
        write(shop_root, "templates/orders/checkout.html", CHECKOUT_TEMPLATE);
        write(shop_root, "static/orders.css", STYLES);

        let platform = ProjectId::new("platform");
        let shop = ProjectId::new("shop");
        let store = index_both(&platform, platform_root, &shop, shop_root);

        Self { store, shop, platform, shop_directory, platform_directory }
    }

    /// The same constellation behind an MCP server, for the two tools whose
    /// rendering lives on the server rather than in a free renderer: `explore`
    /// holds a graph cache across calls, and `path` searches over that cache.
    ///
    /// A second store rather than the one this fixture exposes, because
    /// [`ConstellationServer::new`] takes its store by value while every other
    /// snapshot reads [`Fixture::store`] directly. Both are indexed from the
    /// same two trees by the same pass, so the two agree.
    pub fn server(&self) -> ConstellationServer {
        let store = index_both(
            &self.platform,
            self.platform_directory.path(),
            &self.shop,
            self.shop_directory.path(),
        );

        ConstellationServer::new(store)
    }

    /// A rendered tool result with every volatile value replaced, ready to
    /// snapshot.
    ///
    /// Two things vary between runs of an otherwise identical fixture. The
    /// project roots are absolute paths into a directory whose name is random
    /// per run and whose separators differ between the two platforms CI builds
    /// on. And `status` prints how long ago each project was indexed, which is
    /// a wall-clock delta: it reads `0s ago` on a fast machine and something
    /// else on a loaded one, so pinning it would buy a flake in exchange for
    /// nothing. Every other path a tool prints is project-relative and already
    /// normalized to forward slashes by the walk.
    pub fn render(&self, text: &str) -> String {
        let shop = replace_root(text, self.shop_directory.path(), "<shop-root>");
        let both = replace_root(&shop, self.platform_directory.path(), "<platform-root>");
        let normalized = mask_ages(&both);

        assert!(
            !normalized.contains(temporary_marker()),
            "a temporary path survived normalization, so this snapshot would differ every run",
        );

        normalized
    }
}

/// The app at its first revision: one model, one field, one method.
const HISTORY_MODELS_FIRST: &str = "from django.db import models


class Order(models.Model):
    reference = models.CharField(max_length=32)

    def recalculate_totals(self):
        return 0
";

/// The app at its second revision: a field added, a method's signature changed,
/// and a method added, so the symbol history has one row of each kind to render
/// rather than only additions.
const HISTORY_MODELS_SECOND: &str = "from django.db import models


class Order(models.Model):
    reference = models.CharField(max_length=32)
    total = models.DecimalField(max_digits=10, decimal_places=2)

    def recalculate_totals(self, precision):
        return self.total

    def verify_password(self):
        return True
";

/// The app as the working tree leaves it: both method bodies edited and nothing
/// committed, which is the diff `changed` scores.
const HISTORY_MODELS_WORKING: &str = "from django.db import models


class Order(models.Model):
    reference = models.CharField(max_length=32)
    total = models.DecimalField(max_digits=10, decimal_places=2)

    def recalculate_totals(self, precision):
        return round(self.total, precision)

    def verify_password(self):
        return False
";

/// A view calling one of the two edited methods, so `changed` can rank a symbol
/// with a caller above one without.
const HISTORY_VIEWS: &str = "from orders.models import Order


def checkout_view(request):
    order = Order()

    return order.recalculate_totals(2)
";

/// The identity every fixture commit is authored and committed by.
const COMMIT_AUTHOR: &str = "constellation tests";

/// The address every fixture commit is authored and committed from.
const COMMIT_EMAIL: &str = "tests@example.invalid";

/// The time the first revision was committed.
///
/// Long enough ago to sit outside every window a tool measures against the
/// current time, which is what keeps this fixture's output a function of its
/// content. `changed` scores a symbol partly on commits to its file in the last
/// 90 days: a commit dated near the day the snapshot was taken counts toward
/// that window on the day it is taken and falls out of it three months later,
/// so the risk numbers would drift with the calendar and the snapshot would
/// fail on a date nobody changed anything on. Dated here, churn is zero
/// forever.
const FIRST_COMMITTED_AT: &str = "2020-01-05T09:00:00+00:00";

/// The time the second revision was committed. Old for the same reason as
/// [`FIRST_COMMITTED_AT`].
const SECOND_COMMITTED_AT: &str = "2020-02-10T14:30:00+00:00";

/// A project under git, at two commits, with its history ingested and its
/// working tree dirty.
///
/// Separate from [`Fixture`] rather than folded into it, because history is not
/// free context: once commits are ingested, explore's recency signal reads them
/// and every ranking downstream shifts. The tools that need history get a
/// fixture that has it, and the tools that do not stay pinned against a graph
/// built from source alone.
///
/// The dates and the identity are pinned, so a commit is a function of its
/// content: the tools print the date, and an unpinned one would differ every
/// run.
pub struct HistoryFixture {
    pub store: Store,
    pub shop: ProjectId,

    /// The commit hashes, oldest first, held for [`HistoryFixture::render`] and
    /// for addressing a point in the past by hash.
    commits: Vec<String>,
    directory: tempfile::TempDir,
}

impl HistoryFixture {
    /// The whole fixture: a git repository at two commits with a third revision
    /// left uncommitted, indexed, with both history tiers ingested.
    ///
    /// `None` when git is not on `PATH`, which every caller reports and skips.
    /// CI always has git, so the snapshots are pinned there even when a
    /// developer machine skips them.
    pub fn build() -> Option<Self> {
        if !Command::new("git").arg("--version").output().is_ok_and(|out| out.status.success()) {
            return None;
        }

        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path();

        write(root, "orders/__init__.py", "");
        write(root, "orders/models.py", HISTORY_MODELS_FIRST);
        write(root, "orders/views.py", HISTORY_VIEWS);

        if !git(root, FIRST_COMMITTED_AT, &["init", "--initial-branch=main"])
            || !git(root, FIRST_COMMITTED_AT, &["config", "core.autocrlf", "false"])
            || !git(root, FIRST_COMMITTED_AT, &["add", "-A"])
            || !git(root, FIRST_COMMITTED_AT, &["commit", "-m", "Add orders", "--no-gpg-sign"])
        {
            return None;
        }

        write(root, "orders/models.py", HISTORY_MODELS_SECOND);

        let second = ["commit", "-m", "Total on orders", "--no-gpg-sign"];

        if !git(root, SECOND_COMMITTED_AT, &["add", "-A"])
            || !git(root, SECOND_COMMITTED_AT, &second)
        {
            return None;
        }

        let commits = commit_hashes(root)?;

        assert_eq!(commits.len(), 2, "the fixture repository holds exactly its two commits");

        // Last, so the diff `changed` reads is this edit and nothing else.
        write(root, "orders/models.py", HISTORY_MODELS_WORKING);

        let store = Store::open_in_memory().expect("an in-memory store");
        let shop = ProjectId::new("shop");

        index_project(&store, &shop, "shop", root).expect("indexing the shop project");

        ingest_history(&store, &shop, root, HISTORY_COMMITS_MAX).expect("ingesting history");
        ingest_symbol_revisions(&store, &shop, root).expect("ingesting symbol revisions");

        Some(Self { store, shop, commits, directory })
    }

    /// The first commit's hash, the point in the past `as_of` is asked about.
    pub fn first_commit(&self) -> &str {
        self.commits.first().expect("the fixture repository has a first commit")
    }

    /// A rendered tool result with every volatile value replaced, ready to
    /// snapshot. As [`Fixture::render`], plus the commit hashes.
    ///
    /// The hashes are masked rather than pinned even though the dates and the
    /// identity make them reproducible, because they are also a function of
    /// git's object format: a repository created under SHA-256, or under a
    /// future default, holds the same history under different names. Nothing a
    /// renderer decides is in a hash, so pinning one would buy a cross-platform
    /// failure in exchange for nothing.
    pub fn render(&self, text: &str) -> String {
        let mut out = replace_root(text, self.directory.path(), "<shop-root>");

        // Full hashes before short ones, or a full hash is left with its tail
        // dangling off the placeholder that replaced its prefix.
        for (position, hash) in self.commits.iter().enumerate() {
            let placeholder = format!("<commit-{}>", position + 1);
            let short = &hash[..hash.len().min(SHORT_HASH_LENGTH)];

            out = out.replace(hash.as_str(), &placeholder).replace(short, &placeholder);
        }

        let normalized = mask_ages(&out);

        assert!(
            !normalized.contains(temporary_marker()),
            "a temporary path survived normalization, so this snapshot would differ every run",
        );

        normalized
    }
}

/// The commit cap the fixture ingests under, far above its two commits.
const HISTORY_COMMITS_MAX: u32 = 64;

/// The leading characters of a commit hash the history tools print.
const SHORT_HASH_LENGTH: usize = 8;

/// A git invocation in `root`, returning whether it succeeded.
///
/// Both timestamps and both identities are set on every call, not just the
/// commits: git reads the committer date when it writes a commit object, and a
/// repository that inherited the developer's `user.name` would put that name in
/// the rendered history.
fn git(root: &Path, date: &str, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", COMMIT_AUTHOR)
        .env("GIT_AUTHOR_EMAIL", COMMIT_EMAIL)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_NAME", COMMIT_AUTHOR)
        .env("GIT_COMMITTER_EMAIL", COMMIT_EMAIL)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The commit hashes in `root`, oldest first.
fn commit_hashes(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--reverse", "--format=%H"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let listing = String::from_utf8(output.stdout).ok()?;

    Some(listing.lines().map(str::to_string).collect())
}

/// The two trees indexed into one fresh in-memory store, linked across, and
/// flow-traced.
///
/// Every stage runs because every stage is a thing a tool renders. Skipping the
/// link pass would produce a constellation that is not connected, and skipping
/// the flow pass would render the "run `constellation flows`" notice instead of
/// any flow.
fn index_both(
    platform: &ProjectId,
    platform_root: &Path,
    shop: &ProjectId,
    shop_root: &Path,
) -> Store {
    let store = Store::open_in_memory().expect("an in-memory store");

    let platform_stats = index_project(&store, platform, "platform", platform_root)
        .expect("indexing the platform project");

    let shop_stats =
        index_project(&store, shop, "shop", shop_root).expect("indexing the shop project");

    assert!(platform_stats.files_indexed > 0, "the platform tree indexed no files");
    assert!(shop_stats.files_indexed > 0, "the shop tree indexed no files");

    link_constellation(&store).expect("linking the constellation");

    compute_flows(&store, shop, FlowOptions::default()).expect("tracing the shop flows");

    store
}

/// The prefix every temporary directory in this fixture shares, used to catch a
/// path that normalization missed.
///
/// `tempfile` names its directories `.tmp` followed by random characters, so a
/// leaked absolute path always carries it and a normalized one never does.
fn temporary_marker() -> &'static str {
    ".tmp"
}

/// A source file written under `root`, creating parent directories as needed.
fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the parent directory");
    }

    std::fs::write(&path, source).expect("writing a fixture file");
}

/// `text` with every `12s ago` style elapsed time replaced by `<age>`.
///
/// The count and its unit are both masked, because a delta that crosses a unit
/// boundary changes the unit too: the same run reads `59s ago` or `1m ago`
/// depending only on how loaded the machine was.
fn mask_ages(text: &str) -> String {
    const SUFFIX: &str = " ago";
    const UNITS: [char; 4] = ['d', 'h', 'm', 's'];

    /// A cap on how many ages one rendered result may carry, which `status`
    /// reaches at one per indexed project.
    const AGES_MAX: u32 = 1_024;

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut seen: u32 = 0;

    while let Some(position) = rest.find(SUFFIX) {
        seen += 1;

        assert!(seen <= AGES_MAX, "a rendered result never carries this many elapsed times");

        let (head, tail) = rest.split_at(position);
        let without_unit = head.strip_suffix(UNITS).unwrap_or(head);

        let without_count =
            without_unit.trim_end_matches(|character: char| character.is_ascii_digit());

        // Only a count that was actually there is masked, so prose that merely
        // ends in the word "ago" is left exactly as the renderer wrote it.
        if without_count.len() < without_unit.len() {
            out.push_str(without_count);
            out.push_str("<age>");
        } else {
            out.push_str(head);
            out.push_str(SUFFIX);
        }

        rest = &tail[SUFFIX.len()..];
    }

    out.push_str(rest);

    out
}

/// `text` with every spelling of `root` replaced by `placeholder`.
///
/// Both separator forms are replaced because the two do not agree on which one
/// reaches the store: the walk normalizes the paths it derives, while the root
/// itself is recorded as the platform spells it.
fn replace_root(text: &str, root: &Path, placeholder: &str) -> String {
    let native = root.to_string_lossy().to_string();
    let posix = native.replace('\\', "/");

    text.replace(&native, placeholder).replace(&posix, placeholder)
}
