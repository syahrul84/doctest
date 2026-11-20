<?php

namespace App\Services;

use App\Models\Order;
use Illuminate\Support\Facades\Mail;

class Slow
{
    public function run($user)
    {
        $open = Order::all()->where('status', 'open');
        if (Order::where('status', 'x')->count() > 0) {
            $first = Order::where('status', 'x')->get()[0];
        }
        Mail::to($user)->send(new \App\Mail\Done());
        $key = env('SHOP_KEY');
        sleep(2);
        $user->notify(new \App\Notifications\Queued());
        return $open;
    }
}
