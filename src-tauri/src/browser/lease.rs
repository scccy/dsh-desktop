//! lease 与 guest 的归属表。
//!
//! 对齐官方 `browser-guests.ts` 的两级所有权：
//!
//! * **workspace → 分区**：分区由 workspace key 决定（[`super::partition`]），跨标签、跨重启稳定；
//! * **lease → guest**：每次 `dshDesktop.browser.acquire(workspace)` 发一个新 lease，
//!   页面用这个 lease 建 `<webview>`；`release` 只销毁该 lease 名下那一个 guest，分区保留。
//!
//! 所有状态都在一个 `Mutex` 里，规则与官方一致：
//! * workspace 必须是 1..=4096 字符的字符串，否则 `acquire` 报错；
//! * 只有发起 `acquire` 的那个 webview（= `content`）能 `release` 它自己的 lease，
//!   别人的 lease 报错；未知 lease 静默成功（幂等）；
//! * `attach` 时再校验一次 lease 归属、partition 与「尚未挂载」，等价于官方
//!   `will-attach-webview` 里的三道检查（lease 存在 / owner 匹配 / partition 匹配 / 未重复挂载）。
//!
//! 锁纪律：持锁期间只改表，不做任何 webview 操作；webview 操作一律在锁外做，
//! 因为 Tauri 的 dispatcher 调用在部分平台会回到主线程，而回调本身就在主线程上。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::Webview;

use super::guest::{Guest, GuestSnapshot};
use super::partition;

/// workspace key 的长度上限（与官方 IPC 校验一致）。
const WORKSPACE_LIMIT: usize = 4096;

/// 一次 guest 预约：页面拿着 lease 换一个子 webview。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// 不透明的 lease 标识（同时是 `<webview name>` 与状态事件的关联键）。
    pub lease: String,
    /// 已批准的持久分区标识（页面原样写到 `<webview partition>` 上）。
    pub partition: String,
}

/// 一条 lease 记录。
pub(crate) struct Lease {
    /// 发起 `acquire` 的 webview label（只有它能 `release`）。
    owner: String,
    /// 已批准的持久分区标识。
    partition: String,
    /// 已挂载的 guest。
    guest: Option<Guest>,
}

/// 壳进程内所有 guest 的归属与分区状态（`.manage(BrowserState::new(data_dir))`）。
pub struct BrowserState {
    /// guest 数据目录的根（`<data_dir>/browser`）。
    root: PathBuf,
    /// lease / label 的递增序号。
    next: AtomicU64,
    /// lease 表。
    registry: Mutex<BTreeMap<String, Lease>>,
}

