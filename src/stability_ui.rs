//! `/stability/ui` 监看界面（issue #20）：全自包含 HTML。
//!
//! 维护者硬性约束：零前端依赖、零 CDN、零构建链——因此页面以
//! `include_str!` 打进二进制，CSS/JS 全部内联在 `assets/stability_ui.html`
//! 单文件里。本模块只做一件事：把数据面开关注入占位符；展示逻辑（轮询
//! `/stability`、SVG 时间轴、表格渲染）都由页内内联 JS 完成。

/// 渲染监看页。`disabled` 表示 `[stability]` 数据面未启用：页面渲染成
/// 可读的禁用引导页（端点仍返回 200，不是报错页）。
pub fn render(disabled: bool) -> String {
    include_str!("../assets/stability_ui.html")
        .replace("__DATA_PLANE_DISABLED__", if disabled { "true" } else { "false" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_flag_is_injected_without_leftover_placeholder() {
        let on = render(false);
        let off = render(true);
        assert!(on.contains(r#"data-plane-disabled="false""#));
        assert!(off.contains(r#"data-plane-disabled="true""#));
        for page in [&on, &off] {
            assert!(
                !page.contains("__DATA_PLANE_DISABLED__"),
                "占位符必须被完全替换"
            );
        }
    }

    #[test]
    fn page_is_self_contained_with_all_mounts() {
        let page = render(false);
        // 零外部依赖：不得引用任何网络资源（createElementNS 的 xmlns 常量除外）。
        assert!(!page.contains(r#"src="http"#), "不得引用外部脚本");
        assert!(!page.contains(r#"href="http"#), "不得引用外部样式/资源");
        // 关键挂载点必须存在（集成测试对同一组 id 做断言）。
        for marker in [
            r#"id="dashboard""#,
            r#"id="disabled-page""#,
            r#"id="turns-table""#,
            r#"id="divergence-table""#,
        ] {
            assert!(page.contains(marker), "缺少挂载点 {marker}");
        }
    }
}
