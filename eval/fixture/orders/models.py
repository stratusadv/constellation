from django.db import models


class Order(models.Model):
    total = models.DecimalField(max_digits=10, decimal_places=2)

    def recalculate_totals(self):
        return self.total
