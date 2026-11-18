<?php

namespace Tests\Feature\Http\Controllers;

use Tests\TestCase;

class OrderControllerTest extends TestCase
{
    public function test_order_needs_an_email(): void
    {
        $user = $this->admin();
        $response = $this->actingAs($user)->postJson('/api/orders/' . $this->order->id, ['qty' => 1]);
        $response->assertStatus(422)->assertJsonValidationErrors(['email']);
    }

    public function test_order_saves_through_named_route(): void
    {
        $this->actingAs($this->admin())->postJson(route('orders.store', 5), ['email' => 'a@b.c', 'qty' => 2])->assertOk();
    }

    public function test_unrelated(): void
    {
        $this->assertTrue(true);
    }

    // <laravel-parity> generated
    /** POST api/orders/{order} */
    /**
     * @dataProvider cases_post_api_orders_order
     * @group laravel-parity
     */
    public function test_post_api_orders_order(bool $signIn, bool $missing, array $body, array $query, array $without, int $status, array $structure, array $errors): void
    {
    }

    public static function cases_post_api_orders_order(): array
    {
        return [
            // case => [sign in, missing record, body, query, fields left out, status, JSON keys, validation errors]
            'guest' => [false, false, [], [], [], 401, [], []],
            'empty' => [true, false, [], [], [], 422, [], ['email', 'qty']],
        ];
    }
    // </laravel-parity>
}
