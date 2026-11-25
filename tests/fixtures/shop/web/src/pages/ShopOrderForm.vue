<template>
  <div>
    <h1>{{ $t('message.newOrder') }}</h1>
    <OrderCustomer />
    <v-text-field v-model="form.remarks" label="Remarks" />
    <v-btn color="primary" @click="save">Save</v-btn>
    <button type="button" title="Delete order">X</button>
  </div>
</template>
<script setup>
import OrderCustomer from '@/components/OrderCustomer.vue'
import { useOrderForm } from '@/api/orders'
const { form, save } = useOrderForm()
</script>
