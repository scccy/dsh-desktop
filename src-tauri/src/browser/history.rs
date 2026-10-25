//! guest 的线性历史模型。
//!
//! Electron 的 `<webview>` 直接暴露 Chromium 的 history：`canGoBack` / `canGoForward` /
//! `goBack` / `goForward` / `clearHistory`。Tauri / wry 没有查询或操作原生 history list 的
//! API，所以这里按「主 frame 已提交的 URL 序列」近似：
//!
//! * 每次 `PageLoadEvent::Finished` 调用 [`GuestHistory::commit`]，同文档跳转（只有 hash /
//!   history state 变化）覆盖当前条目，跨文档跳转压入新条目；
//! * `goBack` / `goForward` 是**重新导航到记录里的 URL**，不是真正的 history 恢复
//!   （表单状态、滚动位置不保留）——这条差异写进 docs/desktop-parity.md §9；
//! * `clearHistory` 只保留当前文档，对齐 Electron 的语义。
//!
//! 已知边界：被策略拦下的导航不会进入记录；服务端 30x 跳转只记录最终 URL。

use url::Url;

/// 一个 guest 的历史记录与当前位置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestHistory {
    entries: Vec<String>,
    /// `entries` 里的当前位置；空历史为 `None`。
    index: Option<usize>,
}

impl GuestHistory {
    /// 空历史。
    pub fn new() -> Self {
        Self::default()
    }

    /// 主 frame 提交了一个文档。
    pub fn commit(&mut self, url: &str) {
        if url.is_empty() {
            return;
        }
        let Some(index) = self.index else {
            self.entries.push(url.to_owned());
            self.index = Some(0);
            return;
        };
        if self.entries.get(index).is_some_and(|entry| entry == url) {
            // 重载：位置不变。
            return;
        }
        // 后退 / 前进：我们主动导航到相邻条目，索引跟着走，不产生新条目。
        if index > 0
            && self
                .entries
                .get(index - 1)
                .is_some_and(|entry| entry == url)
        {
            self.index = Some(index - 1);
            return;
        }
        if self
            .entries
            .get(index + 1)
            .is_some_and(|entry| entry == url)
        {
            self.index = Some(index + 1);
            return;
        }
        // 同文档跳转（hash / history state）：覆盖当前条目。
        if self
            .entries
            .get(index)
            .is_some_and(|entry| same_document(entry, url))
        {
            self.entries[index] = url.to_owned();
            return;
        }
        // 新文档：截断前进分支再压入。
        self.entries.truncate(index + 1);
        self.entries.push(url.to_owned());
        self.index = Some(index + 1);
    }

    /// 清空除当前文档以外的条目（Electron 的 `clearHistory`）。
    pub fn clear(&mut self, url: &str) {
        self.entries.clear();
        if url.is_empty() {
            self.index = None;
        } else {
            self.entries.push(url.to_owned());
            self.index = Some(0);
        }
    }

    /// 当前文档。
    pub fn current(&self) -> Option<&str> {
        self.index
            .and_then(|index| self.entries.get(index))
            .map(String::as_str)
    }

    /// 是否还能后退。
    pub fn can_go_back(&self) -> bool {
        matches!(self.index, Some(index) if index > 0)
    }

    /// 是否还能前进。
    pub fn can_go_forward(&self) -> bool {
        matches!(self.index, Some(index) if index + 1 < self.entries.len())
    }

    /// 后退一步，返回要导航到的 URL；索引同时前移。
    pub fn back(&mut self) -> Option<String> {
        let index = self.index?;
        if index == 0 {
            return None;
        }
        self.index = Some(index - 1);
        self.entries.get(index - 1).cloned()
    }

    /// 前进一步，返回要导航到的 URL；索引同时后移。
    pub fn forward(&mut self) -> Option<String> {
        let index = self.index?;
        let target = self.entries.get(index + 1)?.clone();
        self.index = Some(index + 1);
        Some(target)
    }
}

/// 两个 URL 是否指向同一个文档（scheme / host / port / path / query 相同，仅 fragment 不同）。
fn same_document(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
        && left.path() == right.path()
        && left.query() == right.query()
}

#[cfg(test)]
mod tests {
    use super::GuestHistory;

    fn history(entries: &[&str]) -> GuestHistory {
        let mut history = GuestHistory::new();
        for entry in entries {
            history.commit(entry);
        }
        history
    }

    #[test]
    fn starts_empty() {
        let mut history = GuestHistory::new();
        assert_eq!(history.current(), None);
        assert!(!history.can_go_back());
        assert!(!history.can_go_forward());
        assert_eq!(history.back(), None);
        assert_eq!(history.forward(), None);
    }

    #[test]
    fn pushing_documents_walks_forward() {
        let history = history(&["https://a.test/", "https://b.test/"]);
        assert_eq!(history.current(), Some("https://b.test/"));
        assert!(history.can_go_back());
        assert!(!history.can_go_forward());
    }

    #[test]
    fn reload_keeps_the_position() {
        let mut history = history(&["https://a.test/", "https://b.test/"]);
        history.commit("https://b.test/");
        assert_eq!(history.current(), Some("https://b.test/"));
        assert!(history.can_go_back());
        assert!(!history.can_go_forward());
    }

    #[test]
    fn navigating_back_by_url_moves_the_cursor_without_a_new_entry() {
        let mut history = history(&["https://a.test/", "https://b.test/"]);
        // 页面点后退：Rust 重新导航到 a，提交后应回到 index 0
        history.commit("https://a.test/");
        assert_eq!(history.current(), Some("https://a.test/"));
        assert!(!history.can_go_back());
        assert!(history.can_go_forward());
        assert_eq!(history.forward(), Some("https://b.test/".to_owned()));
        assert!(history.can_go_back());
    }

    #[test]
    fn a_new_document_after_back_drops_the_forward_branch() {
        let mut history = history(&["https://a.test/", "https://b.test/", "https://c.test/"]);
        history.commit("https://b.test/");
        history.commit("https://d.test/");
        assert_eq!(history.current(), Some("https://d.test/"));
        assert!(!history.can_go_forward());
        assert_eq!(history.back(), Some("https://b.test/".to_owned()));
    }

    #[test]
    fn in_page_navigation_replaces_the_current_entry() {
        let mut history = history(&["https://a.test/page"]);
        history.commit("https://a.test/page#debug");
        assert_eq!(history.current(), Some("https://a.test/page#debug"));
        assert!(!history.can_go_back());
    }

    #[test]
    fn clear_history_keeps_only_the_current_document() {
        let mut history = history(&["https://a.test/", "https://b.test/"]);
        history.clear("https://b.test/");
        assert_eq!(history.current(), Some("https://b.test/"));
        assert!(!history.can_go_back());
        assert!(!history.can_go_forward());
        history.clear("");
        assert_eq!(history.current(), None);
    }

    #[test]
    fn back_then_forward_is_symmetric() {
        let mut history = history(&["https://a.test/", "https://b.test/", "https://c.test/"]);
        assert_eq!(history.back(), Some("https://b.test/".to_owned()));
        assert_eq!(history.back(), Some("https://a.test/".to_owned()));
        assert_eq!(history.back(), None);
        assert_eq!(history.forward(), Some("https://b.test/".to_owned()));
        assert_eq!(history.forward(), Some("https://c.test/".to_owned()));
        assert_eq!(history.forward(), None);
    }
}
