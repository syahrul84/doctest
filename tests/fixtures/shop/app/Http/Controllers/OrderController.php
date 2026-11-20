<?php

namespace App\Http\Controllers;

use Illuminate\Http\Request;

class OrderController extends Controller
{
    public function store(Request $request, $order)
    {
        $request->validate([
            'email' => 'required|email|max:255',
            'qty' => ['required', 'integer', 'min:1'],
        ]);
        $row = ['status' => 'new'];
        if ($order === 'closed') {
            return response()->json(['message' => 'Order is already closed.'], 422);
        }
        return response()->json(['message' => 'Order saved.']);
    }
}
