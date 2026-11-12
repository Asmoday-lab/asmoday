/**
 * 生态推荐清单（设计文档 19）。
 *
 * 纯前端常量：不走 API、断网可用，随前端打包发布。
 * 文案一律用 i18n key，名称是专有名词所以直接写死。
 * 该页面「不可修改」——用户不能增删条目，更新随版本走。
 *
 * v4.9.73 重组：一张卡只放一个产品、外链各自指向官网；
 * 删掉与内置下载引擎重叠的 qBittorrent / Aria2（引擎已兼容 aria2 RPC），
 * 改荐 AriaNg 前端；新增「WebDAV 客户端」分类（令牌体系的直接消费入口）。
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
  id: 'sync' | 'download' | 'webdav' | 'tunnel' | 'writing'
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
      },
      {
        id: 'foldersync',
        name: 'FolderSync',
        url: 'https://foldersync.io',
        icon: 'folder',
      },
    ],
  },
  {
    id: 'download',
    items: [
      {
        id: 'ariang',
        name: 'AriaNg',
        url: 'https://ariang.mayswind.net',
        icon: 'rows',
      },
    ],
  },
  {
    id: 'webdav',
    items: [
      {
        id: 'raidrive',
        name: 'RaiDrive',
        url: 'https://www.raidrive.com',
        icon: 'folderOpen',
      },
      {
        id: 'cyberduck',
        name: 'Cyberduck',
        url: 'https://cyberduck.io',
        icon: 'file',
      },
      {
        id: 'winscp',
        name: 'WinSCP',
        url: 'https://winscp.net',
        icon: 'copy',
      },
    ],
  },
  {
    id: 'tunnel',
    items: [
      {
        id: 'caddy',
        name: 'Caddy',
        url: 'https://caddyserver.com',
        icon: 'globe',
      },
      {
        id: 'tailscale',
        name: 'Tailscale',
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
        name: 'PicGo',
        url: 'https://picgo.github.io/PicGo-Doc/',
        icon: 'upload',
      },
      {
        id: 'typora',
        name: 'Typora',
        url: 'https://typora.io',
        icon: 'fileText',
      },
      {
        id: 'obsidian',
        name: 'Obsidian',
        url: 'https://obsidian.md',
        icon: 'edit',
      },
    ],
  },
]
