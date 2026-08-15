//! # repo_manager
//!
//! 纯 HTTP 仓库增量同步：GitHub API 查询远端分支最新 commit SHA，与本地 `state.json`
//! 比对决定是否下载 codeload zip 重建 `repo/`（全程不调用本地 git）。
//!
//! ## 设计决策
//! - 增量判定：远端 SHA == `state.last_commit` 且 `repo/` 存在 → 跳过下载。
//! - zip 顶层目录剥离：codeload zip 内是 `{repo}-{ref}/` 单顶层结构，解压时剥掉。
//! - API 失败回退：网络/限流导致 API 查询失败时，直接全量重新下载，保证流程可用。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 目录操作重试次数（Windows Defender/索引服务可能瞬时锁定刚解压的目录）
const FS_RETRIES: u32 = 6;

/// 目录操作重试间隔基数（逐次翻倍：0.5s / 1s / 2s / 4s / 8s）
const FS_RETRY_DELAY: Duration = Duration::from_millis(500);

/// 解析后的仓库坐标
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// 仓库 owner（GitHub 用户/组织名）
    pub owner: String,
    /// 仓库名（不含 .git 后缀）
    pub repo: String,
}

/// 持久化状态（`<root>/state.json`）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    /// 上次同步成功时的远端分支 commit SHA（40 位十六进制）
    #[serde(default)]
    pub last_commit: String,
    /// 上次使用的 Node 版本（配置覆盖后用于感知版本变化）
    #[serde(default)]
    pub node_version: String,
    /// 宿主是否曾成功启动过（`dsh web` 就绪）。仅置 true 后二次启动才走快速路径跳过下载。
    #[serde(default)]
    pub host_started: bool,
}

/// 同步结果
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// 本次是否实际下载重建了 repo/（false 表示无更新跳过）
    pub updated: bool,
    /// 当前对应的远端 commit SHA（API 失败回退场景可能为 None）
    pub commit: Option<String>,
    /// 仓库源码目录（`<root>/repo/`）
    pub repo_dir: PathBuf,
}

/// 解析 GitHub 仓库 URL 为 owner/repo。
///
/// 支持形态：`https://github.com/{owner}/{repo}[.git]`、`http://...`、`github.com/{owner}/{repo}[.git]`
///
/// # Arguments
/// * `url` - 仓库地址字符串
///
/// # Errors
/// - 非 github.com 域名
/// - 路径不是 owner/repo 两段
pub fn parse_repo_url(url: &str) -> anyhow::Result<RepoInfo> {
    // 去掉协议头，统一处理
    let rest = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let host = parts.next().unwrap_or_default();
    let owner = parts.next().unwrap_or_default();
    let repo_raw = parts.next().unwrap_or_default();

    if host != "github.com" || owner.is_empty() || repo_raw.is_empty() {
        return Err(anyhow::anyhow!("无法解析 GitHub 仓库地址: {url}（期望 https://github.com/{{owner}}/{{repo}}.git）"));
    }
    if parts.next().is_some() {
        return Err(anyhow::anyhow!("仓库地址包含多余路径段: {url}"));
    }

    let repo = repo_raw.trim_end_matches(".git").to_string();
    if repo.is_empty() {
        return Err(anyhow::anyhow!("仓库名为空: {url}"));
    }

    Ok(RepoInfo {
        owner: owner.to_string(),
        repo,
    })
}

/// 构造 GitHub API 查询远端分支最新 commit 的 URL。
fn commits_api_url(info: &RepoInfo, branch: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/{}/commits/{branch}",
        info.owner, info.repo
    )
}

/// 构造 codeload zip 下载 URL。
fn codeload_zip_url(info: &RepoInfo, branch: &str) -> String {
    format!(
        "https://codeload.github.com/{}/{}/zip/refs/heads/{branch}",
        info.owner, info.repo
    )
}

