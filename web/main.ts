import { createApp } from "vue";
import { createPinia } from "pinia";
import { createRouter, createWebHistory } from "vue-router";
import App from "./App.vue";
import "./style.css";
const router = createRouter({
  history: createWebHistory(),
  routes: [{ path: "/:section?", component: { template: "<div />" } }],
});
createApp(App).use(createPinia()).use(router).mount("#app");
