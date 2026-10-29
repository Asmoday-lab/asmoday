import { createApp } from 'vue'
import './style.css'
import App from './App.vue'
import router from './router'
import { installBaseUrlShim } from './lib/base'
import { i18n } from './lib/i18n'
import { pinia } from './stores/pinia'

// 子路径部署：先给 fetch 打补丁，再启动应用（router 自己读 APP_BASE）
installBaseUrlShim()

// pinia 放在 router 之前：路由守卫里会读 store 状态
createApp(App).use(pinia).use(router).use(i18n).mount('#app')