/// 读取 `state.json`；文件不存在或损坏时返回默认值（视为首次同步）。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录
fn load_state(workspace_root: &Path) -> SyncState {
    let path = workspace_root.join("state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return SyncState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写入 `state.json`（原子语义：直接覆盖写）。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录
/// * `state` - 待持久化的状态
///
/// # Errors
/// 序列化或磁盘写入失败
fn save_state(workspace_root: &Path, state: &SyncState) -> anyhow::Result<()> {
    let path = workspace_root.join("state.json");
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| anyhow::anyhow!("序列化 state.json 失败: {e}"))?;
    std::fs::write(&path, text)
        .map_err(|e| anyhow::anyhow!("写入 {} 失败: {e}", path.display()))
}

/// 标记宿主已成功启动过。
///
/// 加载 `state.json` → 置 `host_started = true`（保留 `last_commit`/`node_version`）→ 写回。
/// 供 `run_bootstrap` 在 `dsh web` 宿主成功启动后调用，使二次启动可走快速路径跳过下载。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录
///
/// # Errors
/// - 读取 `state.json` 失败（文件不存在时视为首次，直接创建新状态）
/// - 序列化或写入失败
pub fn mark_host_started(workspace_root: &Path) -> anyhow::Result<()> {
    let mut state = load_state(workspace_root);
    if state.host_started {
        // 已标记过，无需重复写入
        return Ok(());
    }
    state.host_started = true;
    save_state(workspace_root, &state)
}

/// 查询远端分支最新 commit SHA。
///
/// # Arguments
/// * `info` - 仓库坐标
/// * `branch` - 分支名
///
/// # Returns
/// 40 位 commit SHA；API 请求/解析失败返回 `Err`
fn fetch_remote_commit(info: &RepoInfo, branch: &str) -> anyhow::Result<String> {
    let resp: CommitResponse = crate::http_util::get_json(&commits_api_url(info, branch))?;
    if resp.sha.len() != 40 {
        return Err(anyhow::anyhow!("API 返回的 SHA 长度异常: {}", resp.sha));
    }
    Ok(resp.sha)
}

/// GitHub commits API 响应中的有效字段
#[derive(Deserialize)]
struct CommitResponse {
    /// commit SHA（40 位十六进制）
    sha: String,
}

/// 查询仓库的默认分支名（`/repos/{owner}/{repo}` 的 `default_branch` 字段）。
///
/// # Arguments
/// * `info` - 仓库坐标
///
/// # Returns
/// 默认分支名（如 `master` / `main`）；API 失败返回 `Err`
fn fetch_default_branch(info: &RepoInfo) -> anyhow::Result<String> {
    let resp: RepoDetailResponse = crate::http_util::get_json(&format!(
        "https://api.github.com/repos/{}/{}",
        info.owner, info.repo
    ))?;
    if resp.default_branch.is_empty() {
        return Err(anyhow::anyhow!("API 返回的 default_branch 为空"));
    }
    Ok(resp.default_branch)
}

/// GitHub 仓库详情 API 响应中的有效字段
#[derive(Deserialize)]
struct RepoDetailResponse {
    /// 仓库默认分支名
    default_branch: String,
}

/// 带重试的目录移动（`fs::rename`）。
///
/// Windows 上 Defender/索引服务可能瞬时锁定刚解压的大目录，导致 rename 返回
/// error 5（拒绝访问）；指数退避重试通常可自行恢复。
///
/// # Arguments
/// * `src` - 源目录
/// * `dst` - 目标路径（同卷移动）
///
/// # Errors
/// 重试耗尽后仍失败则返回最后一次错误
fn rename_dir_with_retry(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let mut delay = FS_RETRY_DELAY;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..FS_RETRIES {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // 最后一次失败直接返回，不再等待
                if attempt + 1 == FS_RETRIES {
                    last_err = Some(e);
                    break;
                }
                eprintln!(
                    "目录移动被占用（第 {} 次，{delay:?} 后重试）: {} -> {}: {e}",
                    attempt + 1,
                    src.display(),
                    dst.display()
                );
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::anyhow!(
        "移动 {} -> {} 失败: {}",
        src.display(),
        dst.display(),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// 带重试的目录删除（同样防御瞬时句柄锁定）。
///
/// # Arguments
/// * `dir` - 待删除目录（不存在视为成功）
///
/// # Errors
/// 重试耗尽后仍失败则返回错误
fn remove_dir_all_with_retry(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut delay = FS_RETRY_DELAY;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..FS_RETRIES {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt + 1 == FS_RETRIES {
                    last_err = Some(e);
                    break;
                }
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::anyhow!(
        "删除 {} 失败: {}",
        dir.display(),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// 下载 codeload zip 并解压重建 `repo/`（剥离 zip 顶层目录）。
///
/// # Arguments
/// * `info` - 仓库坐标
/// * `branch` - 分支名
/// * `repo_dir` - 目标目录 `<root>/repo/`
///
/// # Errors
/// - 下载失败
/// - 清空或解压失败
fn rebuild_repo_from_zip(info: &RepoInfo, branch: &str, repo_dir: &Path) -> anyhow::Result<()> {
    let zip_url = codeload_zip_url(info, branch);

    // zip 暂存到 repo/ 同级的临时文件
    let zip_path = repo_dir.with_extension("zip.tmp");
    crate::http_util::download_file(&zip_url, &zip_path)?;

    // 先清空旧 repo/（避免新旧文件混杂），再解压到父目录后剥离顶层
    let staging_dir = repo_dir.with_extension("extract.tmp");
    let _ = std::fs::remove_dir_all(&staging_dir);
    std::fs::create_dir_all(&staging_dir)
        .map_err(|e| anyhow::anyhow!("创建临时目录 {} 失败: {e}", staging_dir.display()))?;

    let result = (|| {
        crate::node_manager::extract_zip(&zip_path, &staging_dir)?;

        // zip 内是单一顶层目录（{repo}-{ref}/），剥离后整体移动为 repo/
        let mut entries = std::fs::read_dir(&staging_dir)?
            .filter_map(|e| e.ok())
            .collect::<Vec<_>>();
        if entries.len() != 1 || !entries[0].path().is_dir() {
            return Err(anyhow::anyhow!(
                "zip 结构异常：期望单一顶层目录，实际 {} 个条目",
                entries.len()
            ));
        }
        let top_dir = entries.remove(0).path();

        // 删除旧 repo/ 后整体移动（均带重试，防御 Defender 瞬时锁定）
        remove_dir_all_with_retry(repo_dir)?;
        rename_dir_with_retry(&top_dir, repo_dir)?;
        Ok(())
    })();

    // 清理临时产物（无论成败）
    let _ = std::fs::remove_file(&zip_path);
    let _ = std::fs::remove_dir_all(&staging_dir);

    result
}

/// 执行增量同步主流程。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录（`.runtime/`）
/// * `repo_url` - 仓库地址
/// * `branch` - 分支名
/// * `node_version` - 当前配置的 Node 版本（写入 state）
/// * `on_status` - 阶段状态回调（用于状态页/日志）
///
/// # Returns
/// [`SyncResult`]：`updated=true` 表示本次重建了 repo/；`commit=None` 表示 API 失败走了回退
///
/// # Errors
/// - 仓库地址无法解析
/// - 需要下载时下载/解压失败（API 失败本身不算错误，走回退）
pub fn sync_repo(
    workspace_root: &Path,
    repo_url: &str,
    branch: &str,
    node_version: &str,
    on_status: &dyn Fn(String),
) -> anyhow::Result<SyncResult> {
    let info = parse_repo_url(repo_url)?;
    let repo_dir = workspace_root.join("repo");

    // 尝试 API 查询远端 commit；分支不存在时回退仓库默认分支重试；仍失败则全量下载
    let mut effective_branch = branch.to_string();
    let mut remote_commit = fetch_remote_commit(&info, &effective_branch);
    if remote_commit.is_err() {
        // 配置分支查询失败：常见原因是分支名写错（如 main vs master）
        // → 查询仓库 default_branch 并用其重试
        match fetch_default_branch(&info) {
            Ok(default_branch) if default_branch != effective_branch => {
                on_status(format!(
                    "分支 {effective_branch} 查询失败，回退仓库默认分支 {default_branch}"
                ));
                effective_branch = default_branch;
                remote_commit = fetch_remote_commit(&info, &effective_branch);
            }
            Ok(_) => {}
            Err(db_err) => {
                remote_commit = Err(db_err);
            }
        }
    }

    let remote_commit = match remote_commit {
        Ok(sha) => Some(sha),
        Err(e) => {
            on_status(format!("GitHub API 查询失败（{e}），回退为直接重新下载"));
            None
        }
    };

    let state = load_state(workspace_root);

    // 快速路径：宿主曾成功启动 + 远端 commit 一致 + repo/ 存在 → 跳过下载解压，
    // 直接复用本地 repo 启动宿主（轻量检查后跳过，避免重复下载重建）
    if let Some(sha) = remote_commit.as_ref() {
        if *sha == state.last_commit && state.host_started && repo_dir.exists() {
            on_status(format!(
                "版本一致（commit {sha}），复用本地仓库直接启动宿主"
            ));
            return Ok(SyncResult {
                updated: false,
                commit: remote_commit,
                repo_dir,
            });
        }
    }

    // 场景：首次同步 或 有更新 或 API 失败回退 → 下载 zip 重建 repo/
    on_status(format!(
        "正在同步仓库 {}/{}（分支 {effective_branch}）...",
        info.owner, info.repo
    ));
    rebuild_repo_from_zip(&info, &effective_branch, &repo_dir)?;
    on_status("仓库同步完成".to_string());

    // 更新并持久化 state
    let new_state = SyncState {
        last_commit: remote_commit.clone().unwrap_or_default(),
        node_version: node_version.to_string(),
        host_started: false,
    };
    save_state(workspace_root, &new_state)?;

    Ok(SyncResult {
        updated: true,
        commit: remote_commit,
        repo_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库 URL 解析：标准 .git 形态
    #[test]
    fn test_parse_repo_url_git_suffix() {
        let info = parse_repo_url("https://github.com/deepseek-ai/deepseek-harness.git")
            .unwrap();
        assert_eq!(info.owner, "deepseek-ai");
        assert_eq!(info.repo, "deepseek-harness");
    }

    /// 仓库 URL 解析：无 .git 后缀 / 无协议头
    #[test]
    fn test_parse_repo_url_variants() {
        let info = parse_repo_url("https://github.com/octocat/Hello-World").unwrap();
        assert_eq!(info.repo, "Hello-World");

        let info = parse_repo_url("github.com/a/b.git").unwrap();
        assert_eq!(info.owner, "a");
        assert_eq!(info.repo, "b");
    }

    /// 仓库 URL 解析：非法输入报错
    #[test]
    fn test_parse_repo_url_invalid() {
        assert!(parse_repo_url("https://gitlab.com/a/b").is_err());
        assert!(parse_repo_url("https://github.com/onlyowner").is_err());
        assert!(parse_repo_url("https://github.com/a/b/extra").is_err());
        assert!(parse_repo_url("").is_err());
    }

    /// 端点 URL 构造正确
    #[test]
    fn test_endpoint_urls() {
        let info = RepoInfo {
            owner: "foo".into(),
            repo: "bar".into(),
        };
        assert_eq!(
            commits_api_url(&info, "main"),
            "https://api.github.com/repos/foo/bar/commits/main"
        );
        assert_eq!(
            codeload_zip_url(&info, "dev"),
            "https://codeload.github.com/foo/bar/zip/refs/heads/dev"
        );
    }

    /// 仓库详情响应的 default_branch 字段解析
    #[test]
    fn test_repo_detail_response_parses_default_branch() {
        let json = r#"{"full_name":"deepseek-ai/deepseek-harness","default_branch":"master"}"#;
        let resp: RepoDetailResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.default_branch, "master");
    }

    /// state.json 读写回环
    #[test]
    fn test_state_roundtrip() {
        let dir = std::env::temp_dir().join("dsh_repo_test_state");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 首次：无文件 → 默认值
        assert_eq!(load_state(&dir).last_commit, "");

        let state = SyncState {
            last_commit: "a".repeat(40),
            node_version: "24.19.0".into(),
            host_started: false,
        };
        save_state(&dir, &state).unwrap();
        assert_eq!(load_state(&dir), state);

        // 损坏 JSON → 回落默认值（不 panic）
        std::fs::write(dir.join("state.json"), "{broken").unwrap();
        assert_eq!(load_state(&dir).last_commit, "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧格式 state.json（无 host_started 字段）→ 加载后 host_started 为 false
    #[test]
    fn test_state_legacy_defaults_host_started_false() {
        let dir = std::env::temp_dir().join("dsh_repo_test_legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 仅含旧字段的 JSON，无 host_started
        std::fs::write(
            dir.join("state.json"),
            r#"{"last_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","node_version":"24.19.0"}"#,
        )
        .unwrap();

        let state = load_state(&dir);
        assert_eq!(state.last_commit, "a".repeat(40));
        assert_eq!(state.node_version, "24.19.0");
        assert!(!state.host_started, "旧格式缺省 host_started 应为 false");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mark_host_started：置位后 load_state 返回 host_started == true，且保留原有字段
    #[test]
    fn test_mark_host_started() {
        let dir = std::env::temp_dir().join("dsh_repo_test_mark");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let state = SyncState {
            last_commit: "b".repeat(40),
            node_version: "24.19.0".into(),
            host_started: false,
        };
        save_state(&dir, &state).unwrap();

        mark_host_started(&dir).unwrap();
        let loaded = load_state(&dir);
        assert!(loaded.host_started, "mark_host_started 后应为 true");
        assert_eq!(loaded.last_commit, "b".repeat(40), "应保留 last_commit");
        assert_eq!(loaded.node_version, "24.19.0", "应保留 node_version");

        // 已标记后再次调用：幂等，不报错
        mark_host_started(&dir).unwrap();
        assert!(load_state(&dir).host_started);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mark_host_started：无既有 state.json（首次）也能成功置位
    #[test]
    fn test_mark_host_started_from_missing() {
        let dir = std::env::temp_dir().join("dsh_repo_test_mark_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        mark_host_started(&dir).unwrap();
        let loaded = load_state(&dir);
        assert!(loaded.host_started);
        assert_eq!(loaded.last_commit, "", "无既有记录时 last_commit 为空");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
