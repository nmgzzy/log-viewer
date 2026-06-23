//! Windows 构建脚本：把图标与版本/产品信息嵌入 exe 资源。
//! 非 Windows 平台为空操作。winit 在未显式设置窗口图标时会复用 exe 的
//! 资源图标，故嵌入后 Explorer、任务栏与窗口标题栏都会带图标。

fn main() {
    #[cfg(windows)]
    {
        // 图标变更时重跑本脚本，确保新图标重新嵌入
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        // 图标存在才嵌入，缺失时不阻断构建（可后续替换 assets/icon.ico）
        if std::path::Path::new("assets/icon.ico").exists() {
            res.set_icon("assets/icon.ico");
        }
        // 文件属性（版本号由 winresource 自动取自 CARGO_PKG_VERSION）
        res.set("ProductName", "Log Viewer");
        res.set("FileDescription", "uf_log 日志查看器 / Log Viewer");
        res.set("CompanyName", "logviewer");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            // 不让资源编译失败阻断整个构建（例如缺少 rc 工具链时）
            println!("cargo:warning=windows resource compile failed: {e}");
        }
    }
}
