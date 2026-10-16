//! 构建脚本：保证 `web/dist` 存在，这样 `rust-embed` 才能把前端产物打进二进制
//! （开发文档 §0：构建产物由 Rust 侧 `embed` 打包）。
//!
//! 干净 checkout 里 `web/dist` 是被 gitignore 的，所以这里兜一个占位页，
//! 让 `cargo build` 不会因为「目录不存在」直接失败；真实产物由
//! `cd web && pnpm install && pnpm build` 生成，rebuild 时会自动重新嵌入。

use std::path::Path;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>prompt-vault</title></head>
<body style="font:16px/1.6 system-ui;margin:3rem auto;max-width:44em">
<h3>前端产物还没构建</h3>
<p>二进制里嵌的是占位页。先执行：</p>
<pre>cd web &amp;&amp; pnpm install &amp;&amp; pnpm build &amp;&amp; cd .. &amp;&amp; cargo build</pre>
<p>开发期也可以不重新编译，直接用 <code>--web-dir web/dist</code> 指向磁盘上的产物（默认就是它，磁盘优先）。</p>
<p>API 仍然可用：<a href="/api/health">/api/health</a></p>
</body></html>
"#;

fn main() {
    let dist = Path::new("web/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        if let Err(e) = std::fs::create_dir_all(dist) {
            println!("cargo:warning=创建 web/dist 失败: {e}");
        }
        if let Err(e) = std::fs::write(&index, PLACEHOLDER) {
            println!("cargo:warning=写入占位 index.html 失败: {e}");
        }
        println!("cargo:warning=web/dist/index.html 不存在，已写入占位页（前端未构建）");
    }
    println!("cargo:rerun-if-changed=web/dist");
    // 前端源码变化时也提醒重新嵌入（重新构建前端由人来做）
    println!("cargo:rerun-if-changed=web/src");
}
