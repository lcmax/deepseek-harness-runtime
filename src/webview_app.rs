//! # webview_app
//!
//! tao 窗口 + wry webview 桌面外壳：
//! - 启动即显示内置状态页（各阶段进度与流式日志，经 `eval` 更新）
//! - 构建就绪后经自定义 `dist://` 协议加载 `repo/dist` 静态资源（MIME 正确映射）
//!
//! ## 设计决策
//! - dist 目录通过 `Arc<Mutex<Option<PathBuf>>>` 共享给协议处理器：启动时为空（加载
//!   状态页），收到 Ready 事件后 main 线程注入路径并 `load_url("dist://localhost/")`。
//! - 协议处理器内做路径穿越防御（拒绝 `..` 组件），目录访问自动回落 `index.html`。

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use tao::event_loop::EventLoop;
use tao::window::{Window, WindowBuilder};
use wry::http::{Response, StatusCode};
use wry::{WebView, WebViewBuilder};

use crate::i18n::Lang;
use crate::UserEvent;

/// dist:// 协议名
const DIST_SCHEME: &str = "dist";

/// 就绪后加载的入口 URL（自定义协议 + localhost 伪主机 + index.html 入口）
///
/// 当前主加载路径已切换为 dsh web 宿主 URL（dist 是 build-only 壳）；
/// `dist://` 协议保留为静态产物备用通道，此常量暂无运行时调用方。
#[allow(dead_code)]
pub const DIST_ENTRY_URL: &str = "dist://localhost/index.html";

/// 运行时 `load_url` 实际使用的入口 URL（备用通道，当前主路径为宿主 URL）。
///
/// Windows/Android 的 WebView 后端不支持直接 Navigate 非标准 scheme：
/// wry 仅在**创建时**自动把 `dist://` 转换为 `http://dist.localhost/`，
/// 运行时 `load_url` 不做转换（直接 Navigate `dist://` 报 0x8007139F），
/// 因此这两个平台需主动使用 workaround URI；请求经 wry 拦截还原后仍以
/// `dist://` 到达自定义协议处理器，handler 无需改动。
///
/// # Returns
/// - Windows/Android: `http://dist.localhost/index.html`
/// - 其他平台: `dist://localhost/index.html`
#[allow(dead_code)]
pub fn runtime_entry_url() -> String {
    if cfg!(any(target_os = "windows", target_os = "android")) {
        format!("http://{DIST_SCHEME}.localhost/index.html")
    } else {
        DIST_ENTRY_URL.to_string()
    }
}

/// 共享的 dist 目录（None = 尚未就绪，仍在状态页阶段）
pub type SharedDistDir = Arc<Mutex<Option<PathBuf>>>;

/// 创建主窗口（启动即显示）。
///
/// # Arguments
/// * `event_loop` - tao 事件循环（UserEvent 为自定义事件）
pub fn create_window(event_loop: &EventLoop<UserEvent>) -> Window {
    WindowBuilder::new()
        .with_title("DeepSeek Harness Runtime")
        .with_inner_size(tao::dpi::LogicalSize::new(1180, 800))
        .build(event_loop)
        .expect("创建主窗口失败")
}

/// 创建 webview：初始加载内置状态页，并注册 `dist://` 自定义协议。
///
/// # Arguments
/// * `window` - 宿主窗口
/// * `dist_dir` - 共享 dist 目录（Ready 后注入实际路径）
/// * `lang` - 界面语言（决定状态页文案）
///
/// # Returns
/// 可 `eval` / `load_url` 的 [`WebView`]
///
/// # Errors
/// webview 初始化失败（WebView2 运行时缺失等）
pub fn create_webview(
    window: &Window,
    dist_dir: SharedDistDir,
    lang: Lang,
) -> anyhow::Result<WebView> {
    let dist_for_protocol = Arc::clone(&dist_dir);

    let webview = WebViewBuilder::new()
        .with_html(status_page_html(lang))
        .with_custom_protocol(DIST_SCHEME.into(), move |_id, request| {
            let path = request.uri().path().to_string();
            let dist = dist_for_protocol
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            match dist {
                Some(root) => serve_dist_file(&root, &path),
                None => text_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "构建尚未就绪，请等待同步与构建完成。",
                ),
            }
        })
        .build(window)
        .map_err(|e| anyhow::anyhow!("创建 WebView 失败: {e:?}"))?;

    Ok(webview)
}

