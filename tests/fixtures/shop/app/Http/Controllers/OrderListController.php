<?php

namespace App\Http\Controllers;

use App\Models\Order;

class OrderListController extends Controller
{
    public function index()
    {
        return response()->json(Order::where('status', 'open')->get());
    }
}
