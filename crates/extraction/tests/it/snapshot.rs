//! Pinned extractor output: one snapshot per fixture, across every language.
//!
//! The other modules in this suite assert on one fact each, which is what makes
//! them readable and what makes them narrow. A parser refactor does not break
//! one fact; it moves a hundred of them at once, and the ones nobody wrote an
//! assertion for are exactly the ones that go quietly. These fixtures cover the
//! whole of an extractor's output instead, so a refactor that was meant to
//! change nothing shows up as an empty diff or does not pass.
//!
//! The fixtures are therefore deliberately broad rather than minimal: each is a
//! plausible file from a Django project, chosen to touch as many extraction
//! paths at once as one readable file can. A fixture that isolates one
//! behaviour belongs in the module for its language, next to the assertion that
//! names the behaviour.
//!
//! Updating a snapshot is not a way to make a test pass. The diff is the
//! review: every moved line is a change in what an agent will be told about the
//! code, and it is accepted only once someone has read it and agrees.
//!
//! ```text
//! cargo test -p constellation-extraction     # writes .snap.new beside each miss
//! cargo insta review                         # read every diff, accept or reject
//! ```

use constellation_extraction::{
    CssExtractor, Extractor, JavaScriptExtractor, PythonExtractor, TemplateExtractor,
};
use constellation_graph::ProjectId;

use crate::dump::dump;

/// The project every fixture is extracted under. One id across the suite keeps
/// the prefix the dump strips constant, so a snapshot shows qualified names
/// rather than repeating the project on every line.
const PROJECT: &str = "shop";

/// A Django models module: a module docstring, aliased and package imports, a
/// module constant, an abstract base with an inner `Meta`, a concrete model
/// carrying every relation field kind, and methods of each flavour the
/// extractor classifies separately (dunder, property, classmethod, async).
const MODELS: &str = r#""""Order records and the helpers that build them."""

from django.db import models
from django.utils import timezone as tz

from shop.customers.models import Customer

STATUS_OPEN = 'open'


class TimeStamped(models.Model):
    """The audit columns every record carries."""

    created_at = models.DateTimeField(default=tz.now)

    class Meta:
        abstract = True


class Order(TimeStamped):
    customer = models.ForeignKey(Customer, on_delete=models.CASCADE, related_name='orders')
    tags = models.ManyToManyField('shop.Tag', blank=True)
    status = models.CharField(max_length=16, default=STATUS_OPEN)

    def __str__(self):
        return f'Order {self.pk}'

    @property
    def is_open(self):
        return self.status == STATUS_OPEN

    @classmethod
    def open_orders(cls):
        return cls.objects.filter(status=STATUS_OPEN)

    async def notify(self):
        await self.customer.send_receipt()


def build_order(customer: Customer) -> Order:
    """The order created for a customer."""
    return Order.objects.create(customer=customer)
"#;

/// A views module: a class-based view naming its template as an attribute, a
/// function-based view naming one through `render`, a `super()` dispatch, a
/// queryset chain, and a `reverse` of a namespaced route.
const VIEWS: &str = "from django.shortcuts import redirect, render
from django.urls import reverse
from django.views.generic import DetailView

from shop.orders.models import Order


class OrderDetailView(DetailView):
    model = Order
    template_name = 'orders/detail.html'

    def get_context_data(self, **kwargs):
        context = super().get_context_data(**kwargs)
        context['open'] = Order.open_orders()

        return context


def checkout_view(request):
    order = Order.objects.first()
    order.notify()

    return render(request, 'orders/checkout.html', {'order': order})


def cancel_view(request, pk):
    return redirect(reverse('orders:detail', args=[pk]))
";

/// A URLconf: an app namespace, a route to a function view, a route to a
/// class-based view through `as_view()`, and an `include` that opens a nested
/// namespace.
const URLS: &str = "from django.urls import include, path

from shop.orders import views

app_name = 'orders'

