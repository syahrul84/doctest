<?php

namespace App\Filament\Resources;

class AdOrderResource extends Resource
{
    public static function table(Table $table): Table
    {
        return $table->columns([
            TextColumn::make('banner_type'),
            TextColumn::make('starts_on')->label('Starts on date'),
            TextColumn::make('amount')->money('MYR'),
        ]);
    }
}
