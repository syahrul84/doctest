<template>
  <ul>
    <li v-for="o in orders" v-if="o.open">{{ o.id }}</li>
    <li v-for="o in orders" :key="o.id">{{ o.id }}</li>
  </ul>
</template>
<script setup>
import _ from 'lodash'
import api from '@/api/client'
async function loadAll(ids) {
  for (const id of ids) {
    await api.get(`/orders/${id}`)
  }
  console.log('done')
}
</script>
