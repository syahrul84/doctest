import api from './client'
export function useOrderForm() { return { form: {}, save: () => api.post('/orders/1') } }
export function deleteEverything() {
  return api.delete('/everything')
}
