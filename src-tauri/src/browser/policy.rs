//! guest 的导航策略（官方 `apps/desktop/src/browser-guests.ts::allowedNavigation` 的纯函数部分）。
//!
//! 官方用 Electron 的四条钩子约束 Sidebar guest：
//!
//! | Electron 钩子 | 官方行为 | 本模块 |
//! |---|---|---|
//! | `will-frame-navigate` / `will-redirect` | 主 frame 只允许 `http(s)`、不含内嵌凭据、不是受权应用 origin | [`allows_navigation`] |
//! | `setWindowOpenHandler` | 一律 `deny`，只有白名单内的 URL 上报给页面自己开标签 | [`allows_new_window`] |
//! | `session.webRequest.onBeforeRequest` | 取消非 `http(s)/ws(s)` 与带凭据的请求 | 无等价物（见 docs/desktop-parity.md §9） |
//! | `will-download` | 一律 `preventDefault` | `on_download` 返回 `false`（在 guest.rs 内联） |
//!
//! Tauri 没有 redirect 钩子，也拿不到子资源级的请求拦截，所以这里只覆盖「主 frame 导航」
//! 与「新窗口」两条；WRY 的 `on_navigation` 只在主 frame 决策点触发。

use url::Url;

/// 回环别名：与受权 origin 同端口时一并视为「受权 origin」。
///
/// 与官方 `isApplicationHost` 的名单逐字一致（官方写的是 `'[::1]'`，`url::Url::host_str`
/// 对 IPv6 也返回带方括号的形态）。
const LOOPBACK_ALIASES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// 应用自己的受权 origin 集合：`(hostname, port)`。
///
/// guest 绝不能到达这些 origin —— 它们背后是持有 Host cookie 的回环代理，
/// 放行等于让第三方页面借壳拿到已认证的 API。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockedOrigins {
    entries: Vec<(String, u16)>,
}

impl BlockedOrigins {
    /// 空的集合（Host/app server 尚未就绪时）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一个 `scheme://host:port` origin。
    ///
    /// @returns 是否被接受；非 `http(s)`、无 host 或无法解析时忽略并返回 `false`。
    pub fn insert(&mut self, value: &str) -> bool {
        let Ok(url) = Url::parse(value.trim()) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return false;
        };
        let entry = (host.to_ascii_lowercase(), port);
        if !self.entries.contains(&entry) {
            self.entries.push(entry);
        }
        true
    }

    /// 是否一条受权 origin 都没登记。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 官方 `isApplicationHost`：端口相同，且主机名相同或落在回环别名里。
    pub fn blocks(&self, url: &Url) -> bool {
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let loopback = LOOPBACK_ALIASES
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(&host));
        self.entries.iter().any(|(blocked_host, blocked_port)| {
            *blocked_port == port && (*blocked_host == host || loopback)
        })
    }
}

/// 主 frame 导航白名单（官方 `allowedNavigation`）。
///
/// 只放行 `http(s)`、不含 `user:password@`、且不是受权 origin 的 URL。
pub fn allows_navigation(url: &Url, blocked: &BlockedOrigins) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && !blocked.blocks(url)
}

/// 新窗口请求沿用同一张白名单（官方 `setWindowOpenHandler`）。
pub fn allows_new_window(url: &Url, blocked: &BlockedOrigins) -> bool {
    allows_navigation(url, blocked)
}

/// `on_navigation` 的最终判定：白名单之外**额外放行引导文档 `about:blank`**。
///
/// 子 webview 用 `about:blank` 作为初始文档（对齐官方 `about:blank#<lease>` 的两段式）；
/// 这个初始加载在 Electron 里不经过 `will-frame-navigate`，但 wry 的导航处理可能会看到它，
/// 所以显式放行，避免 guest 一创建就被自己的策略拦掉。其余 `about:` 仍然拒绝。
pub fn allows_guest_navigation(url: &Url, blocked: &BlockedOrigins) -> bool {
    allows_navigation(url, blocked) || url.as_str() == "about:blank"
}

