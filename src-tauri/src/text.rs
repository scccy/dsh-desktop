//! Static shell copy for the native menus and prompts.
//!
//! The shell ships two dictionaries, Chinese and English, and picks one from
//! the operating system's user language. Only the shell's own chrome uses
//! them: the `dsh web` page brings its own translations.

/// Copy used by the native menu bar and the prompts it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuStrings {
    /// Top-level application menu.
    pub application: &'static str,
    /// Top-level edit menu.
    pub edit: &'static str,
    /// Menu item that opens the shell settings page.
    pub open_settings: &'static str,
    /// Menu item that opens the log directory.
    pub open_logs: &'static str,
    /// Menu item that asks npm for the published version.
    pub check_update: &'static str,
    /// Menu item that quits the shell.
    pub quit: &'static str,
    /// Title of the "you are up to date" prompt.
    pub update_current_title: &'static str,
    /// Title of the "a newer dsh exists" prompt.
    pub update_available_title: &'static str,
    /// Title of a prompt that could not reach npm.
    pub update_failed_title: &'static str,
    /// Label closing a prompt.
    pub ok: &'static str,
    /// Suffix of the installed-version line of a prompt.
    pub installed_label: &'static str,
    /// Suffix of the published-version line of a prompt.
    pub latest_label: &'static str,
    /// Body of the prompt shown when npm cannot be started at all.
    pub update_npm_missing: &'static str,
}

/// Chinese dictionary.
pub const CHINESE: MenuStrings = MenuStrings {
    application: "应用",
    edit: "编辑",
    open_settings: "打开设置",
    open_logs: "打开日志目录",
    check_update: "检查 dsh 更新",
    quit: "退出",
    update_current_title: "dsh 已是最新版本",
    update_available_title: "发现新的 dsh 版本",
    update_failed_title: "无法检查 dsh 更新",
    ok: "确定",
    installed_label: "已安装",
    latest_label: "最新发布",
    update_npm_missing: "检查更新失败：未找到 npm。请确认已安装 Node.js 并加入 PATH；升级 dsh 可执行 npm i -g @deepseek-ai/dsh",
};

/// English dictionary.
pub const ENGLISH: MenuStrings = MenuStrings {
    application: "Application",
    edit: "Edit",
    open_settings: "Open settings",
    open_logs: "Open log directory",
    check_update: "Check dsh updates",
    quit: "Quit",
    update_current_title: "dsh is up to date",
    update_available_title: "A newer dsh is available",
    update_failed_title: "Could not check for dsh updates",
    ok: "OK",
    installed_label: "Installed",
    latest_label: "Published",
    update_npm_missing: "Update check failed: npm was not found. Make sure Node.js is installed and on PATH; upgrade dsh with npm i -g @deepseek-ai/dsh",
};

/// Copy for one locale tag, as reported by the operating system.
///
/// Anything that is not Chinese gets the English dictionary, including an
/// unknown locale: English is the safer default for a technical tool whose
/// product names are English.
pub fn select(locale: Option<&str>) -> MenuStrings {
    match locale {
        Some(tag) if tag.to_ascii_lowercase().starts_with("zh") => CHINESE,
        _ => ENGLISH,
    }
}

/// Copy of the running system's user language.
pub fn menu_strings() -> MenuStrings {
    select(sys_locale::get_locale().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_tags_pick_the_chinese_dictionary() {
        for tag in ["zh", "zh-CN", "ZH-hans", "zh_Hant"] {
            assert_eq!(select(Some(tag)), CHINESE, "tag {tag}");
        }
    }

    #[test]
    fn every_other_tag_picks_english() {
        for tag in [None, Some("en-US"), Some("de"), Some("ja-JP"), Some("")] {
            assert_eq!(select(tag), ENGLISH, "tag {tag:?}");
        }
    }

    #[test]
    fn the_dictionaries_cover_the_same_items_with_different_words() {
        let pairs = [
            (CHINESE.application, ENGLISH.application),
            (CHINESE.edit, ENGLISH.edit),
            (CHINESE.open_settings, ENGLISH.open_settings),
            (CHINESE.open_logs, ENGLISH.open_logs),
            (CHINESE.check_update, ENGLISH.check_update),
            (CHINESE.quit, ENGLISH.quit),
        ];
        for (chinese, english) in pairs {
            assert!(!chinese.is_empty() && !english.is_empty());
            assert_ne!(chinese, english);
        }
    }
}