/// 处理 `dist://` 请求：路径安全解析 → 读文件 → MIME 映射响应。
///
/// # Arguments
/// * `dist_root` - dist 根目录（repo/dist）
/// * `uri_path` - 请求路径（如 `/index.html`、`/assets/app.js`）
///
/// # Returns
/// 200 文件内容（含正确 Content-Type）或 404 文本
fn serve_dist_file(dist_root: &Path, uri_path: &str) -> Response<Cow<'static, [u8]>> {
    // 归一化："/" 或空 → index.html 入口
    let rel = if uri_path.is_empty() || uri_path == "/" {
        "index.html"
    } else {
        uri_path.trim_start_matches('/')
    };

    // 路径穿越防御：仅允许正常组件，拒绝 `..` 与前缀（盘符等）
    let mut safe_path = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => safe_path.push(c),
            Component::CurDir => {}
            _ => return not_found_response(),
        }
    }
    if safe_path.as_os_str().is_empty() {
        safe_path.push("index.html");
    }

    let full_path = dist_root.join(safe_path);

    // 目录访问回落 index.html（SPA 常见约定）
    let target = if full_path.is_dir() {
        full_path.join("index.html")
    } else {
        full_path
    };

    match std::fs::read(&target) {
        Ok(bytes) => {
            let mime = mime_for(&target);
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", mime)
                .body(Cow::Owned(bytes))
                .unwrap_or_else(|_| not_found_response())
        }
        Err(_) => not_found_response(),
    }
}

/// 404 响应（text/plain）。
fn not_found_response() -> Response<Cow<'static, [u8]>> {
    text_response(StatusCode::NOT_FOUND, "404 Not Found")
}

/// 简易文本响应构造。
fn text_response(status: StatusCode, text: &str) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Cow::Owned(text.as_bytes().to_vec()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Cow::Borrowed(&[][..]))
                .expect("构造空响应不会失败")
        })
}

