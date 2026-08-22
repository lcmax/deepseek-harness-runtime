//! # i18n
//!
//! 负责系统 locale 检测与界面文案表，实现内置状态页中/英文案自动切换。
//!
//! ## 检测规则
//! - locale 以 `zh` 开头（如 `zh-CN`、`zh_TW`、`zhCN`）→ [`Lang::Zh`]
//! - 其余种类（`en-US`、`fr-FR` 等）→ [`Lang::En`]
//! - 检测失败（`sys_locale::get_locale()` 返回 `None`）→ 默认 [`Lang::En`]
//!
//! 文案方法均返回 `&'static str`，由调用方直接嵌入 HTML。

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 中文
    Zh,
    /// 英文
    En,
}

impl Lang {
    /// 依据系统 locale 检测当前界面语言。
    ///
    /// # Returns
    /// 检测失败或无匹配时回退为 [`Lang::En`]
    pub fn detect() -> Lang {
        match sys_locale::get_locale() {
            None => Lang::En,
            Some(s) => Self::from_locale(&s),
        }
    }

    /// 依据 locale 字符串判定语言。
    ///
    /// # Arguments
    /// * `locale` - 形如 `zh-CN` / `en-US` 的 locale 字符串
    ///
    /// # Returns
    /// locale 小写化后以 `zh` 开头 → [`Lang::Zh`]，否则 [`Lang::En`]
    pub fn from_locale(locale: &str) -> Lang {
        if locale.to_ascii_lowercase().starts_with("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }

    /// 返回 HTML `<html lang>` 属性值。
    ///
    /// # Returns
    /// [`Lang::Zh`] → `"zh-CN"`；[`Lang::En`] → `"en"`
    pub fn lang_attr(self) -> &'static str {
        match self {
            Lang::Zh => "zh-CN",
            Lang::En => "en",
        }
    }

    /// 各阶段的「等待开始」初始 detail 文案。
    pub fn pending_detail(self) -> &'static str {
        match self {
            Lang::Zh => "等待开始",
            Lang::En => "Pending",
        }
    }