impl BrowserState {
    /// 建状态。@param data_dir - 壳自己的数据目录（`AppState::data_dir`）。
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: data_dir.into().join("browser"),
            next: AtomicU64::new(1),
            registry: Mutex::new(BTreeMap::new()),
        }
    }

    /// guest 数据目录的根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 下一个子 webview 的 label。
    pub(crate) fn next_guest_label(&self) -> String {
        format!(
            "browser-guest-{}",
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// 为一个 workspace 预约一个 guest（官方 `acquire`）。
    pub(crate) fn reserve(&self, owner: &str, workspace: &Value) -> Result<Reservation, String> {
        let Some(key) = workspace.as_str() else {
            return Err("dsh desktop: a workspace storage identity is required".to_owned());
        };
        if key.is_empty() || key.len() > WORKSPACE_LIMIT {
            return Err("dsh desktop: a workspace storage identity is required".to_owned());
        }
        let partition = partition::partition_id(key);
        let lease = self.next_lease_id();
        self.lock().insert(
            lease.clone(),
            Lease {
                owner: owner.to_owned(),
                partition: partition.clone(),
                guest: None,
            },
        );
        Ok(Reservation { lease, partition })
    }

    /// 校验一次挂载请求（等价于官方 `will-attach-webview` 的三道检查）。
    pub(crate) fn check_attachment(
        &self,
        owner: &str,
        lease: &str,
        partition: &str,
    ) -> Result<(), String> {
        let registry = self.lock();
        let Some(record) = registry.get(lease) else {
            return Err("dsh desktop: unknown browser guest lease".to_owned());
        };
        if record.owner != owner {
            return Err("dsh desktop: browser guest belongs to another window".to_owned());
        }
        if record.partition != partition {
            return Err("dsh desktop: browser guest partition mismatch".to_owned());
        }
        if record.guest.is_some() {
            return Err("dsh desktop: browser guest is already attached".to_owned());
        }
        Ok(())
    }

    /// 记录已创建的 guest，返回初始快照。
    pub(crate) fn attach(
        &self,
        owner: &str,
        lease: &str,
        partition: &str,
        webview: Webview,
    ) -> Result<GuestSnapshot, String> {
        // 锁外创建 -> 锁内二次校验，避免「建好之后发现租约已被页面释放」的泄漏。
        let mut registry = self.lock();
        let Some(record) = registry.get_mut(lease) else {
            return Err("dsh desktop: unknown browser guest lease".to_owned());
        };
        if record.owner != owner || record.partition != partition || record.guest.is_some() {
            return Err("dsh desktop: browser guest lease changed while attaching".to_owned());
        }
        let guest = Guest::new(webview, lease);
        let snapshot = guest.snapshot.clone();
        record.guest = Some(guest);
        Ok(snapshot)
    }

    /// 在短锁内读写一个 guest。闭包里**不得**调用任何 webview 操作（见模块文档的锁纪律）。
    pub(crate) fn with_guest<R>(
        &self,
        lease: &str,
        apply: impl FnOnce(&mut Guest) -> R,
    ) -> Option<R> {
        let mut registry = self.lock();
        let guest = registry.get_mut(lease)?.guest.as_mut()?;
        Some(apply(guest))
    }

    /// 当前快照（页面重连时用来重新对齐）。
    pub(crate) fn snapshot(&self, owner: &str, lease: &str) -> Result<GuestSnapshot, String> {
        let registry = self.lock();
        let Some(record) = registry.get(lease) else {
            return Err("dsh desktop: unknown browser guest lease".to_owned());
        };
        if record.owner != owner {
            return Err("dsh desktop: browser guest belongs to another window".to_owned());
        }
        match record.guest.as_ref() {
            Some(guest) => Ok(guest.snapshot.clone()),
            None => Ok(GuestSnapshot {
                lease: lease.to_owned(),
                ..GuestSnapshot::default()
            }),
        }
    }

    /// guest 的子 webview 句柄（命令在锁外操作它）。
    pub(crate) fn guest_webview(&self, owner: &str, lease: &str) -> Result<Webview, String> {
        let registry = self.lock();
        let Some(record) = registry.get(lease) else {
            return Err("dsh desktop: unknown browser guest lease".to_owned());
        };
        if record.owner != owner {
            return Err("dsh desktop: browser guest belongs to another window".to_owned());
        }
        record
            .guest
            .as_ref()
            .map(|guest| guest.webview.clone())
            .ok_or_else(|| "dsh desktop: browser guest is not attached".to_owned())
    }

    /// 释放一条 lease（未知 lease 幂等成功）。@returns 需要关闭的 guest。
    pub(crate) fn release(&self, owner: &str, lease: &str) -> Result<Option<Guest>, String> {
        let mut registry = self.lock();
        match registry.get(lease) {
            None => return Ok(None),
            Some(record) if record.owner != owner => {
                return Err("dsh desktop: browser guest belongs to another window".to_owned());
            }
            Some(_) => {}
        }
        Ok(registry.remove(lease).and_then(|record| record.guest))
    }

    /// 清空全部 lease（窗口关闭 / 应用退出 / content 主 frame 导航时调用）。
    pub(crate) fn close_all(&self) -> Vec<Guest> {
        std::mem::take(&mut *self.lock())
            .into_values()
            .filter_map(|record| record.guest)
            .collect()
    }

    /// 当前 lease 数量（诊断 / 单测）。
    pub fn lease_count(&self) -> usize {
        self.lock().len()
    }

    /// 一条 lease 是否已挂载 guest。只给单测断言用（生产代码看 `snapshot` / `guest_webview`）。
    #[cfg(test)]
    pub(crate) fn is_attached(&self, lease: &str) -> bool {
        self.lock()
            .get(lease)
            .is_some_and(|record| record.guest.is_some())
    }

    fn next_lease_id(&self) -> String {
        let counter = self.next.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        format!("lease-{nanos:032x}-{counter:04x}")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Lease>> {
        self.registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserState, Reservation, WORKSPACE_LIMIT};
    use serde_json::json;
    use std::path::Path;

    fn state() -> BrowserState {
        BrowserState::new("/tmp/dsh-desktop-test")
    }

    #[test]
    fn reserve_requires_a_workspace_string() {
        let state = state();
        assert!(state.reserve("content", &json!(null)).is_err());
        assert!(state.reserve("content", &json!(7)).is_err());
        assert!(state.reserve("content", &json!("")).is_err());
        assert!(
            state
                .reserve("content", &json!("x".repeat(WORKSPACE_LIMIT + 1)))
                .is_err()
        );
        assert!(state.reserve("content", &json!("cwd:/tmp/work")).is_ok());
    }

    #[test]
    fn the_same_workspace_always_gets_the_same_partition() {
        let state = state();
        let first = state
            .reserve("content", &json!("cwd:/tmp/work"))
            .expect("reserved");
        let second = state
            .reserve("content", &json!("cwd:/tmp/work"))
            .expect("reserved");
        // 两次 lease 不同，但分区相同（跨标签共享同一份持久存储）
        assert_ne!(first.lease, second.lease);
        assert_eq!(first.partition, second.partition);
        let other = state
            .reserve("content", &json!("session:abc"))
            .expect("reserved");
        assert_ne!(first.partition, other.partition);
        assert_eq!(state.lease_count(), 3);
    }

    #[test]
    fn release_only_accepts_the_owner() {
        let state = state();
        let reservation = state
            .reserve("content", &json!("cwd:/tmp/work"))
            .expect("reserved");
        assert!(state.release("header", &reservation.lease).is_err());
        // 未知 lease 幂等
        assert!(
            state
                .release("content", "lease-unknown")
                .expect("idempotent")
                .is_none()
        );
        assert!(
            state
                .release("content", &reservation.lease)
                .expect("released")
                .is_none()
        );
        assert_eq!(state.lease_count(), 0);
    }

    #[test]
    fn attachment_checks_owner_and_partition() {
        let state = state();
        let Reservation { lease, partition } = state
            .reserve("content", &json!("cwd:/tmp/work"))
            .expect("reserved");
        assert!(
            state
                .check_attachment("header", &lease, &partition)
                .is_err()
        );
        assert!(
            state
                .check_attachment("content", &lease, "ws-other")
                .is_err()
        );
        assert!(
            state
                .check_attachment("content", "lease-unknown", &partition)
                .is_err()
        );
        assert!(
            state
                .check_attachment("content", &lease, &partition)
                .is_ok()
        );
        assert!(!state.is_attached(&lease));
    }

    #[test]
    fn templates_stay_under_the_data_directory() {
        let state = BrowserState::new("/tmp/dsh-desktop-test");
        assert_eq!(state.root(), Path::new("/tmp/dsh-desktop-test/browser"));
    }

    #[test]
    fn guest_labels_are_unique() {
        let state = state();
        let first = state.next_guest_label();
        let second = state.next_guest_label();
        assert_ne!(first, second);
        assert!(first.starts_with("browser-guest-"));
    }
}
