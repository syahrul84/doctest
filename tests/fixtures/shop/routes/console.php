<?php
use Illuminate\Support\Facades\Schedule;
Schedule::command('orders:close')->daily();
