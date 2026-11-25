<?php

namespace App\Services;

use App\Models\Order;
use App\Models\OrderLine;

class OrderReport
{
    public function lines(): array
    {
        $out = [];
        $orders = Order::where('status', 'open')->get();
        foreach ($orders as $order) {
            $name = $order->customer->name;
            $count = OrderLine::where('order_id', $order->id)->count();
            $out[] = [$name, $count];
        }
        $eager = Order::with('customer')->orderBy('placed_at')->get();
        foreach ($eager as $o) {
            $out[] = $o->customer->name;
        }
        return $out;
    }
}