/// 根据扩展名推断 MIME 类型。
///
/// # Arguments
/// * `path` - 文件路径
///
/// # Returns
/// MIME 字符串；未知扩展名返回 `application/octet-stream`
pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// 内置状态页 HTML：阶段进度 + 流式日志。
///
/// 页面暴露两个 JS 全局方法，由主线程 `eval` 调用：
/// - `dshSetStage(stage, state, detail)`：更新阶段状态
/// - `dshLog(line)`：追加一行日志
///
/// # Arguments
/// * `lang` - 界面语言（决定 `<html lang>` 属性、标题及各阶段文案）
pub fn status_page_html(lang: Lang) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="{}">
<head>
<meta charset="utf-8">
<title>{}</title>
<style>
  :root {{ color-scheme: dark; }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; padding: 32px;
    background: #0f1117; color: #e6e6e6;
    font-family: "Segoe UI", "Microsoft YaHei", sans-serif;
  }}
  h1 {{ font-size: 20px; margin: 0 0 24px; }}
  .stages {{ display: flex; flex-direction: column; gap: 10px; max-width: 640px; }}
  .stage {{
    display: flex; align-items: center; gap: 12px;
    padding: 10px 14px; border-radius: 8px;
    background: #1a1d27; border: 1px solid #262a36;
  }}
  .dot {{
    width: 10px; height: 10px; border-radius: 50%;
    background: #3a3f4f; flex: none;
  }}
  .stage.pending .dot {{ background: #3a3f4f; }}
  .stage.running .dot {{ background: #f0b429; animation: pulse 1s infinite alternate; }}
  .stage.done    .dot {{ background: #2ecc71; }}
  .stage.failed  .dot {{ background: #e74c3c; }}
  .stage .name {{ font-weight: 600; min-width: 140px; }}
  .stage .detail {{ color: #9aa0b0; font-size: 13px; flex: 1; }}
  .progress {{
    width: 160px; height: 4px; border-radius: 2px;
    background: #262a36; overflow: hidden; flex: none;
    visibility: hidden; margin-left: 12px;
  }}
  .progress.show {{ visibility: visible; }}
  .progress i {{
    display: block; height: 100%; border-radius: 2px;
    background: #4f8cff; width: 0%; transition: width .2s ease;
  }}
  .progress.indet i {{
    width: 40%; animation: slide 1.1s ease-in-out infinite;
  }}
  @keyframes slide {{
    0%   {{ margin-left: -40%; }}
    100% {{ margin-left: 100%; }}
  }}
  @keyframes pulse {{ from {{ opacity: .4; }} to {{ opacity: 1; }} }}
  pre#log {{
    margin-top: 24px; padding: 14px; max-height: 320px; overflow: auto;
    background: #05060a; color: #9fe3a1; border-radius: 8px;
    font-size: 12px; line-height: 1.6; white-space: pre-wrap; word-break: break-all;
  }}
</style>
</head>
<body>
<h1>{}</h1>
<div class="stages">
  <div class="stage pending" id="stage-node"><span class="dot"></span><span class="name">{}</span><span class="detail">{}</span><div class="progress" id="wrap-node"><i id="bar-node"></i></div></div>
  <div class="stage pending" id="stage-repo"><span class="dot"></span><span class="name">{}</span><span class="detail">{}</span><div class="progress" id="wrap-repo"><i id="bar-repo"></i></div></div>
  <div class="stage pending" id="stage-install"><span class="dot"></span><span class="name">{}</span><span class="detail">{}</span><div class="progress" id="wrap-install"><i id="bar-install"></i></div></div>
  <div class="stage pending" id="stage-build"><span class="dot"></span><span class="name">{}</span><span class="detail">{}</span><div class="progress" id="wrap-build"><i id="bar-build"></i></div></div>
  <div class="stage pending" id="stage-host"><span class="dot"></span><span class="name">{}</span><span class="detail">{}</span><div class="progress" id="wrap-host"><i id="bar-host"></i></div></div>
</div>
<pre id="log"></pre>
<script>
  window.dshSetStage = function (stage, state, detail) {{
    var el = document.getElementById('stage-' + stage);
    if (!el) return;
    el.className = 'stage ' + state;
    el.querySelector('.detail').textContent = detail;
  }};
  // value: -1 不确定（滑动动画）| 0..1 精确百分比 | null/undefined 隐藏
  window.dshSetProgress = function (stage, value, detail) {{
    var bar = document.getElementById('bar-' + stage);
    if (!bar) return;
    var wrap = bar.parentNode;
    var detailEl = bar.closest('.stage').querySelector('.detail');
    if (detail) detailEl.textContent = detail;
    if (value === null || value === undefined) {{
      wrap.classList.remove('show');
      return;
    }}
    wrap.classList.add('show');
    if (value < 0) {{
      wrap.classList.add('indet');
      return;
    }}
    wrap.classList.remove('indet');
    bar.style.width = (value * 100) + '%';
  }};
  window.dshLog = function (line) {{
    var log = document.getElementById('log');
    log.textContent += line + "\n";
    log.scrollTop = log.scrollHeight;
  }};
</script>
</body>
</html>"#,
        lang.lang_attr(),
        lang.title(),
        lang.heading(),
        lang.stage_node(),
        lang.pending_detail(),
        lang.stage_repo(),
        lang.pending_detail(),
        lang.stage_install(),
        lang.pending_detail(),
        lang.stage_build(),
        lang.pending_detail(),
        lang.stage_host(),
        lang.pending_detail(),
    )
}

/// 生成安全的 eval 片段：调用 `dshSetStage`。
///
/// # Arguments
/// * `stage` - 阶段 id（node/repo/install/build）
/// * `state` - pending/running/done/failed
/// * `detail` - 展示文案（自动 JSON 转义）
pub fn eval_set_stage(stage: &str, state: &str, detail: &str) -> String {
    format!(
        "window.dshSetStage && dshSetStage({}, {}, {});",
        serde_json::json!(stage),
        serde_json::json!(state),
        serde_json::json!(detail)
    )
}

/// 生成安全的 eval 片段：追加一行日志。
///
/// # Arguments
/// * `line` - 日志行（自动 JSON 转义）
pub fn eval_log(line: &str) -> String {
    format!(
        "window.dshLog && dshLog({});",
        serde_json::json!(line)
    )
}

/// 生成安全的 eval 片段：更新阶段进度条。
///
/// # Arguments
/// * `stage` - 阶段 id（node/repo/install/build/host）
/// * `progress` - `Some(p)` 精确百分比（0.0~1.0）；`None` 表示不确定（滑动动画）
/// * `detail` - 进度文案（如"下载中 32% …"），可传 `None` 保持原文案
pub fn eval_progress(stage: &str, progress: Option<f64>, detail: Option<&str>) -> String {
    let value = progress
        .map(|p| p.clamp(0.0, 1.0))
        .unwrap_or(-1.0);
    format!(
        "window.dshSetProgress && dshSetProgress({}, {}, {});",
        serde_json::json!(stage),
        serde_json::json!(value),
        serde_json::json!(detail)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MIME 映射正确性
    #[test]
    fn test_mime_for() {
        assert_eq!(mime_for(Path::new("a/index.html")), "text/html; charset=utf-8");
        assert_eq!(mime_for(Path::new("a/app.JS")), "text/javascript; charset=utf-8");
        assert_eq!(mime_for(Path::new("a/app.css")), "text/css; charset=utf-8");
        assert_eq!(mime_for(Path::new("a/logo.svg")), "image/svg+xml");
        assert_eq!(mime_for(Path::new("a/f.woff2")), "font/woff2");
        assert_eq!(mime_for(Path::new("a/data.json")), "application/json; charset=utf-8");
        assert_eq!(mime_for(Path::new("a/unknown.xyz")), "application/octet-stream");
        assert_eq!(mime_for(Path::new("a/img.webp")), "image/webp");
        assert_eq!(mime_for(Path::new("a/img.avif")), "image/avif");
    }

    /// dist 文件服务：正常读取 / 目录回落 index / 路径穿越拦截 / 404
    #[test]
    fn test_serve_dist_file() {
        let dir = std::env::temp_dir().join("dsh_webview_test_dist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::write(dir.join("assets/app.js"), b"console.log(1)").unwrap();

        // 正常文件
        let resp = serve_dist_file(&dir, "/index.html");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "text/html; charset=utf-8"
        );

        // 根路径 → index.html 回落
        let resp = serve_dist_file(&dir, "/");
        assert_eq!(resp.status(), StatusCode::OK);

        // 子目录文件
        let resp = serve_dist_file(&dir, "/assets/app.js");
        assert_eq!(resp.status(), StatusCode::OK);

        // 404
        let resp = serve_dist_file(&dir, "/nope.js");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // 路径穿越拦截
        let resp = serve_dist_file(&dir, "/../state.json");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// eval 片段生成（含引号转义）
    #[test]
    fn test_eval_snippets() {
        let s = eval_set_stage("build", "running", "正在构建 \"vite\"");
        assert!(s.contains("dshSetStage"));
        assert!(s.contains("\\\"vite\\\""));

        let l = eval_log("line'with\\quotes");
        assert!(l.contains("dshLog"));

        // 精确进度
        let p = eval_progress("repo", Some(0.5), Some("下载中 50%"));
        assert!(p.contains("dshSetProgress"));
        assert!(p.contains("0.5"));
        assert!(p.contains("下载中 50%"));

        // 不确定进度（None → -1）
        let indet = eval_progress("install", None, None);
        assert!(indet.contains("-1"));

        // 无文案
        let no_detail = eval_progress("build", Some(1.0), None);
        assert!(no_detail.contains("null"));
    }

    /// 状态页 HTML 包含进度条容器（每阶段一个）
    #[test]
    fn test_status_page_has_progress_bars() {
        let html = status_page_html(Lang::Zh);
        assert!(html.contains("id=\"bar-node\""));
        assert!(html.contains("id=\"bar-repo\""));
        assert!(html.contains("id=\"bar-install\""));
        assert!(html.contains("id=\"bar-build\""));
        assert!(html.contains("dshSetProgress"));
    }

    /// 状态页中文文案与 lang 属性
    #[test]
    fn test_status_page_html_zh() {
        let html = status_page_html(Lang::Zh);
        assert!(html.contains("等待开始"));
        assert!(!html.contains("Pending"));
        assert!(html.contains(r#"<html lang="zh-CN">"#));
    }

    /// 状态页英文文案与 lang 属性
    #[test]
    fn test_status_page_html_en() {
        let html = status_page_html(Lang::En);
        assert!(html.contains("Pending"));
        assert!(!html.contains("等待开始"));
        assert!(html.contains(r#"<html lang="en">"#));
    }

    /// 中英文状态页文案互不相同
    #[test]
    fn test_status_page_html_distinct() {
        assert_ne!(status_page_html(Lang::Zh), status_page_html(Lang::En));
    }
}
