/** 目录选择器的起点（挂载点或某个允许的保存目录） */
export interface DirRoot {
  id: string
  name: string
  path: string
}

/**
 * 选中结果。`rel` 是相对起点挂载点根的路径，设置页拿它拼 `setRoots` 入参；
 * 新建下载只用 `path`（后端 `resolve_dir` 接受白名单内的子目录）。
 */
export interface DirChoice {
  path: string
  mountId: string
  rel: string
}
