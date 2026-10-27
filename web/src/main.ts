import { createApp } from 'vue'
import './style.css'
import App from './App.vue'
import router from './router'
import { installBaseUrlShim } from './lib/base'
import { i18n } from './lib/i18n'

// 子路径部署：先给 fetch 打补丁，再启动应用（router 自己读 APP_BASE）
installBaseUrlShim()

createApp(App).use(router).use(i18n).mount('#app')
