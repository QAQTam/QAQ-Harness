//! WorkspaceStore — 会话工作区注册表（组织语义）。
//!
//! 与「运行环境 workspace」（`workspace.set` local/wsl/remote）解耦：本模块只负责
//! 把会话按目录归类，持久化到 `{data_dir}/workspaces.json`。
//!
//! 设计对齐 deepseek-harness `packages/workspace/workspace/src/types.ts`：
//! - id 用生成串（非路径——路径会被规范化/重命名，锚点必须稳定）；
//! - 归属 = 显式账户（`session_ids`）+ cwd 匹配自动 attach 双轨；
//! - 一个会话最多属于一个 workspace；不在任何 workspace = 未分组（Ungrouped）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use qaqh_types::SessionMeta;

/// 生成稳定 id：`ws-{unix_ms}-{path 哈希前 8 位}-{计数器}`（不引入 uuid 依赖；
/// 时间戳 + 路径哈希 + 进程内计数器防碰撞，同一毫秒同路径也得不同 id）。
fn generate_id(path: &str) -> String {
    use sha2::Digest;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let hash = sha2::Sha256::digest(path.as_bytes());
    let short = hex::encode(&hash[..4]);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ws-{ms}-{short}-{n}")
}

/// canonicalize 并归一化为可比较路径串：Windows 下去掉 `\\?\` 扩展前缀
/// （`std::fs::canonicalize` 在 Windows 返回 verbatim 路径，与用户输入/前端
/// 传入路径不匹配会导致归属判定失效），分隔符统一 `\`。失败返回原样字符串。
pub fn canonical_cwd(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => {
            let mut s = p.to_string_lossy().replace('/', "\\");
            if let Some(stripped) = s.strip_prefix("\\\\?\\") {
                s = stripped.to_string();
            }
            s
        }
        Err(_) => path.to_string_lossy().replace('/', "\\"),
    }
}

/// 一个工作区：稳定 id + canonical path + 标题 + 会话账户（手动有序）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMeta {
    pub id: String,
    pub path: String,
    pub title: String,
    pub order: u32,
    pub session_ids: Vec<String>,
}

static INSTANCE: OnceLock<WorkspaceStore> = OnceLock::new();

/// 工作区注册表单例：内存态 = 磁盘态（每次变更 atomic replace-write）。
#[derive(Debug)]
pub struct WorkspaceStore {
    file: PathBuf,
    inner: Mutex<Vec<WorkspaceMeta>>,
    next_order: Mutex<u32>,
}

