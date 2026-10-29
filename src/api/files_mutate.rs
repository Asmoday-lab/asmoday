//! 文件变更接口（设计文档 6.2 / 3.2.6）。
//!
//! ```text
//! POST /api/files/mkdir         { mount_id?, path, name }
//! POST /api/files/touch         { mount_id?, path, name }
//! POST /api/files/rename        { mount_id?, path, new_name }
//! POST /api/files/move          { mount_id?, paths[], dest_mount_id?, dest_path, on_conflict }
//! POST /api/files/copy          同上
//! POST /api/files/delete-token  { mount_id?, paths[] } → { token, expires_in }
//! POST /api/files/delete        { mount_id?, paths[], confirm_name? | confirm_token? }
//! GET  /api/files/stat?path=
//! POST /api/files/hash          { mount_id?, path }
//! ```
//!
//! 每个 handler 都走同一条流水线：
//! **解析挂载点 → 归属校验 → 只读拦截 → per-path 加锁 → spawn_blocking 干活 →
//!  元数据联动 → 逐项结果**。任何一步失败都按设计文档 6.6 的错误码返回。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::core::auth::now_secs;
use crate::core::fs::{self, FsError};
use crate::core::fs_ops::{self, ItemResult, NameBook, OnConflict};
use crate::core::path_guard::{self, MountEntry, MountSet};

/// 删除确认令牌有效期（设计文档 3.2.6.5）
pub const DELETE_TOKEN_TTL: Duration = Duration::from_secs(60);
/// 单次哈希的大小上限（设计文档 6.2）
pub const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

/// 一次性删除令牌（存内存，用后即删，重启丢失可接受）
#[derive(Debug, Clone)]
pub struct DeleteTicket {
    pub user_id: i64,
    pub mount_id: String,
    pub digest: u64,
    pub expires: Instant,
}

/// 令牌表：token → 票据
pub type DeleteTickets = Mutex<HashMap<String, DeleteTicket>>;

// ---------------------------------------------------------------------------
// 公共小工具
// ---------------------------------------------------------------------------

async fn mounts_of(state: &AppState) -> MountSet {
    let config = state.config.read().await;
    path_guard::load(&config, &state.cwd)
}

/// `[server.limits]` → 运行期闸门（`0` 表示不限）。
async fn limits_of(state: &AppState) -> fs_ops::BatchLimits {
    let config = state.config.read().await;
    let limits = &config.server.limits;
    fs_ops::BatchLimits {
        max_items: limits.batch_max_items,
        // 配置单位是 MiB，内部统一按字节比较
        max_bytes: limits.batch_max_bytes_mb.saturating_mul(1024 * 1024),
    }
}

/// 定位挂载点：优先 `mount_id`，否则按路径前缀反查，最后退化到默认挂载点
fn pick_mount<'a>(
    set: &'a MountSet,
    mount_id: Option<&str>,
    raw: &str,
) -> Result<&'a MountEntry, FsError> {
    if let Some(id) = mount_id.map(str::trim).filter(|id| !id.is_empty()) {
        return set
            .find(id)
            .ok_or_else(|| FsError::NotFound(format!("挂载点不存在：{id}")));
    }
    set.find_by_path_text(raw)
        .or_else(|| set.default_mount())
        .ok_or_else(|| FsError::NotFound("没有可用的挂载点".to_string()))
}

/// 阻塞任务包装：目录遍历 / 流式复制不能占着 async 执行器
async fn blocking<T, F>(work: F) -> Result<T, FsError>
where
    F: FnOnce() -> Result<T, FsError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(err) => Err(FsError::Io(format!("后台任务失败：{err}"))),
    }
}

fn ok_json(value: Value) -> Response {
    Json(value).into_response()
}

fn fail(err: FsError) -> Response {
    crate::api::error::fs_error(&err)
}

/// 拒绝把挂载点根目录本身当作操作对象
fn ensure_not_mount_root(path: &Path, roots: &[PathBuf]) -> Result<(), FsError> {
    if roots.iter().any(|root| root == path) {
        return Err(FsError::Forbidden(
            "不能对挂载点根目录执行该操作".to_string(),
        ));
    }
    Ok(())
}

