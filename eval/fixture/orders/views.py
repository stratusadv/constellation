from django.shortcuts import render

from orders.models import Order


def checkout_view(request):
    order = Order()
    order.recalculate_totals()
    return render(request, 'orders/checkout.html', {'order': order})
