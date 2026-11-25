export interface MenuItem {
  label: string
  routeName?: string
}
export const menuGroups = {
  orders: [
    {
      label: 'Orders',
      items: [
        { label: 'New Order ', aro: 'orders/create', routeName: 'shop.orders.create' },
        { label: 'Order List', aro: 'orders/list', routeName: 'shop.orders.index' },
      ],
    },
  ],
}
