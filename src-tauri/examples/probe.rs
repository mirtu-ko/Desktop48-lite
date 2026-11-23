//! 无 GUI 的端到端探针：走与 App 完全相同的桥（`BridgeContext::dispatch`）打真实接口。
//! `cargo run --example probe`（或 `npm run probe`）。
//!
//! 验证两件事：
//! 1. `netRequest`（渲染层正式路径）能过服务端客户端身份判定、限流与白名单；
//! 2. MVP 演示通道（Rust 直连解析）仍可用。
//!
//! UA / appInfo 拼错会直接 403 / HTML 拦截页，这是迁移里最容易静默失败的一环。

use desktop48_lite_lib::{api::LIVE_LIST_URL, bridge::BridgeContext, database::Database};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("探针失败: {err}");
        std::process::exit(1);
    }
}

fn list_body(record: bool) -> String {
    json!({
        "next": "0",
        "loadMore": "true",
        "userId": "0",
        "teamId": "0",
        "groupId": "0",
        "record": if record { "true" } else { "false" },
    })
    .to_string()
}

/// 只读检查 database.json：成员数、配置、名单解析、成员树分组。
/// 必须由调用方显式给路径——探针默认不碰用户真实数据库。
fn report_database(path: &std::path::Path) -> Result<(), String> {
    let db = Database::load(path);
    if !db.has_members() {
        println!(
            "database: {} · 成员库为空（尚未同步或迁移失败）",
            path.display()
        );
        return Ok(());
    }
    let tree = db.member_tree();
    let groups = tree.as_array().map(Vec::len).unwrap_or(0);
    let teams: usize = tree
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .map(|group| {
                    group
                        .get("children")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);
    let blocked = db.member_flags("blocked")?;
    let followed = db.member_flags("followed")?;
    println!(
        "database: {} 人 · 树 {} 团 / {} 队 · 屏蔽 {} / 关注 {} · 弹幕={} / 托盘={}",
        db.member_count(),
        groups,
        teams,
        blocked.as_array().map(Vec::len).unwrap_or(0),
        followed.as_array().map(Vec::len).unwrap_or(0),
        db.config("danmakuPlacement")?.as_str().unwrap_or(""),
        db.config("trayOnClose")?.as_str().unwrap_or(""),
    );
    Ok(())
}

/// 在临时数据库上验证写链路：配置改回、名单幂等、备份存在、重载可见。
/// 只应由 `D48_PROBE_MIGRATE=1` 调用（目标是迁移出来的临时副本）。
fn exercise_database_writes(path: &std::path::Path) -> Result<(), String> {
    let db = Database::load(path);
    let before = db
        .config("danmakuPlacement")?
        .as_str()
        .unwrap_or("video")
        .to_string();
    let next = if before == "left" { "video" } else { "left" };
    db.set_config("danmakuPlacement", next)?;
    // 从成员树拿一个真实 userId：allmembers 的首条不一定带 userId。
    let user_id = db
        .member_tree()
        .as_array()
        .and_then(|groups| groups.first())
        .and_then(|group| group.get("children"))
        .and_then(Value::as_array)
        .and_then(|teams| teams.first())
        .and_then(|team| team.get("children"))
        .and_then(Value::as_array)
        .and_then(|members| members.first())
        .and_then(|member| member.get("value"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<i64>().ok());
    let mut added = false;
    if let Some(user_id) = user_id {
        let before_ids = db.member_flag_ids("blocked")?;
        db.add_member_flag("blocked", user_id)?;
        let after_ids = db.member_flag_ids("blocked")?;
        added = after_ids.contains(&user_id);
        println!(
            "database 写: 配置 {} → {} · 屏蔽 {} → {} 条（用户 {}）",
            before,
            next,
            before_ids.len(),
            after_ids.len(),
            user_id
        );
    }
    let reloaded = Database::load(path);
    if reloaded.config("danmakuPlacement")?.as_str() != Some(next) {
        return Err("写入后重载配置不一致".to_string());
    }
    let backup = format!("{}.bak", path.display());
    if !std::path::Path::new(&backup).exists() {
        return Err("写入后缺少 .bak 备份".to_string());
    }
    if user_id.is_some() && !added {
        return Err("屏蔽名单写入后未生效".to_string());
    }
    println!("database 写: 重载通过，.bak 已生成");
    Ok(())
}

fn envelope_content(payload: &Value) -> Result<Value, String> {
    let status = payload.get("status").and_then(Value::as_i64).unwrap_or(0);
    let success = payload
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !success || status != 200 {
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("接口返回失败");
        return Err(format!("{message}（status: {status}）"));
    }
    Ok(payload.get("content").cloned().unwrap_or(Value::Null))
}

async fn run() -> Result<(), String> {
    let data_dir = std::env::temp_dir().join("desktop48-lite-probe");
    let ctx = BridgeContext::new_isolated(&data_dir)?;
    println!("设备 ID: {}", ctx.device_id());
    println!(
        "已注册通道: {}",
        desktop48_lite_lib::bridge::CHANNELS.join(", ")
    );

    // 0) 可选：FFmpeg 一键下载（写探针临时目录，不碰用户真实配置）。
    if std::env::var("D48_PROBE_FFMPEG_DOWNLOAD").ok().as_deref() == Some("1") {
        println!("ffmpeg: 开始下载到探针目录...");
        let dir = ctx.dispatch("downloadFfmpeg", &[]).await?;
        let dir = dir.as_str().unwrap_or_default();
        let status = ctx.dispatch("getFfmpegStatus", &[json!(dir)]).await?;
        println!("ffmpeg: 目录={dir} 状态={status}");
    }

    // 1) 渲染层正式路径：通用代理（白名单 + App 身份 + 限流 + 非 2xx 原样返回）
    let response = ctx
        .dispatch(
            "netRequest",
            &[json!({
                "url": LIVE_LIST_URL,
                "method": "POST",
                "headers": { "Content-Type": "application/json; charset=UTF-8" },
                "body": list_body(true),
            })],
        )
        .await?;
    let status = response.get("status").and_then(Value::as_u64).unwrap_or(0);
    let body = response
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    println!("netRequest: status={status} 正文={} 字节", body.len());
    let payload: Value =
        serde_json::from_str(&body).map_err(|err| format!("netRequest 回包不是 JSON: {err}"))?;
    let content = envelope_content(&payload)?;
    let records = content
        .get("liveList")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("  netRequest 回放列表: {records} 条");

    // 3) 图片转 data URL（生日贺卡用的跨域图片路径）：封面取自上面 netRequest 的回包
    let cover = content
        .get("liveList")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|item| item.get("coverPath"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        // 接口下发的是相对路径，补上静态 CDN 前缀才是可直接请求的绝对地址
        .to_string();
    let cover = if cover.starts_with("http") || cover.is_empty() {
        cover
    } else {
        format!("https://source.48.cn{cover}")
    };
    if !cover.is_empty() {
        let data_url = ctx.dispatch("imageDataUrl", &[json!(cover)]).await?;
        let data_url = data_url.as_str().unwrap_or_default();
        if !data_url.starts_with("data:image/") {
            return Err(format!(
                "imageDataUrl 返回格式不对: {}",
                &data_url[..data_url.len().min(40)]
            ));
        }
        println!("imageDataUrl: {} 字符（封面）", data_url.len());
    }

    // 4) 网络诊断：7 个目标并发探测，单行失败不影响其余
    let items = ctx.dispatch("netDiagnose", &[]).await?;
    let rows = items.as_array().cloned().unwrap_or_default();
    println!("netDiagnose: {} 行", rows.len());
    for row in &rows {
        let label = row.get("label").and_then(Value::as_str).unwrap_or("");
        let status = row.get("status").and_then(Value::as_u64);
        let ms = row.get("ms").and_then(Value::as_u64).unwrap_or(0);
        let detail = row.get("detail").and_then(Value::as_str).unwrap_or("");
        let error = row.get("error").and_then(Value::as_str).unwrap_or("");
        let summary = status
            .map(|code| format!("HTTP {code}"))
            .unwrap_or_else(|| error.to_string());
        println!(
            "  {label} · {summary} · {ms}ms {}",
            if detail.is_empty() { "" } else { detail }
        );
    }
    if rows.is_empty() {
        return Err("netDiagnose 没有返回任何探测行".to_string());
    }

    let app = ctx.dispatch("getAppInfo", &[]).await?;
    println!(
        "AppInfo: v{} {} {}",
        app.get("version").and_then(Value::as_str).unwrap_or(""),
        app.get("platform").and_then(Value::as_str).unwrap_or(""),
        app.get("arch").and_then(Value::as_str).unwrap_or("")
    );

    // 5) P2：防休眠句柄（默认不跑，避免探针影响系统电源状态）
    if std::env::var("D48_PROBE_SLEEP").ok().as_deref() == Some("1") {
        let id = ctx.dispatch("preventSleep", &[]).await?;
        println!("sleep: preventSleep 返回 {}", id.as_u64().unwrap_or(0));
        ctx.dispatch("allowSleep", &[id]).await?;
        println!("sleep: allowSleep 已释放");
    }

    // 6) P2：本地 HTTP-FLV 服务与 FFmpeg 环境
    let port = ctx.start_media()?;
    println!("media: 本地流媒体服务 {port}");
    let status = ctx.dispatch("getFfmpegStatus", &[]).await?;
    println!(
        "media: FFmpeg ok={} problem={} dir={}",
        status.get("ok").and_then(Value::as_bool).unwrap_or(false),
        status
            .get("problem")
            .and_then(Value::as_str)
            .unwrap_or("null"),
        status
            .get("directory")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    // 有真实 RTMP 地址时可做一次端到端拉流（默认不跑，避免依赖直播是否在播）。
    if let Ok(rtmp) = std::env::var("D48_PROBE_RTMP") {
        let session = ctx
            .dispatch("createLiveStream", &[json!(rtmp), json!("probe-live")])
            .await?;
        let url = session
            .get("url")
            .and_then(Value::as_str)
            .ok_or("createLiveStream 未返回 url")?;
        println!("media: 本地播放地址 {url}");
        let client = reqwest::Client::new();
        // 本地转流只放行 WebView 来源；探针显式带开发服务器 Origin，等同渲染层请求。
        let mut response = client
            .get(url)
            .header(reqwest::header::ORIGIN, "http://localhost:1420")
            .send()
            .await
            .map_err(|err| err.to_string())?;
        println!("media: HTTP {}", response.status());
        match tokio::time::timeout(std::time::Duration::from_secs(12), response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                println!(
                    "media: 首块 {} 字节，前 3 字节 {:?}",
                    chunk.len(),
                    &chunk[..chunk.len().min(3)]
                );
            }
            Ok(Ok(None)) => println!("media: 流已结束（未收到数据）"),
            Ok(Err(err)) => return Err(format!("读取本地 FLV 失败: {err}")),
            Err(_) => return Err("12 秒内未收到首块流数据".to_string()),
        }
        drop(response);
        ctx.dispatch("stopLiveStream", &[json!("probe-live")])
            .await?;
    }

    // 6) P3：账号域。默认只读登录态；带 `D48_PROBE_POCKET_TOKEN` 时做一次真实登录校验，
    // 令牌只从进程环境进入内存，不打印、不落盘（探针用 MemoryVault）。
    let auth = ctx.dispatch("pocketGetAuthState", &[]).await?;
    println!(
        "pocket: loggedIn={} profile={} mobile={} plainFallback={}",
        auth.get("loggedIn")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        auth.get("clientProfile")
            .and_then(Value::as_str)
            .unwrap_or(""),
        auth.get("mobileMasked")
            .and_then(Value::as_str)
            .unwrap_or(""),
        auth.get("plainFallback")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    if let Ok(token) = std::env::var("D48_PROBE_POCKET_TOKEN")
        && !token.trim().is_empty()
    {
        let state = ctx
            .dispatch("pocketLoginByToken", &[json!(token), json!("")])
            .await?;
        println!(
            "pocket: token 登录成功，userId={} nickname={}",
            state.get("userId").and_then(Value::as_str).unwrap_or(""),
            state.get("nickname").and_then(Value::as_str).unwrap_or("")
        );
        let verify = ctx.dispatch("pocketVerifyLogin", &[]).await?;
        println!(
            "pocket: verify valid={} imOk={} moneyOk={} level={} message={}",
            verify
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            verify.get("imOk").and_then(Value::as_bool).unwrap_or(false),
            verify
                .get("moneyOk")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            verify.get("level").and_then(Value::as_str).unwrap_or(""),
            verify.get("message").and_then(Value::as_str).unwrap_or("")
        );
    }

    // 6b) 系统集成：B 站房间信息（真实网络）+ 升格归属门 + 路径白名单。
    match ctx.dispatch("bilibiliRoomInfos", &[]).await {
        Ok(infos) => {
            let list = infos.as_array().cloned().unwrap_or_default();
            let titled = list
                .iter()
                .filter(|info| {
                    !info
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .is_empty()
                })
                .count();
            println!(
                "bilibili: {} 个房间，{} 个拿到标题；{}",
                list.len(),
                titled,
                list.iter()
                    .map(|info| format!(
                        "{}={:?}({})",
                        info.get("roomId").and_then(Value::as_u64).unwrap_or(0),
                        info.get("title").and_then(Value::as_str).unwrap_or(""),
                        info.get("liveStatus").and_then(Value::as_i64).unwrap_or(0)
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        Err(err) => println!("bilibili: 失败 {err}"),
    }
    let promotions = ctx.dispatch("getMemberTeamPromotions", &[]).await?;
    println!(
        "promotion: 当前账号 {} 条升格",
        promotions.as_array().map(Vec::len).unwrap_or(0)
    );
    let denied = ctx
        .dispatch("openPath", &[json!("/etc/hosts")])
        .await
        .unwrap_err();
    println!("openPath: 白名单外路径被拒 → {denied}");
    let bad_image = ctx
        .dispatch(
            "copyImageToClipboard",
            &[json!("data:image/png;base64,QUJD")],
        )
        .await
        .unwrap_err();
    println!("copyImageToClipboard: 非法 PNG 被拒 → {bad_image}");

    // 6c) 设置页依赖：路径拼接 / 版本 / 反馈 / 更新检查
    let joined = ctx
        .dispatch("pathJoin", &[json!("/tmp/d48"), json!("logs")])
        .await?;
    println!("pathJoin: {joined}");
    println!(
        "getAppVersion: {}",
        ctx.dispatch("getAppVersion", &[]).await?
    );
    let meta = ctx.dispatch("feedbackMeta", &[]).await?;
    println!(
        "feedback: endpointReady={} version={} platform={} runtime={} locale={} profile={}",
        meta.get("endpointReady")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        meta["meta"]["appVersion"].as_str().unwrap_or(""),
        meta["meta"]["platform"].as_str().unwrap_or(""),
        meta["meta"]["electron"].as_str().unwrap_or(""),
        meta["meta"]["locale"].as_str().unwrap_or(""),
        meta["meta"]["clientProfile"].as_str().unwrap_or("")
    );
    let bad_feedback = ctx
        .dispatch("submitFeedback", &[json!({ "content": "   " })])
        .await
        .unwrap_err();
    println!("submitFeedback: 空内容被拒 → {bad_feedback}");
    // 自动更新（updaterCheck / updaterGetState）已整块移除，探针不再有对应步骤。

    // 7) 数据库只读通道（显式指定路径；默认不碰用户真实库）
    if let Ok(path) = std::env::var("D48_PROBE_DB") {
        report_database(std::path::Path::new(&path))?;
    }

    // 8) 图片剪贴板（默认关闭：会改写用户剪贴板；跑完由调用方自行确认）
    if std::env::var("D48_PROBE_CLIPBOARD").as_deref() == Ok("1") {
        let png = one_pixel_png();
        use base64::Engine as _;
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        ctx.dispatch("copyImageToClipboard", &[json!(data_url)])
            .await?;
        println!("copyImageToClipboard: 1×1 红点 PNG 已写入系统剪贴板");
        let bad = ctx
            .dispatch(
                "copyImageToClipboard",
                &[json!("data:image/png;base64,QUJD")],
            )
            .await
            .unwrap_err();
        println!("copyImageToClipboard: 非法 PNG 被拒 → {bad}");
    }

    // 9) 旧版数据迁移（默认关闭：探针不应把本机真实 database.json 搬进临时目录）
    if std::env::var("D48_PROBE_MIGRATE").is_ok() {
        let target = std::env::temp_dir().join("desktop48-lite-probe-migrate");
        let _ = std::fs::remove_dir_all(&target);
        let source =
            desktop48_lite_lib::migrate::legacy_data_dir().ok_or("无法定位旧版数据目录")?;
        let report = desktop48_lite_lib::migrate::migrate_from(&target, &source);
        println!(
            "migrate: {} 个文件 {:?} / 备注 {:?}",
            report.files.len(),
            report.files,
            report.note
        );
        let database = target.join("database.json");
        report_database(&database)?;
        exercise_database_writes(&database)?;
        if let Ok(raw) = std::fs::read_to_string(&database) {
            let value: Value = serde_json::from_str(&raw)
                .map_err(|err| format!("迁移后的 database.json 无法解析: {err}"))?;
            let config = value.get("config").and_then(Value::as_object);
            println!(
                "  迁移库: {} 字节 · config {} 项 · starInfo {} 条",
                raw.len(),
                config.map(|c| c.len()).unwrap_or(0),
                value
                    .get("starInfo")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0)
            );
        }
        let device = target.join("pocket48-device.json");
        if let Ok(raw) = std::fs::read_to_string(&device) {
            println!("  迁移设备 ID: {raw}");
        }
        let _ = std::fs::remove_dir_all(&target);
    }

    println!("OK: 通用桥链路可用");
    Ok(())
}

/// 1×1 红色 PNG：剪贴板通道的端到端样本（结构合法且可解码）。
fn one_pixel_png() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
        .expect("内置 PNG 样本必须可解码")
}