urlpatterns = [
    path('', views.checkout_view, name='checkout'),
    path('<int:pk>/', views.OrderDetailView.as_view(), name='detail'),
    path('archive/', include('shop.orders.archive_urls', namespace='archive')),
]
";

/// A Django template: inheritance, a tag library load, a block, a loop binding,
/// a filter, a `url` reversal, a conditional, an include with context, and an
/// Alpine component binding onto a styled element.
const TEMPLATE: &str = r#"{% extends "base.html" %}
{% load humanize %}

{% block content %}
  <div class="order-card" x-data="orderCard">
    <h1>{{ order.customer.name }}</h1>

    {% for line in order.lines.all %}
      <p class="total">{{ line.total|intcomma }}</p>
    {% endfor %}

    {% if order.is_open %}
      <a href="{% url 'orders:detail' order.pk %}">Detail</a>
    {% endif %}

    {% include "orders/_summary.html" with order=order %}
  </div>
{% endblock %}
"#;

/// An Alpine component: a module import, component state and methods, an arrow
/// closure, a `$dispatch` of a custom event, and the listener that answers it.
const COMPONENT: &str = "import { formatMoney } from './money.js';

Alpine.data('orderCard', () => ({
    lines: [],
    total() {
        return formatMoney(this.lines.length);
    },
    remove(line) {
        this.lines = this.lines.filter((item) => item !== line);
        this.$dispatch('order-changed', { line });
    },
}));

document.addEventListener('order-changed', (event) => {
    console.log(event.detail);
});
";

/// A stylesheet: a class, a descendant pair, an id with a pseudo-class, and a
/// rule nested inside a media query.
const STYLES: &str = ".order-card { padding: 1rem; }
.order-card .total { font-weight: 700; }
#checkout-form input:focus { outline: none; }

@media (max-width: 600px) {
    .order-card { padding: 0.5rem; }
}
";

/// A fixture extracted and rendered as the text its snapshot holds.
fn extracted(extractor: &dyn Extractor, file_path: &str, source: &str) -> String {
    let project = ProjectId::new(PROJECT);
    let output = extractor.extract(&project, file_path, source);

    dump(&project, file_path, &output)
}

/// A fixture extracted twice, asserting the two runs agree before returning
/// the text to snapshot.
///
/// "Extraction is pure and per-file" is the invariant the whole pipeline rests
/// on, and a snapshot alone cannot see a violation of it: a parser that leaked
/// state between runs would still match on the first extraction of a fresh
/// process. Extracting twice through one extractor is what catches that, and
/// doing it here means every fixture checks it for free.
fn stable(extractor: &dyn Extractor, file_path: &str, source: &str) -> String {
    let first = extracted(extractor, file_path, source);
    let second = extracted(extractor, file_path, source);

    assert_eq!(first, second, "extracting {file_path} twice gave two different graphs");
    assert!(first.contains("nodes ("), "a dump of {file_path} carries a node section");

    first
}

#[test]
fn python_models_module() {
    let text = stable(&PythonExtractor::new(), "shop/orders/models.py", MODELS);

    insta::assert_snapshot!("python_models", text);
}

#[test]
fn python_views_module() {
    let text = stable(&PythonExtractor::new(), "shop/orders/views.py", VIEWS);

    insta::assert_snapshot!("python_views", text);
}

#[test]
fn python_urls_module() {
    let text = stable(&PythonExtractor::new(), "shop/orders/urls.py", URLS);

    insta::assert_snapshot!("python_urls", text);
}

#[test]
fn django_template() {
    let text = stable(&TemplateExtractor::new(), "shop/templates/orders/detail.html", TEMPLATE);

    insta::assert_snapshot!("django_template", text);
}

#[test]
fn javascript_component() {
    let text = stable(&JavaScriptExtractor::new(), "shop/static/order_card.js", COMPONENT);

    insta::assert_snapshot!("javascript_component", text);
}

#[test]
fn css_stylesheet() {
    let text = stable(&CssExtractor::new(), "shop/static/orders.css", STYLES);

    insta::assert_snapshot!("css_stylesheet", text);
}