    /// 阶段名：Node 运行时。
    pub fn stage_node(self) -> &'static str {
        match self {
            Lang::Zh => "Node 运行时",
            Lang::En => "Node Runtime",
        }
    }

    /// 阶段名：同步仓库。
    pub fn stage_repo(self) -> &'static str {
        match self {
            Lang::Zh => "同步仓库",
            Lang::En => "Sync Repo",
        }
    }

    /// 阶段名：安装依赖。
    pub fn stage_install(self) -> &'static str {
        match self {
            Lang::Zh => "安装依赖",
            Lang::En => "Install Dependencies",
        }
    }

    /// 阶段名：构建产物。
    pub fn stage_build(self) -> &'static str {
        match self {
            Lang::Zh => "构建产物",
            Lang::En => "Build Artifacts",
        }
    }

    /// 阶段名：启动宿主。
    pub fn stage_host(self) -> &'static str {
        match self {
            Lang::Zh => "启动宿主",
            Lang::En => "Start Host",
        }
    }

    /// 页面标题。
    pub fn title(self) -> &'static str {
        match self {
            Lang::Zh => "DeepSeek Harness Runtime - 启动中",
            Lang::En => "DeepSeek Harness Runtime - Starting",
        }
    }

    /// 页面主标题。
    pub fn heading(self) -> &'static str {
        match self {
            Lang::Zh => "DeepSeek Harness Runtime · 正在准备应用…",
            Lang::En => "DeepSeek Harness Runtime · Preparing application…",
        }
    }

    /// 阶段 detail：准备 Node 运行时。`{}` 由调用方以版本号插值。
    pub fn detail_prepare_node(self) -> &'static str {
        match self {
            Lang::Zh => "准备 Node v{}",
            Lang::En => "Preparing Node v{}",
        }
    }

    /// 阶段 detail：Node 就绪。`{}` 由调用方以版本号插值。
    pub fn detail_node_ready(self) -> &'static str {
        match self {
            Lang::Zh => "Node v{} 就绪",
            Lang::En => "Node v{} ready",
        }
    }

    /// 阶段 detail：检查远端更新。
    pub fn detail_check_remote(self) -> &'static str {
        match self {
            Lang::Zh => "检查远端更新",
            Lang::En => "Checking remote updates",
        }
    }

    /// 阶段 detail：安装依赖。
    pub fn detail_installing(self) -> &'static str {
        match self {
            Lang::Zh => "安装依赖",
            Lang::En => "Installing dependencies",
        }
    }

    /// 阶段 detail：依赖安装完成。
    pub fn detail_install_done(self) -> &'static str {
        match self {
            Lang::Zh => "依赖安装完成",
            Lang::En => "Dependencies installed",
        }
    }

    /// 阶段 detail：执行构建脚本。
    pub fn detail_building(self) -> &'static str {
        match self {
            Lang::Zh => "执行构建脚本",
            Lang::En => "Running build script",
        }
    }

    /// 阶段 detail：构建完成。
    pub fn detail_build_done(self) -> &'static str {
        match self {
            Lang::Zh => "构建完成",
            Lang::En => "Build complete",
        }
    }

    /// 阶段 detail：启动 dsh web 宿主。
    pub fn detail_starting_host(self) -> &'static str {
        match self {
            Lang::Zh => "启动 dsh web 宿主",
            Lang::En => "Starting dsh web host",
        }
    }

    /// 阶段 detail：宿主就绪。`{}` 由调用方以宿主 URL 插值。
    pub fn detail_host_ready(self) -> &'static str {
        match self {
            Lang::Zh => "宿主就绪 {}",
            Lang::En => "Host ready {}",
        }
    }

    /// 同步结果：快速路径，版本一致直接复用本地启动宿主。
    pub fn detail_sync_cached(self) -> &'static str {
        match self {
            Lang::Zh => "版本一致，复用本地启动宿主",
            Lang::En => "Version matches, reusing local to start host",
        }
    }

    /// 同步结果：已更新至某提交。`{}` 由调用方以 SHA 插值。
    pub fn detail_sync_updated_sha(self) -> &'static str {
        match self {
            Lang::Zh => "已更新至 {}",
            Lang::En => "Updated to {}",
        }
    }

    /// 同步结果：API 回退后重新同步。
    pub fn detail_sync_updated_fallback(self) -> &'static str {
        match self {
            Lang::Zh => "已重新同步（API 回退）",
            Lang::En => "Re-synced (API fallback)",
        }
    }

    /// 构建结果：快速路径，版本一致复用已有构建产物跳过构建。
    pub fn detail_build_cached(self) -> &'static str {
        match self {
            Lang::Zh => "版本一致，复用已有构建产物，跳过构建",
            Lang::En => "Version matches, reusing existing build, skipping build",
        }
    }

    /// 进度：仓库同步开始。
    pub fn progress_sync_start(self) -> &'static str {
        match self {
            Lang::Zh => "开始同步...",
            Lang::En => "Starting sync...",
        }
    }

    /// 进度：仓库下载百分比（有总长）。
    pub fn progress_download_pct(self, pct: u64, done_mb: f64, total_mb: f64) -> String {
        match self {
            Lang::Zh => format!("下载中 {pct}% （{done_mb:.1} MB / {total_mb:.1} MB）"),
            Lang::En => format!("Downloading {pct}% ({done_mb:.1} MB / {total_mb:.1} MB)"),
        }
    }

    /// 进度：仓库下载中（无总长）。
    pub fn progress_download_mb(self, done_mb: f64) -> String {
        match self {
            Lang::Zh => format!("下载中 {done_mb:.1} MB ..."),
            Lang::En => format!("Downloading {done_mb:.1} MB ..."),
        }
    }

    /// 进度：正在解压仓库。
    pub fn progress_extracting(self) -> &'static str {
        match self {
            Lang::Zh => "正在解压仓库...",
            Lang::En => "Extracting repository...",
        }
    }

    /// 进度：仓库同步完成。
    pub fn progress_sync_done(self) -> &'static str {
        match self {
            Lang::Zh => "仓库同步完成",
            Lang::En => "Repository synced",
        }
    }

    /// 进度：安装依赖完成。
    pub fn progress_install_done(self) -> &'static str {
        match self {
            Lang::Zh => "依赖安装完成",
            Lang::En => "Dependencies installed",
        }
    }

    /// 进度：构建完成。
    pub fn progress_build_done(self) -> &'static str {
        match self {
            Lang::Zh => "构建完成",
            Lang::En => "Build complete",
        }
    }

    /// 进度：命令输出行数（install/build 阶段的活动进度提示）。
    pub fn progress_lines(self, n: u64) -> String {
        match self {
            Lang::Zh => format!("（已输出 {n} 行）"),
            Lang::En => format!("({n} lines output)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// locale → Lang 映射正确性
    #[test]
    fn test_from_locale() {
        assert_eq!(Lang::from_locale("zh-CN"), Lang::Zh);
        assert_eq!(Lang::from_locale("zh_TW"), Lang::Zh);
        assert_eq!(Lang::from_locale("zhCN"), Lang::Zh);
        assert_eq!(Lang::from_locale("en-US"), Lang::En);
        assert_eq!(Lang::from_locale("en"), Lang::En);
        assert_eq!(Lang::from_locale(""), Lang::En);
        assert_eq!(Lang::from_locale("fr-FR"), Lang::En);
    }

    /// lang_attr 返回值正确性
    #[test]
    fn test_lang_attr() {
        assert_eq!(Lang::Zh.lang_attr(), "zh-CN");
        assert_eq!(Lang::En.lang_attr(), "en");
    }

    /// 中文文案取值正确
    #[test]
    fn test_zh_texts() {
        assert_eq!(Lang::Zh.pending_detail(), "等待开始");
        assert_eq!(Lang::Zh.stage_node(), "Node 运行时");
        assert_eq!(Lang::Zh.stage_repo(), "同步仓库");
        assert_eq!(Lang::Zh.stage_install(), "安装依赖");
        assert_eq!(Lang::Zh.stage_build(), "构建产物");
        assert_eq!(Lang::Zh.stage_host(), "启动宿主");
        assert_eq!(Lang::Zh.title(), "DeepSeek Harness Runtime - 启动中");
        assert_eq!(Lang::Zh.heading(), "DeepSeek Harness Runtime · 正在准备应用…");
    }

    /// 英文文案取值正确
    #[test]
    fn test_en_texts() {
        assert_eq!(Lang::En.pending_detail(), "Pending");
        assert_eq!(Lang::En.stage_node(), "Node Runtime");
        assert_eq!(Lang::En.stage_repo(), "Sync Repo");
        assert_eq!(Lang::En.stage_install(), "Install Dependencies");
        assert_eq!(Lang::En.stage_build(), "Build Artifacts");
        assert_eq!(Lang::En.stage_host(), "Start Host");
        assert_eq!(Lang::En.title(), "DeepSeek Harness Runtime - Starting");
        assert_eq!(Lang::En.heading(), "DeepSeek Harness Runtime · Preparing application…");
    }

    /// 阶段 detail 文案取值正确
    #[test]
    fn test_detail_texts() {
        assert!(Lang::Zh.detail_prepare_node().contains("准备 Node"));
        assert!(Lang::En.detail_prepare_node().contains("Preparing Node"));
        assert!(Lang::Zh.detail_sync_cached().contains("版本一致"));
        assert!(Lang::En.detail_sync_cached().contains("Version matches"));
        assert!(Lang::Zh.detail_build_cached().contains("版本一致"));
        assert!(Lang::En.detail_build_cached().contains("Version matches"));
    }

    /// 进度文案取值正确（中英有别，数字插值正确）
    #[test]
    fn test_progress_texts() {
        assert_eq!(Lang::Zh.progress_sync_start(), "开始同步...");
        assert_eq!(Lang::En.progress_sync_start(), "Starting sync...");
        assert_eq!(Lang::Zh.progress_extracting(), "正在解压仓库...");
        assert_eq!(Lang::En.progress_extracting(), "Extracting repository...");

        let zh_pct = Lang::Zh.progress_download_pct(35, 1.2, 3.4);
        assert!(zh_pct.contains("35%"));
        let en_pct = Lang::En.progress_download_pct(35, 1.2, 3.4);
        assert!(en_pct.contains("35%"));
        assert_ne!(zh_pct, en_pct);

        assert!(Lang::Zh.progress_lines(3).contains("3"));
        assert!(Lang::En.progress_lines(3).contains("3"));
        assert_ne!(Lang::Zh.progress_lines(3), Lang::En.progress_lines(3));
    }

    /// 中英文文案互不相同
    #[test]
    fn test_zh_vs_en_distinct() {
        assert_ne!(Lang::Zh.pending_detail(), Lang::En.pending_detail());
        assert_ne!(Lang::Zh.stage_node(), Lang::En.stage_node());
        assert_ne!(Lang::Zh.stage_repo(), Lang::En.stage_repo());
        assert_ne!(Lang::Zh.stage_install(), Lang::En.stage_install());
        assert_ne!(Lang::Zh.stage_build(), Lang::En.stage_build());
        assert_ne!(Lang::Zh.stage_host(), Lang::En.stage_host());
    }
}