/// 归一化路径用于归属比较：分隔符统一 `\`、去尾部、Windows 下大小写不敏感。
fn normalize_path(p: &str) -> String {
    let mut s = p.replace('/', "\\");
    while s.ends_with('\\') {
        s.pop();
    }
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// cwd 是否位于 workspace 路径内（相等或为其子目录，D7 匹配规则）。
fn cwd_belongs(cwd: &str, ws_path: &str) -> bool {
    let c = normalize_path(cwd);
    let w = normalize_path(ws_path);
    if c.is_empty() || w.is_empty() {
        return false;
    }
    c == w || c.starts_with(&format!("{w}\\"))
}

impl WorkspaceStore {
    /// 初始化全局单例（daemon 启动时与 SessionManager::init 同点调用）。
    pub fn init(data_dir: PathBuf) {
        let file = data_dir.join("workspaces.json");
        let mut inner: Vec<WorkspaceMeta> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        inner.sort_by_key(|w| w.order);
        let next_order = inner.iter().map(|w| w.order).max().map_or(0, |m| m + 1);
        let store = Self {
            file,
            inner: Mutex::new(inner),
            next_order: Mutex::new(next_order),
        };
        INSTANCE
            .set(store)
            .expect("WorkspaceStore already initialized");
    }

    /// 访问全局实例。
    pub fn global() -> &'static Self {
        INSTANCE
            .get()
            .expect("WorkspaceStore not initialized — call init() first")
    }

    fn persist(&self, items: &[WorkspaceMeta]) -> Result<(), String> {
        let tmp = self.file.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(items)
            .map_err(|e| format!("serialize workspaces: {e}"))?;
        {
            use std::io::Write;
            let mut f =
                std::fs::File::create(&tmp).map_err(|e| format!("create workspaces tmp: {e}"))?;
            f.write_all(json.as_bytes())
                .map_err(|e| format!("write workspaces tmp: {e}"))?;
            f.flush()
                .map_err(|e| format!("flush workspaces tmp: {e}"))?;
            f.sync_all()
                .map_err(|e| format!("sync workspaces tmp: {e}"))?;
        }
        std::fs::rename(&tmp, &self.file).map_err(|e| format!("rename workspaces: {e}"))
    }

    fn save(&self, items: &[WorkspaceMeta]) {
        if let Err(e) = self.persist(items) {
            log::error!("WorkspaceStore: persist failed: {e}");
        }
    }

    /// 全部 workspace，按 order 升序（调用方只读，勿改成员顺序）。
    pub fn list(&self) -> Vec<WorkspaceMeta> {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        items.sort_by_key(|w| w.order);
        items.clone()
    }

    /// 某会话当前归属的 workspace id（无 = 未分组）。
    pub fn workspace_of(&self, seed: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|w| w.session_ids.iter().any(|s| s == seed))
            .map(|w| w.id.clone())
    }

    /// 注册一个目录为 workspace。目录必须存在（canonicalize）；
    /// 已存在的 cwd 匹配会话自动归属（D1 双轨自动侧）。
    pub fn create(&self, path: &str, existing: &[SessionMeta]) -> Result<WorkspaceMeta, String> {
        if !Path::new(path).is_dir() {
            return Err(format!("workspace.create: not a directory: {path}"));
        }
        let canonical_str = canonical_cwd(Path::new(path));
        // 去重：同一 canonical 路径不重复创建，直接返回已存在项（避免
        // 顶部/左侧两入口重复点导致多条同路径工作区，左侧筛选失焦）。
        {
            let items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = items.iter().find(|w| normalize_path(&w.path) == normalize_path(&canonical_str)) {
                return Ok(existing.clone());
            }
        }
        let title = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| path.to_string());
        let id = generate_id(&canonical_str);

        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let order = *self.next_order.lock().unwrap_or_else(|e| e.into_inner());
        *self.next_order.lock().unwrap_or_else(|e| e.into_inner()) = order + 1;

        let mut session_ids: Vec<String> = Vec::new();
        // 自动归属：已有会话 cwd 匹配本 workspace，且当前未归属其他 workspace。
        for meta in existing {
            let belongs = meta
                .cwd
                .as_deref()
                .is_some_and(|cwd| cwd_belongs(cwd, &canonical_str));
            if belongs
                && !items
                    .iter()
                    .any(|w| w.session_ids.iter().any(|s| s == &meta.seed))
            {
                session_ids.push(meta.seed.clone());
            }
        }
        let ws = WorkspaceMeta {
            id: id.clone(),
            path: canonical_str,
            title,
            order,
            session_ids,
        };
        items.push(ws.clone());
        self.save(&items);
        Ok(ws)
    }

    /// 重命名（标题任意，重复允许——对齐 dsh 语义）。
    pub fn rename(&self, id: &str, title: String) -> Result<WorkspaceMeta, String> {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ws = items
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| format!("workspace.rename: unknown id {id}"))?;
        ws.title = title;
        let out = ws.clone();
        self.save(&items);
        Ok(out)
    }

    /// 删除 workspace 注册（不删会话；其会话变为未分组）。
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = items.len();
        items.retain(|w| w.id != id);
        if items.len() == before {
            return Err(format!("workspace.delete: unknown id {id}"));
        }
        self.save(&items);
        Ok(())
    }

    /// 把会话放入 cwd 匹配的 workspace（自动归属；新会话创建时调用）。
    /// 不匹配返回 None（保持未分组）；已归属其他 workspace 则迁移。
    pub fn attach_by_cwd(&self, seed: &str, cwd: &str) -> Option<String> {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let target = items.iter().find(|w| cwd_belongs(cwd, &w.path))?.id.clone();
        for w in items.iter_mut() {
            w.session_ids.retain(|s| s != seed);
        }
        let ws = items
            .iter_mut()
            .find(|w| w.id == target)
            .expect("target workspace vanished");
        ws.session_ids.push(seed.to_string());
        let out = ws.id.clone();
        self.save(&items);
        Some(out)
    }

    /// 显式把会话移入指定 workspace（D5 菜单移动）；原归属自动移除。
    pub fn move_session(&self, seed: &str, to_ws_id: &str) -> Result<(), String> {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !items.iter().any(|w| w.id == to_ws_id) {
            return Err(format!(
                "workspace.move_session: unknown workspace {to_ws_id}"
            ));
        }
        for w in items.iter_mut() {
            w.session_ids.retain(|s| s != seed);
        }
        let ws = items
            .iter_mut()
            .find(|w| w.id == to_ws_id)
            .expect("target workspace vanished");
        if !ws.session_ids.iter().any(|s| s == seed) {
            ws.session_ids.push(seed.to_string());
        }
        self.save(&items);
        Ok(())
    }

    /// 把会话从所有 workspace 账户移除（会话删除时由 SessionManager 调用）。
    pub fn remove_session(&self, seed: &str) {
        let mut items = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for w in items.iter_mut() {
            let before = w.session_ids.len();
            w.session_ids.retain(|s| s != seed);
            changed |= w.session_ids.len() != before;
        }
        if changed {
            self.save(&items);
        }
    }

    /// 目录是否仍存在（前端 missing-dir 标记，对齐 dsh `status()`）。
    pub fn path_status(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)] // Windows 盘符/cmd 语义；Linux 无对应环境
    fn cwd_belongs_matches_self_and_children() {
        assert!(cwd_belongs(r"C:\proj\a", r"C:\proj\a"));
        assert!(cwd_belongs(r"C:\proj\a\src", r"C:\proj\a"));
        assert!(cwd_belongs(r"c:\PROJ\a\src", r"C:\proj\a")); // Windows 大小写不敏感
        assert!(!cwd_belongs(r"C:\proj\ab", r"C:\proj\a"));
        assert!(!cwd_belongs(r"C:\other", r"C:\proj\a"));
        assert!(!cwd_belongs("", r"C:\proj\a"));
    }

    #[test]
    fn generate_id_is_stable_shape() {
        let a = generate_id(r"C:\proj");
        let b = generate_id(r"C:\proj");
        assert!(a.starts_with("ws-"));
        assert_ne!(a, b); // 时间戳前缀保证不同
    }

    #[test]
    fn move_session_migrates_account() {
        let dir = std::env::temp_dir().join(format!("dsh-ws-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("a"));
        let _ = std::fs::create_dir_all(dir.join("b"));
        WorkspaceStore::init(dir.clone());
        let store = WorkspaceStore::global();
        let ws_a = store
            .create(dir.join("a").to_str().expect("path"), &[])
            .expect("create a");
        let ws_b = store
            .create(dir.join("b").to_str().expect("path"), &[])
            .expect("create b");
        assert_eq!(store.list().len(), 2);

        store.attach_by_cwd("s1", dir.join("a").to_str().expect("path"));
        assert_eq!(store.workspace_of("s1").as_deref(), Some(ws_a.id.as_str()));

        store.move_session("s1", &ws_b.id).expect("move");
        assert_eq!(store.workspace_of("s1").as_deref(), Some(ws_b.id.as_str()));
        let list = store.list();
        assert!(
            list.iter()
                .find(|w| w.id == ws_a.id)
                .expect("a")
                .session_ids
                .is_empty()
        );

        store.remove_session("s1");
        assert_eq!(store.workspace_of("s1"), None);

        store.delete(&ws_a.id).expect("delete a");
        assert_eq!(store.list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ═══════════════════════════════════════════════════════
// 运行环境工作目录统一数据源
// ═══════════════════════════════════════════════════════

/// 会话运行环境工作目录（worker 启动 cwd / git 归属 / workspace.get 的权威
/// 数据源）。统一存 `SessionMeta.cwd`。
///
/// 存量迁移：旧版本存 `sessions/{seed}/workspace.txt`。首次读到且
/// `meta.cwd` 为空时，惰性迁移进 meta（atomic replace-write）并删除 txt；
/// 两个进程（daemon/worker）竞争迁移时幂等——写 meta 原子、删 txt 幂等。
///
/// 调用方必须先 `SessionManager::init`。不需要 SessionManager 的环境
/// （如 workspace serve 进程）用 [`session_workspace_from_disk`]（只读）。
pub fn session_workspace_cwd(seed: &str) -> Option<String> {
    let mgr = crate::SessionManager::global();
    let meta = mgr.load_meta(seed)?;
    if let Some(cwd) = meta.cwd.as_deref().filter(|c| !c.is_empty()) {
        return Some(cwd.to_string());
    }
    // 惰性迁移：旧 workspace.txt → meta.cwd
    let txt_path = qaqh_types::platform::sessions_dir()
        .join(seed)
        .join("workspace.txt");
    let legacy = std::fs::read_to_string(&txt_path).ok()?;
    let legacy = legacy.trim().to_string();
    if legacy.is_empty() {
        return None;
    }
    let canonical = canonical_cwd(std::path::Path::new(&legacy));
    mgr.set_cwd(seed, &canonical, true);
    let _ = std::fs::remove_file(&txt_path);
    Some(canonical)
}

/// 只读版：meta.json `cwd` → 旧 `workspace.txt` fallback（不做迁移、不要求
/// SessionManager init）。给 workspace serve / CLI 等无 session 环境的进程用。
pub fn session_workspace_from_disk(seed: &str) -> Option<String> {
    let dir = qaqh_types::platform::sessions_dir().join(seed);
    if let Ok(text) = std::fs::read_to_string(dir.join("meta.json"))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(cwd) = value.get("cwd").and_then(|c| c.as_str())
        && !cwd.is_empty()
    {
        return Some(cwd.to_string());
    }
    std::fs::read_to_string(dir.join("workspace.txt"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
