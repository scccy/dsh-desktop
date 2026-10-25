//! guest 的持久分区标识。
//!
//! **与官方的有意差异**：官方 `browser-guests.ts` 用
//! `session.fromPartition("dsh-sidebar-browser-<randomUUID>")` —— 没有 `persist:` 前缀，
//! 是**内存**分区：进程内同一 workspace 的多标签共享，退出即丢。
//! 本仓库按 [desktop-parity §6.4](../../../docs/desktop-parity.md) 的
//! 「按账号/来源的**持久**分区」实现：workspace key → 稳定哈希 → 固定的数据存储，
//! 同一 workspace 每次启动落回同一个 cookie / localStorage 分区，不同 workspace 互相隔离。
//!
//! | 平台 | 承载方式 |
//! |---|---|
//! | macOS ≥ 14 | [`data_store_identifier`] → wry `WKWebsiteDataStore.dataStoreForIdentifier:` |
//! | Windows / Linux | [`data_directory`] → 该 guest 独占的 wry `WebContext`（与 content webview 不共享） |
//!
//! **已知限制**：macOS < 14 上 wry 会静默回落到 default data store，分区不再隔离
//! （wry 源码注释明确 custom data store 仅 macOS ≥ 14 可用）。写进 docs/desktop-parity.md §9。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 分区标识前缀，同时是 data 目录名的一部分。
const PARTITION_PREFIX: &str = "ws-";

/// workspace key → 稳定的分区标识。纯函数：同一输入永远得到同一输出。
pub fn partition_id(workspace: &str) -> String {
    let mut value = String::with_capacity(PARTITION_PREFIX.len() + 32);
    value.push_str(PARTITION_PREFIX);
    for salt in ["dsh-sidebar-browser/1", "dsh-sidebar-browser/2"] {
        value.push_str(&format!("{:016x}", salted_hash(salt, workspace)));
    }
    value
}

/// 分区标识 → wry 的 16 字节 data store identifier（macOS ≥ 14 使用）。
pub fn data_store_identifier(partition: &str) -> [u8; 16] {
    let mut identifier = [0_u8; 16];
    identifier[..8].copy_from_slice(&salted_hash("data-store/1", partition).to_be_bytes());
    identifier[8..].copy_from_slice(&salted_hash("data-store/2", partition).to_be_bytes());
    identifier
}

/// 分区标识 → Windows / Linux 上给 guest 用的独立 data 目录。
pub fn data_directory(root: &Path, partition: &str) -> PathBuf {
    root.join(partition)
}

/// `DefaultHasher::new()` 用固定密钥，因此跨进程稳定（同 bridge.rs 的既有用法）。
fn salted_hash(salt: &str, value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{data_directory, data_store_identifier, partition_id};
    use std::path::Path;

    #[test]
    fn partition_ids_are_stable_and_distinct() {
        let first = partition_id("cwd:/Users/alice/project");
        // 同一输入 → 同一输出（可复现，跨进程也成立：DefaultHasher 固定密钥）
        assert_eq!(first, partition_id("cwd:/Users/alice/project"));
        assert_ne!(first, partition_id("cwd:/Users/alice/other"));
        assert_ne!(first, partition_id("session:abc"));
        assert!(first.starts_with("ws-"));
        // 前缀 + 32 个十六进制字符
        assert_eq!(first.len(), "ws-".len() + 32);
        assert!(first["ws-".len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn data_store_identifiers_are_stable_and_distinct() {
        let partition = partition_id("cwd:/tmp/work");
        let identifier = data_store_identifier(&partition);
        assert_eq!(identifier, data_store_identifier(&partition));
        assert_eq!(identifier.len(), 16);
        assert_ne!(
            identifier,
            data_store_identifier(&partition_id("cwd:/tmp/other"))
        );
        // 不同平台/不同分区不会撞成全零
        assert_ne!(identifier, [0_u8; 16]);
    }

    #[test]
    fn data_directories_stay_under_the_root() {
        let root = Path::new("/tmp/dsh-desktop/browser");
        let partition = partition_id("cwd:/tmp/work");
        assert_eq!(data_directory(root, &partition), root.join(&partition));
        // 分区标识是自己拼的，不含路径分隔符
        assert!(!partition.contains('/'));
    }
}
