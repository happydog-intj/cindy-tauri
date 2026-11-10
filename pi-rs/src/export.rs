// export_html：把会话数据（base64 JSON）嵌入原版的 HTML 模板。模板、样式、脚本和 marked / highlight.js 都取自
// 原版 1.4.0 的 export-html 目录，编译进二进制；主题变量是原版 dark 主题解析后的结果（export_theme.txt）。
use base64::Engine;
use serde_json::{json, Value};

const TEMPLATE: &str = include_str!("static/export/template.html");
const CSS: &str = include_str!("static/export/template.css");
const JS: &str = include_str!("static/export/template.js");
const MARKED: &str = include_str!("static/export/marked.min.js");
const MARKED_CJK: &str = include_str!("static/export/marked-cjk-friendly.min.js");
const HLJS: &str = include_str!("static/export/highlight.min.js");
const THEME_VARS: &str = include_str!("static/export_theme.txt");

/// 对应原版 template.replace(...)：按顺序各替换第一处占位符。注意先替换 CSS 再替换 JS，和原版一致。
pub fn generate_html(header: &Value, entries: &[Value], leaf: Option<&str>, system_prompt: &str, tools: &[Value]) -> String {
    let data = json!({
        "header": header,
        "entries": entries,
        "leafId": leaf,
        "systemPrompt": system_prompt,
        "tools": tools.iter().map(|t| json!({"name": t["name"], "description": t["description"], "parameters": t["parameters"]})).collect::<Vec<_>>(),
    });
    let b64 = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&data).unwrap_or_default());
    let css = CSS
        .replacen("{{THEME_VARS}}", THEME_VARS, 1)
        .replacen("{{BODY_BG}}", "rgb(0, 0, 0)", 1)
        .replacen("{{CONTAINER_BG}}", "rgb(0, 0, 0)", 1)
        .replacen("{{INFO_BG}}", "rgb(20, 15, 0)", 1);
    TEMPLATE
        .replacen("{{CSS}}", &css, 1)
        .replacen("{{JS}}", JS, 1)
        .replacen("{{SESSION_DATA}}", &b64, 1)
        .replacen("{{MARKED_JS}}", MARKED, 1)
        .replacen("{{MARKED_CJK_FRIENDLY_JS}}", MARKED_CJK, 1)
        .replacen("{{HIGHLIGHT_JS}}", HLJS, 1)
}