/// 解析并校验页面给的地址（`element.loadURL()` / 地址栏提交）。
///
/// @returns 通过白名单的绝对 URL；`file:`、`javascript:`、带凭据的 URL 与受权 origin 都拒绝。
pub fn parse_guest_target(value: &str, blocked: &BlockedOrigins) -> Result<Url, String> {
    let url = Url::parse(value.trim())
        .map_err(|error| format!("dsh desktop: invalid browser address: {error}"))?;
    if !allows_navigation(&url, blocked) {
        return Err("dsh desktop: this address is not allowed in the embedded browser".to_owned());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{
        BlockedOrigins, allows_guest_navigation, allows_navigation, allows_new_window,
        parse_guest_target,
    };

    fn blocked() -> BlockedOrigins {
        // 回环 app server 的形态：127.0.0.1:43117
        let mut blocked = BlockedOrigins::new();
        assert!(blocked.insert("http://127.0.0.1:43117"));
        blocked
    }

    fn parse(value: &str) -> url::Url {
        url::Url::parse(value).expect("test URL is valid")
    }

    #[test]
    fn accepts_plain_https_pages() {
        assert!(allows_navigation(
            &parse("https://example.com/a?b=1"),
            &blocked()
        ));
        assert!(allows_new_window(&parse("http://example.com/"), &blocked()));
    }

    #[test]
    fn rejects_non_http_schemes() {
        for value in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "about:blank",
            "chrome://settings",
            "dsh-app://app/",
        ] {
            assert!(
                !allows_navigation(&parse(value), &blocked()),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert!(!allows_navigation(
            &parse("https://user:pass@example.com/"),
            &blocked()
        ));
        assert!(!allows_navigation(
            &parse("https://user@example.com/"),
            &blocked()
        ));
    }

    #[test]
    fn rejects_the_application_origin_and_loopback_aliases() {
        let blocked = blocked();
        // 同一端口 + 同一主机
        assert!(blocked.blocks(&parse("http://127.0.0.1:43117/api/health")));
        // 同一端口 + 回环别名
        assert!(blocked.blocks(&parse("http://localhost:43117/")));
        assert!(blocked.blocks(&parse("http://[::1]:43117/")));
        // 其它端口放行（对齐官方：只按端口比对）
        assert!(!blocked.blocks(&parse("http://127.0.0.1:5173/")));
        assert!(!blocked.blocks(&parse("https://example.com:43117/")));
        assert!(allows_navigation(
            &parse("http://127.0.0.1:5173/"),
            &blocked
        ));
    }

    #[test]
    fn ignores_unusable_origins() {
        let mut blocked = BlockedOrigins::new();
        assert!(!blocked.insert(""));
        assert!(!blocked.insert("file:///tmp/x"));
        assert!(!blocked.insert("not a url"));
        assert!(blocked.is_empty());
        // 空集合不拦任何 http(s)
        assert!(allows_navigation(&parse("https://example.com/"), &blocked));
    }

    #[test]
    fn parse_guest_target_normalizes_and_rejects() {
        let blocked = blocked();
        let target = parse_guest_target("  https://example.com/a  ", &blocked).expect("allowed");
        assert_eq!(target.as_str(), "https://example.com/a");
        for value in [
            "",
            "not a url",
            "file:///etc/passwd",
            "http://127.0.0.1:43117/api",
        ] {
            assert!(
                parse_guest_target(value, &blocked).is_err(),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn loopback_aliases_are_interchangeable_at_the_same_port() {
        // 登记其中任意一个回环写法，同端口的另外两种写法都必须被挡（官方名单 'localhost' /
        // '127.0.0.1' / '[::1]'）。
        for registered in [
            "http://127.0.0.1:5000",
            "http://localhost:5000",
            "http://[::1]:5000",
        ] {
            let mut blocked = BlockedOrigins::new();
            assert!(blocked.insert(registered), "{registered} must be accepted");
            for candidate in [
                "http://127.0.0.1:5000/x",
                "http://localhost:5000/",
                "http://[::1]:5000/",
            ] {
                assert!(
                    blocked.blocks(&parse(candidate)),
                    "{candidate} must be blocked when {registered} is registered"
                );
            }
        }
    }

    #[test]
    fn loopback_on_other_ports_is_still_browsable() {
        // 只按「端口 + 主机名」比对：127.0.0.1 上的其它本地开发服务必须能正常打开。
        let blocked = blocked();
        for candidate in [
            "http://127.0.0.1:5173/",
            "http://localhost:8080/",
            "http://[::1]:3000/",
            "https://127.0.0.1:43118/",
            "http://example.com:43117/",
        ] {
            assert!(
                !blocked.blocks(&parse(candidate)),
                "{candidate} must stay browsable"
            );
            assert!(allows_navigation(&parse(candidate), &blocked));
        }
    }

    #[test]
    fn guest_navigation_also_accepts_the_bootstrap_document() {
        let blocked = blocked();
        assert!(allows_guest_navigation(&parse("about:blank"), &blocked));
        assert!(!allows_guest_navigation(&parse("about:srcdoc"), &blocked));
        assert!(!allows_guest_navigation(
            &parse("file:///etc/passwd"),
            &blocked
        ));
        assert!(allows_guest_navigation(
            &parse("https://example.com/"),
            &blocked
        ));
        assert!(!allows_guest_navigation(
            &parse("http://127.0.0.1:43117/api"),
            &blocked
        ));
    }

    #[test]
    fn keeps_default_ports() {
        let mut blocked = BlockedOrigins::new();
        assert!(blocked.insert("https://example.com/"));
        assert!(blocked.blocks(&parse("https://example.com:443/")));
        assert!(!blocked.blocks(&parse("https://example.com:8443/")));
    }
}
