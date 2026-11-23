fn main() {
    // `mut` 只有 Windows 分支用到（替换为不带清单的属性集）
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut attributes = tauri_build::Attributes::new();

    // Windows：tauri-build 只给主 bin 嵌应用清单，测试 / bench 产物没有清单，
    // 加载器解析不到 comctl32 v6 的 SxS 依赖，测试进程一启动即
    // STATUS_ENTRYPOINT_NOT_FOUND（0xc0000139）。这里关掉它自己的注入，
    // 改用 embed-resource 以 `rustc-link-arg` 把同一份清单链进所有可链接目标
    // （tauri-apps/tauri#13419 / #13948）。
    #[cfg(windows)]
    {
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    }

    tauri_build::try_build(attributes).expect("tauri-build 失败");

    #[cfg(windows)]
    embed_resource::compile_for_everything("windows-app-manifest.rc", embed_resource::NONE)
        .manifest_required()
        .expect("嵌入 Windows 应用清单失败");
}