/// 复制软链接前校验目标没逃出挂载点（设计文档 3.2.7）。
///
/// 直接复制链接本身会把这个「洞」搬到新位置，所以默认只允许指向挂载点内的链接。
/// `follow_symlinks = true` 时这个「洞」本来就是被允许的访问方式
/// （与浏览/读取口径一致），复制链接不再额外拦截，只拒绝解析不出的断链。
fn check_symlink_target(path: &Path, root: &Path, follow_symlinks: bool) -> Result<(), FsError> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !meta.file_type().is_symlink() {
        return Ok(());
    }
    let canon = std::fs::canonicalize(path)
        .map_err(|_| FsError::SymlinkEscape("软链接目标不存在或无法解析，拒绝复制".to_string()))?;
    if !follow_symlinks && !canon.starts_with(root) {
        return Err(FsError::SymlinkEscape(format!(
            "软链接指向挂载点外：{}",
            fs::normalize_display(&canon)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 新建 / 重命名
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    /// 父目录（绝对路径；省略时用挂载点根）
    #[serde(default)]
    pub path: String,
    pub name: String,
}

/// POST /api/files/mkdir
pub async fn mkdir(State(state): State<AppState>, Json(body): Json<CreateBody>) -> Response {
    match create_inner(&state, body, true).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

/// POST /api/files/touch
pub async fn touch(State(state): State<AppState>, Json(body): Json<CreateBody>) -> Response {
    match create_inner(&state, body, false).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn create_inner(state: &AppState, body: CreateBody, is_dir: bool) -> Result<Value, FsError> {
    let set = mounts_of(state).await;
    let mount = pick_mount(&set, body.mount_id.as_deref(), &body.path)?;
    mount.ensure_writable()?;

    let target = path_guard::resolve_child(mount, &body.path, &body.name)?;
    let parent = target
        .parent()
        .ok_or_else(|| FsError::BadRequest("父目录缺失".to_string()))?
        .to_path_buf();
    let name = body.name.clone();

    let _guard = state.path_locks.acquire(std::slice::from_ref(&target))?;
    let created = blocking(move || {
        if is_dir {
            fs_ops::mkdir(&parent, &name)
        } else {
            fs_ops::touch(&parent, &name)
        }
    })
    .await?;

    Ok(json!({
        "ok": true,
        "path": fs::normalize_display(&created),
        "is_dir": is_dir,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RenameBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub path: String,
    pub new_name: String,
}

/// POST /api/files/rename
pub async fn rename(State(state): State<AppState>, Json(body): Json<RenameBody>) -> Response {
    match rename_inner(&state, body).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn rename_inner(state: &AppState, body: RenameBody) -> Result<Value, FsError> {
    let set = mounts_of(state).await;
    let mount = pick_mount(&set, body.mount_id.as_deref(), &body.path)?;
    mount.ensure_writable()?;

    // 最后一段不跟随：重命名软链接改的是链接的名字，不是目标实体的名字
    // （canonicalize 会把链接解析成目标，改名后链接原地失效，目标被搬走）
    let source = path_guard::resolve_existing_nofollow(mount, &body.path)?;
    let roots: Vec<PathBuf> = set.mounts.iter().filter_map(|m| m.root.clone()).collect();
    ensure_not_mount_root(&source, &roots)?;

    path_guard::validate_name(&body.new_name)?;
    let guess = source
        .parent()
        .ok_or_else(|| FsError::BadRequest("路径没有父目录".to_string()))?
        .join(&body.new_name);

    let _guard = state.path_locks.acquire(&[source.clone(), guess.clone()])?;

    let old_display = fs::normalize_display(&source);
    let rename_to = body.new_name.clone();
    let source_for_task = source.clone();
    let target = blocking(move || fs_ops::rename_entry(&source_for_task, &rename_to)).await?;
    let new_display = fs::normalize_display(&target);

    // 数据库联动：失败只记 ERROR，不回滚文件系统（设计文档 3.2.6.7）
    if old_display != new_display
        && let Err(err) = state.auth.rewrite_meta_paths(&old_display, &new_display)
    {
        crate::log_error!("failed to sync metadata after rename: {err}");
    }
    // 旧路径的缩略图缓存按路径寻址，改名后必然命中不了：直接清掉，
    // 新路径首次预览时重生成（FileBrowser 在 rename 后同样销毁缓存）
    if old_display != new_display {
        purge_thumb_cache(state, &old_display);
    }

    Ok(json!({
        "ok": true,
        "path": old_display,
        "new_path": new_display,
    }))
}

// ---------------------------------------------------------------------------
// 移动 / 复制
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TransferBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub paths: Vec<String>,
    #[serde(default)]
    pub dest_mount_id: Option<String>,
    pub dest_path: String,
    #[serde(default)]
    pub on_conflict: OnConflict,
}

/// POST /api/files/move
pub async fn move_entries(
    State(state): State<AppState>,
    Json(body): Json<TransferBody>,
) -> Response {
    match transfer_inner(&state, body, true).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

/// POST /api/files/copy
pub async fn copy_entries(
    State(state): State<AppState>,
    Json(body): Json<TransferBody>,
) -> Response {
    match transfer_inner(&state, body, false).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn transfer_inner(
    state: &AppState,
    body: TransferBody,
    is_move: bool,
) -> Result<Value, FsError> {
    if body.paths.is_empty() {
        return Err(FsError::BadRequest("paths 不能为空".to_string()));
    }
    let limits = limits_of(state).await;
    if limits.max_items > 0 && body.paths.len() > limits.max_items {
        return Err(FsError::TooMany(format!(
            "单次最多 {} 项",
            limits.max_items
        )));
    }

    let set = mounts_of(state).await;

    // ---- 源 ----
    let mut sources = Vec::with_capacity(body.paths.len());
    // 每个源所属挂载点的 canonical_root：软链接逃逸校验要用（设计文档 3.2.7）
    let mut src_roots: Vec<PathBuf> = Vec::with_capacity(body.paths.len());
    // 每个源所属挂载点的 follow_symlinks（复制软链接时的放行口径）
    let mut src_follow: Vec<bool> = Vec::with_capacity(body.paths.len());
    // 逃逸的软链接只失败该项，不中断整批（设计文档 3.2.7「记失败项」）
    let mut pre_results: Vec<ItemResult> = Vec::new();
    for raw in &body.paths {
        let mount = pick_mount(&set, body.mount_id.as_deref(), raw)?;
        mount.ensure_writable()?;
        let root = mount.canon_root()?.clone();
        // 最后一段不跟随：移动/复制软链接操作的是链接本身（fs_ops 的
        // move_entry/copy_entry 都按 lstat 分派），canonicalize 会把链接
        // 解析成目标实体，导致「移链接」变成「搬目标目录」。
        // 逃出挂载点的链接由下方 check_symlink_target（复制）逐项拦截。
        let path = match path_guard::resolve_existing_nofollow(mount, raw) {
            Ok(path) => path,
            Err(err @ FsError::SymlinkEscape(_)) => {
                pre_results.push(ItemResult::failed(Path::new(raw.trim()), &err));
                continue;
            }
            Err(err) => return Err(err),
        };
        let roots: Vec<PathBuf> = set.mounts.iter().filter_map(|m| m.root.clone()).collect();
        ensure_not_mount_root(&path, &roots)?;
        sources.push(path);
        src_roots.push(root);
        src_follow.push(mount.info.follow_symlinks);
    }
    if sources.is_empty() {
        let summary = fs_ops::summarize(&pre_results);
        return Ok(json!({ "results": pre_results, "summary": summary }));
    }

    // ---- 目标目录 ----
    let dest_mount = pick_mount(
        &set,
        body.dest_mount_id.as_deref().or(body.mount_id.as_deref()),
        &body.dest_path,
    )?;
    dest_mount.ensure_writable()?;
    let dest_dir = path_guard::resolve_existing(dest_mount, &body.dest_path)?;
    if !dest_dir.is_dir() {
        return Err(FsError::BadRequest("目标不是目录".to_string()));
    }
    let dest_root = dest_mount.canon_root()?.clone();

    // 防目录套娃：不能把目录移动到它自己的子孙里
    for source in &sources {
        if source.is_dir() && (source == &dest_dir || dest_dir.starts_with(source)) {
            return Err(FsError::BadRequest(format!(
                "目标目录位于源目录内部：{}",
                fs::normalize_display(source)
            )));
        }
    }

    // ---- 上限统计（操作开始前完成，遍历中一旦超限立即中止）----
    //
    // 同挂载点移动走原子 rename：不搬数据、不占新空间，统计整棵树纯属浪费
    // （机械盘上搬一个十万文件的目录，光统计就要几十秒），因此直接跳过。
    let cross_mount = sources.iter().any(|source| !source.starts_with(&dest_root));
    let needs_bytes = !is_move || cross_mount;

    // 先做浅层统计：选中 300 项时一秒内就能拒掉，不用等整棵树遍历完
    let mut total_bytes = fs_ops::measure_shallow(&sources, limits)?.1;
    // 真正要复制时才递归累加字节（删除完全不走这条路）
    if needs_bytes && !limits.unlimited() {
        total_bytes = fs_ops::measure(&sources, limits)?.1;
    }

    // ---- 分享记录冲突预检查（设计文档 3.2.6.7）----
    // 覆盖式移动会改写既有分享记录；若目标位置本来就有分享，改写后无法判断
    // 哪条记录的 allow_download / 密码该胜出，因此直接拒绝本次移动。
    if is_move && body.on_conflict == OnConflict::Overwrite {
        for source in &sources {
            let Some(name) = source.file_name() else {
                continue;
            };
            let tentative = dest_dir.join(name);
            if !tentative.exists() {
                continue;
            }
            let display = fs::normalize_display(&tentative);
            let taken = state.auth.share_exists_at(&display).unwrap_or(false);
            if taken {
                return Err(FsError::Conflict(format!(
                    "目标位置已存在分享记录，无法覆盖移动：{display}"
                )));
            }
        }
    }

    // ---- 空间检查 ----
    if needs_bytes {
        fs_ops::ensure_space(&dest_dir, total_bytes)?;
    }

    // ---- 加锁：所有源 + 目标目录 ----
    let mut lock_paths = sources.clone();
    lock_paths.push(dest_dir.clone());
    let _guard = state.path_locks.acquire(&lock_paths)?;

    // ---- 干活 ----
    let conflict = body.on_conflict;
    let work_sources = sources.clone();
    let work_roots = src_roots.clone();
    let work_follow = src_follow.clone();
    let work_dest = dest_dir.clone();
    let work_root = dest_root.clone();
    let results = blocking(move || {
        let mut book = NameBook::new(&work_dest);
        let mut results: Vec<ItemResult> = Vec::with_capacity(work_sources.len());
        for ((source, root), follow) in work_sources
            .iter()
            .zip(work_roots.iter())
            .zip(work_follow.iter())
        {
            // 复制的软链接默认必须指向挂载点内：逃逸的链接拒绝复制该项，不中断整批
            if !is_move && let Err(err) = check_symlink_target(source, root, *follow) {
                results.push(ItemResult::failed(source, &err));
                continue;
            }
            let same_mount = source.starts_with(&work_root);
            let outcome = if is_move {
                fs_ops::move_entry(source, &work_dest, &mut book, conflict, same_mount)
            } else {
                fs_ops::copy_entry(source, &work_dest, &mut book, conflict)
            };
            match outcome {
                Ok(Some(target)) => {
                    let mut item = ItemResult::ok(source, &target);
                    if is_move {
                        item.source = Some(fs::normalize_display(source));
                    }
                    results.push(item);
                }
                Ok(None) => results.push(ItemResult::skipped(source, "skipped by conflict policy")),
                Err(err) => results.push(ItemResult::failed(source, &err)),
            }
        }
        Ok(results)
    })
    .await?;

    // ---- 元数据联动（仅移动）----
    if is_move {
        for item in results.iter().filter(|item| item.ok) {
            // 源路径变了：旧路径的缩略图缓存再也命中不了，直接清掉
            purge_thumb_cache(state, &item.path);
            let Some(new_path) = item.new_path.clone() else {
                continue;
            };
            if Path::new(&item.path).starts_with(&dest_root) {
                if let Err(err) = state.auth.rewrite_meta_paths(&item.path, &new_path) {
                    crate::log_error!("failed to sync metadata after move: {err}");
                }
            } else {
                // 跨挂载点移动 = 「新路径写入 + 旧路径删除」：播放进度与收藏不保留，
                // 旧路径上的分享转成 tombstone（访问时 410，设计文档 3.2.6.7）
                if let Err(err) = state.auth.forget_meta_paths(&item.path, now_secs()) {
                    crate::log_error!("failed to purge metadata after cross-mount move: {err}");
                }
            }
        }
    }

    // 解析阶段就失败的项（逃逸软链接）与真正干活的项合并成一份逐项结果
    let mut all = pre_results;
    all.extend(results);
    let summary = fs_ops::summarize(&all);
    Ok(json!({
        "results": all,
        "summary": summary,
    }))
}

/// 清掉某个路径（display 写法）的缩略图缓存与 ThumbHash 行。
///
/// 缓存键 = `normalize_display(path)` 的 sha1，所以传列表里的展示路径即可，
/// 不需要先 canonicalize（删除后源文件已不存在，也 canonicalize 不了）。
fn purge_thumb_cache(state: &AppState, display_path: &str) {
    let path = Path::new(display_path);
    state.thumbnails.purge(path);
    let key = crate::core::thumbnail::Thumbnailer::hash_key(path);
    state.auth.thumb_hashes_forget(&[key]);
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeleteTokenBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub paths: Vec<String>,
}

fn digest_paths(paths: &[String]) -> u64 {
    let mut sorted: Vec<String> = paths
        .iter()
        .map(|p| p.replace('\\', "/").trim_end_matches('/').to_lowercase())
        .collect();
    sorted.sort();
    sorted.dedup();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

/// POST /api/files/delete-token
///
/// 批量删除必须先取一次性令牌：绑定「用户 + 挂载点 + paths 集合哈希」，60 秒有效，
/// 用后即删。它不是安全边界（已登录的恶意者当然能自己取令牌），而是**防误操作**：
/// 脚本漏掉确认步骤时删不掉东西。
pub async fn delete_token(
    State(state): State<AppState>,
    Json(body): Json<DeleteTokenBody>,
) -> Response {
    if body.paths.is_empty() {
        return fail(FsError::BadRequest("paths 不能为空".to_string()));
    }
    let set = mounts_of(&state).await;
    let mut mount_id = body.mount_id.clone().unwrap_or_default();
    for raw in &body.paths {
        let Some(mount) = pick_mount(&set, body.mount_id.as_deref(), raw).ok() else {
            return fail(FsError::NotFound(format!("挂载点不存在：{raw}")));
        };
        // 与 delete 同口径用 nofollow：逃出挂载点的链接也允许删（校验仅确认
        // 「链接本身在挂载点内」，不跟随目标）
        if let Err(err) = path_guard::resolve_existing_nofollow(mount, raw) {
            return fail(err);
        }
        if mount_id.is_empty() {
            mount_id = mount.info.id.clone();
        }
    }

    let token = crate::core::auth::random_token()[..24].to_string();
    let user_id = 0;
    let ticket = DeleteTicket {
        user_id,
        mount_id,
        digest: digest_paths(&body.paths),
        expires: Instant::now() + DELETE_TOKEN_TTL,
    };
    if let Ok(mut tickets) = state.delete_tickets.lock() {
        // 顺手清掉过期票据，避免长期运行后内存里堆垃圾
        tickets.retain(|_, item| item.expires > Instant::now());
        tickets.insert(token.clone(), ticket);
    }

    ok_json(json!({
        "token": token,
        "expires_in": DELETE_TOKEN_TTL.as_secs(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct DeleteBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub paths: Vec<String>,
    /// 删除单个目录时要求用户输入目录名
    #[serde(default)]
    pub confirm_name: Option<String>,
    /// 批量删除时的一次性令牌
    #[serde(default)]
    pub confirm_token: Option<String>,
}

/// POST /api/files/delete
pub async fn delete(State(state): State<AppState>, Json(body): Json<DeleteBody>) -> Response {
    match delete_inner(&state, body).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn delete_inner(state: &AppState, body: DeleteBody) -> Result<Value, FsError> {
    if body.paths.is_empty() {
        return Err(FsError::BadRequest("paths 不能为空".to_string()));
    }

    let set = mounts_of(state).await;
    let mut targets = Vec::with_capacity(body.paths.len());
    for raw in &body.paths {
        let mount = pick_mount(&set, body.mount_id.as_deref(), raw)?;
        mount.ensure_writable()?;
        // 最后一段不跟随：删软链接只删链接本身，一旦 canonicalize 就会把
        // 目标实体解析出来，remove_dir_all 直接清掉目标目录（设计文档 3.2.7 红线）。
        // 逃出挂载点的链接也允许删——它只是个链接，删掉反而消除隐患。
        let path = path_guard::resolve_existing_nofollow(mount, raw)?;
        let roots: Vec<PathBuf> = set.mounts.iter().filter_map(|m| m.root.clone()).collect();
        ensure_not_mount_root(&path, &roots)?;
        targets.push(path);
    }

    // ---- 确认环节 ----
    // 目录判定用 lstat：软链接不跟随，指向目录的链接按链接处理，
    // 不触发「输入目录名确认」流程（前端同样按链接渲染删除确认）
    let batch = targets.len() > 1;
    let has_dir = targets.iter().any(|path| {
        std::fs::symlink_metadata(path)
            .map(|meta| meta.is_dir() && !meta.file_type().is_symlink())
            .unwrap_or(false)
    });
    if batch || has_dir {
        let token_ok = body
            .confirm_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(|token| consume_delete_token(state, token, &body.paths))
            .transpose()?;
        if token_ok != Some(true) {
            // 单目录删除允许用 confirm_name 代替批量令牌
            if !batch {
                let expected = targets[0]
                    .file_name()
                    .map(|name| name.to_string_lossy().trim().to_string())
                    .unwrap_or_default();
                let given = body
                    .confirm_name
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string();
                if given != expected {
                    return Err(FsError::NameMismatch(format!(
                        "确认名称不匹配：需要输入「{expected}」"
                    )));
                }
            } else {
                return Err(FsError::TokenMismatch(
                    "批量删除必须先获取 delete-token".to_string(),
                ));
            }
        }
    }

    // 删除只校验「本批选中了多少项」，不再统计目录内部的项数与字节：
    // remove_dir_all 是 O(1) 起步的元数据操作，为了拒绝它先扫完整棵树
    // （几万文件要几十秒）毫无意义，反而让「删一个相册目录」变成不可能。
    let limits = limits_of(state).await;
    if limits.max_items > 0 && targets.len() > limits.max_items {
        return Err(FsError::TooMany(format!(
            "单次最多删除 {} 项，请分批进行",
            limits.max_items
        )));
    }

    let _guard = state.path_locks.acquire(&targets)?;
    let work = targets.clone();
    let results = blocking(move || {
        let mut results = Vec::with_capacity(work.len());
        for path in &work {
            match fs_ops::remove_path(path) {
                Ok(()) => results.push(ItemResult::ok(path, path)),
                Err(err) => results.push(ItemResult::failed(path, &err)),
            }
        }
        Ok(results)
    })
    .await?;

    let now = now_secs();
    for item in results.iter().filter(|item| item.ok) {
        // 缩略图缓存与色值跟着源文件一起清（FileBrowser 的 delThumbs 同语义），
        // 否则被删的图会永远留一份缓存，只能等 LRU 慢慢淘汰。
        purge_thumb_cache(state, &item.path);
        if let Err(err) = state.auth.forget_meta_paths(&item.path, now) {
            crate::log_error!("failed to purge metadata after delete: {err}");
        }
    }

    let summary = fs_ops::summarize(&results);
    Ok(json!({ "results": results, "summary": summary }))
}

/// 校验并消费一次性令牌；返回是否有效
fn consume_delete_token(state: &AppState, token: &str, paths: &[String]) -> Result<bool, FsError> {
    let Ok(mut tickets) = state.delete_tickets.lock() else {
        return Err(FsError::Io("ticket store poisoned".to_string()));
    };
    let Some(ticket) = tickets.remove(token) else {
        return Err(FsError::TokenMismatch("删除令牌无效或已被使用".to_string()));
    };
    if ticket.expires <= Instant::now() {
        return Err(FsError::TokenExpired("删除令牌已过期".to_string()));
    }
    if ticket.digest != digest_paths(paths) {
        return Err(FsError::TokenMismatch(
            "删除令牌与本次 paths 不匹配".to_string(),
        ));
    }
    let _ = ticket.user_id;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 删除前统计 / 属性 / 哈希
// ---------------------------------------------------------------------------

/// 删除确认弹窗的统计上限：数到这里就够了（设计文档 3.2.6.5「超过 1000 项提示」）
const SURVEY_CAP: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct MeasureBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub paths: Vec<String>,
}

/// POST /api/files/measure：只读统计「将删除多少项、共多大」
///
/// 统计本身不落任何改动，因此不加 per-path 锁；后端仍会做挂载点归属校验。
pub async fn measure(State(state): State<AppState>, Json(body): Json<MeasureBody>) -> Response {
    match measure_inner(&state, body).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn measure_inner(state: &AppState, body: MeasureBody) -> Result<Value, FsError> {
    if body.paths.is_empty() {
        return Err(FsError::BadRequest("paths 不能为空".to_string()));
    }
    let limits = limits_of(state).await;
    if limits.max_items > 0 && body.paths.len() > limits.max_items {
        return Err(FsError::TooMany(format!(
            "单次最多 {} 项",
            limits.max_items
        )));
    }

    let set = mounts_of(state).await;
    let mut targets = Vec::with_capacity(body.paths.len());
    for raw in &body.paths {
        let mount = pick_mount(&set, body.mount_id.as_deref(), raw)?;
        targets.push(path_guard::resolve_existing(mount, raw)?);
    }

    let work = targets;
    let (items, bytes, truncated) =
        blocking(move || Ok::<_, FsError>(fs_ops::survey(&work, SURVEY_CAP))).await?;
    Ok(json!({
        "items": items,
        "bytes": bytes,
        "truncated": truncated,
    }))
}

// ---------------------------------------------------------------------------
// 属性 / 哈希
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StatQuery {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub path: String,
}

/// GET /api/files/stat：只读属性（不跟随软链接）
pub async fn stat(State(state): State<AppState>, Query(query): Query<StatQuery>) -> Response {
    let set = mounts_of(&state).await;
    let mount = match pick_mount(&set, query.mount_id.as_deref(), &query.path) {
        Ok(mount) => mount,
        Err(err) => return fail(err),
    };
    // 不跟随最后一段：软链接要按 lstat 显示（设计文档 3.2.7）
    let path = match path_guard::resolve_existing_nofollow(mount, &query.path) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let link_meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) => return fail(FsError::Io(format!("读取属性失败：{err}"))),
    };

    let is_symlink = link_meta.file_type().is_symlink();
    // 真实路径：软链接给出解析后的目标，普通文件就是自己（只给 WebDAV/内部逻辑用，属性页不展示）
    let real_path = std::fs::canonicalize(&path)
        .ok()
        .map(|p| fs::normalize_display(&p));
    let readonly = set.is_readonly(&path);

    let mime = if link_meta.is_dir() {
        "inode/directory".to_string()
    } else {
        mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string()
    };

    // 媒体分辨率（v2.9）：图片读文件头（不解码），视频交给外部 FFmpeg 探测；
    // 拿不到就返回 null，前端把这一行整个隐藏，不画「—」占位。
    let dimensions = if link_meta.is_file() {
        if mime.starts_with("image/") {
            let probe_path = path.clone();
            blocking(move || Ok::<_, FsError>(image::image_dimensions(&probe_path).ok()))
                .await
                .ok()
                .flatten()
        } else if mime.starts_with("video/") {
            let configured = crate::api::files::config_snapshot(&state)
                .await
                .media
                .ffmpeg_path;
            let bin = crate::core::ffmpeg::resolve(&configured).path;
            match bin {
                Some(bin) => {
                    let probe_path = path.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::core::ffmpeg::probe_video_size(&bin, &probe_path)
                    })
                    .await
                    .ok()
                    .flatten()
                }
                None => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    let (width, height) = dimensions.map_or((None, None), |(w, h)| (Some(w), Some(h)));

    let body = json!({
        "path": fs::normalize_display(&path),
        "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "is_dir": link_meta.is_dir(),
        "is_symlink": is_symlink,
        "symlink_target": if is_symlink {
            std::fs::read_link(&path).ok().map(|p| fs::normalize_display(&p))
        } else {
            None
        },
        // 软链接不 stat 目标：属性页只展示链接本身的 size / mtime（设计文档 3.2.7）
        "size": link_meta.len(),
        "mtime": link_meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        "mime": mime,
        // 真实物理路径（canonicalize 结果）。**仅限控制台会话**：本路由在会话
        // 中间件之后，分享访客走 /api/public/share/*，到不了这里（设计文档 3.2.5）。
        "real_path": real_path,
        "real_absolute_path": real_path,
        "readonly": readonly,
        "width": width,
        "height": height,
        "mode": mode_bits(&link_meta),
        "inode": inode_of(&link_meta),
        "owner": owner_of(&link_meta),
    });
    ok_json(body)
}

#[cfg(unix)]
fn mode_bits(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.mode())
}

#[cfg(not(unix))]
fn mode_bits(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn owner_of(meta: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", meta.uid(), meta.gid()))
}

#[cfg(not(unix))]
fn owner_of(_meta: &std::fs::Metadata) -> Option<String> {
    None
}

#[derive(Debug, Deserialize)]
pub struct HashBody {
    #[serde(default)]
    pub mount_id: Option<String>,
    pub path: String,
}

/// POST /api/files/hash：仅对 < 64 MiB 文件按需计算，并发上限 2
pub async fn hash(State(state): State<AppState>, Json(body): Json<HashBody>) -> Response {
    match hash_inner(&state, body).await {
        Ok(value) => ok_json(value),
        Err(err) => fail(err),
    }
}

async fn hash_inner(state: &AppState, body: HashBody) -> Result<Value, FsError> {
    let set = mounts_of(state).await;
    let mount = pick_mount(&set, body.mount_id.as_deref(), &body.path)?;
    let path = path_guard::resolve_existing(mount, &body.path)?;

    let meta =
        std::fs::metadata(&path).map_err(|err| FsError::Io(format!("读取文件信息失败：{err}")))?;
    if meta.is_dir() {
        return Err(FsError::BadRequest("目录不支持哈希".to_string()));
    }
    if meta.len() > MAX_HASH_BYTES {
        return Err(FsError::TooLarge(format!(
            "超过 {} MiB 的文件不支持哈希",
            MAX_HASH_BYTES / 1_048_576
        )));
    }

    // 并发上限 2：拿不到槽位立即 E_BUSY，不排队（低功耗设备不能被哈希拖死）
    let Ok(_permit) = state.hash_slots.clone().try_acquire_owned() else {
        return Err(FsError::Busy("哈希任务已达并发上限".to_string()));
    };

    let display = fs::normalize_display(&path);
    let digest = blocking(move || hash_file(&path)).await?;
    Ok(json!({
        "path": display,
        "algorithm": "sha1",
        "hash": digest,
        "size": meta.len(),
    }))
}

fn hash_file(path: &Path) -> Result<String, FsError> {
    use sha1::{Digest, Sha1};
    use std::io::Read as _;

    let mut file =
        std::fs::File::open(path).map_err(|err| FsError::Io(format!("打开文件失败：{err}")))?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| FsError::Io(format!("读取文件失败：{err}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
