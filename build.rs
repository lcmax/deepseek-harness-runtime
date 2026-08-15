//! 构建脚本：在 Windows 平台将 fav.ico 嵌入可执行文件作为图标
//!
//! 使用 winresource crate 完成图标注入，仅在 Windows 构建时生效

#[cfg(windows)]
fn main() {
    if std::path::Path::new("fav.ico").exists() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("fav.ico");
        res.compile().unwrap();
    }
}

#[cfg(not(windows))]
fn main() {
    // 非 Windows 平台无需特殊处理
}
