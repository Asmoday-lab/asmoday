/**
 * 生态推荐清单（设计文档 10.2）。
 *
 * 纯前端常量：不走 API、断网可用，随前端打包发布（10.1 第 5 条）。
 * 文案一律用 i18n key，名称是专有名词所以直接写死。
 * 该页面「不可修改」——用户不能增删条目，更新随版本走（10.3）。
 */

import type { IconName } from '../components/icons'

export interface EcosystemItem {
  /** 唯一 id，同时作为 i18n key：`ecosystem.items.<id>` */
  id: string
  name: string
  /** 官方站点；必须 `https`，外链统一 target=_blank + rel=noopener noreferrer */
  url: string
  icon: IconName
}

export interface EcosystemCategory {
  /** i18n key：`ecosystem.categories.<id>` */
  id: 'sync' | 'download' | 'tunnel' | 'writing'
  items: EcosystemItem[]
}

export const ECOSYSTEM: EcosystemCategory[] = [
  {
    id: 'sync',
    items: [
      {
        id: 'syncthing',
        name: 'Syncthing',
        url: 'https://syncthing.net',
        icon: 'refresh',
      },
      {
        id: 'photosync',
        name: 'PhotoSync',
        url: 'https://www.photosync-app.com',
        icon: 'image',
      },      {
        id: 'foldersync',
        name: 'FolderSync',
        url: 'https://play.google.com/store/apps/details?id=dk.tacit.android.foldersync.lite',
        icon: 'folder',
      },
    ],
  },
  {
    id: 'download',
    items: [
      {
        id: 'qbittorrent',
        name: 'qBittorrent',
        url: 'https://www.qbittorrent.org',
        icon: 'download',
      },
      {
        id: 'aria2',
        name: 'Aria2',
        url: 'https://aria2.github.io',
        icon: 'download',
      },
    ],
  },
  {
    id: 'tunnel',
    items: [
      {
        id: 'caddy',
        name: 'Caddy / Nginx',
        url: 'https://caddyserver.com',
        icon: 'globe',
      },
      {
        id: 'tailscale',
        name: 'Tailscale / ZeroTier',
        url: 'https://tailscale.com',
        icon: 'link',
      },
    ],
  },
  {
    id: 'writing',
    items: [
      {
        id: 'picgo',
        name: 'PicGo / Typora / Obsidian',
        url: 'https://picgo.github.io/PicGo-Doc/',
        icon: 'edit',
      },
    ],
  },
]
