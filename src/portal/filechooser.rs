//! `org.freedesktop.portal.FileChooser.OpenFile` 的语义封装。
//!
//! 协议时序（规范要求**先订阅再调用**，否则可能漏掉应答信号）：
//! 1. 用 `handle_token` 预测 request 对象路径，`AddMatch` 订阅其 Response 信号；
//! 2. 调 `OpenFile`（应答立即返回 handle，与用户操作无关）；
//! 3. 等 `org.freedesktop.portal.Request.Response`（`u response, a{sv} results`）。
//!
//! `parent_window` 传空串 = 不做窗口模态绑定（简化）。将来可从 egui viewport
//! 导出 Wayland toplevel 句柄（xdg-foreign 协议），以 `"wayland:<句柄>"` 传入。

use super::PortalError;
use super::dbus::Connection;
use super::wire::Value;
use std::path::PathBuf;
use std::time::Duration;

/// portal 桌面服务（各后端以其名义应答）。
const SERVICE: &str = "org.freedesktop.portal.Desktop";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const FILECHOOSER: &str = "org.freedesktop.portal.FileChooser";
const REQUEST: &str = "org.freedesktop.portal.Request";
/// portal 调用本身（应答与用户操作无关）的上限。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 用户一直不选择时的等待上限；超时后主动 Close，避免对话框滞留屏幕。
pub const DIALOG_TIMEOUT: Duration = Duration::from_secs(300);

/// 探测 FileChooser 接口是否可用（**不弹任何窗口**），返回其版本号。
pub fn probe(conn: &mut Connection) -> Result<u32, PortalError> {
    let reply = match conn.call(
        SERVICE,
        OBJECT_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        &[Value::Str(FILECHOOSER.into()), Value::Str("version".into())],
        CALL_TIMEOUT,
    ) {
        Ok(reply) => reply,
        Err(PortalError::CallFailed { name, message })
            if name.ends_with("UnknownInterface")
                || name.ends_with("UnknownObject")
                || name.ends_with("UnknownMethod")
                || name.ends_with("ServiceUnknown") =>
        {
            // 接口 / 对象 / 服务不存在 → portal 不可用，与一般调用失败区分。
            return Err(PortalError::NoPortal(format!(
                "portal 未提供 {FILECHOOSER}：{message}"
            )));
        }
        Err(error) => return Err(error),
    };
    let version = match reply.into_iter().next() {
        Some(Value::Variant(inner)) => match *inner {
            Value::U32(version) => version,
            _ => return Err(PortalError::Protocol("FileChooser.version 不是 u32".into())),
        },
        _ => return Err(PortalError::Protocol("Properties.Get 应答不是变体".into())),
    };
    // 顺带验证订阅往返：对假 request 路径 AddMatch / RemoveMatch 均合法。
    let rule = response_match_rule(&request_path("1.0", "guanqi_probe"));
    conn.add_match(&rule)?;
    conn.remove_match(&rule);
    Ok(version)
}

/// 打开「打开文件」对话框并**阻塞**到用户选择 / 取消 / 超时（须在工作线程调用）。
/// `Ok(Some(path))` = 用户选中；`Ok(None)` = 用户取消。
pub fn open_file_blocking(
    conn: &mut Connection,
    title: &str,
) -> Result<Option<PathBuf>, PortalError> {
    let token = handle_token();
    let predicted = request_path(conn.unique_name(), &token);
    let rule = response_match_rule(&predicted);
    conn.add_match(&rule)?;
    let result = run_request(conn, title, &token, &predicted);
    conn.remove_match(&rule);
    result
}

fn run_request(
    conn: &mut Connection,
    title: &str,
    token: &str,
    predicted: &str,
) -> Result<Option<PathBuf>, PortalError> {
    let handle = open_file_call(conn, title, token)?;
    // 预测路径与实际 handle 不一致时补订一条（某些实现可能不透传 handle_token）。
    let mut extra_rule = None;
    if handle != predicted {
        let rule = response_match_rule(&handle);
        conn.add_match(&rule)?;
        extra_rule = Some(rule);
    }
    let wait = conn.wait_signal(
        REQUEST,
        "Response",
        &[predicted, handle.as_str()],
        DIALOG_TIMEOUT,
    );
    let body = match wait {
        Ok(body) => body,
        Err(PortalError::Timeout(_)) => {
            // 用户一直未操作：先关掉 portal 窗口再上报超时。
            close_request(conn, &handle);
            return Err(PortalError::Timeout(DIALOG_TIMEOUT));
        }
        Err(error) => return Err(error),
    };
    if let Some(rule) = extra_rule {
        conn.remove_match(&rule);
    }
    decode_response(&body)
}

/// 调 `OpenFile`（签名 `s sa{sv} -> o`），返回 request 对象路径（handle）。
/// 调用方必须**先**对 `request_path(unique_name, token)` 做 AddMatch。
pub fn open_file_call(
    conn: &mut Connection,
    title: &str,
    token: &str,
) -> Result<String, PortalError> {
    let options = Value::Array {
        elem: "{sv}".into(),
        items: vec![
            entry("handle_token", Value::Str(token.to_owned())),
            entry("multiple", Value::Bool(false)),
            entry(
                "filters",
                Value::Array {
                    elem: "(sa(us))".into(),
                    items: vec![Value::Struct(vec![
                        Value::Str("棋谱 (SGF)".into()),
                        Value::Array {
                            elem: "(us)".into(),
                            items: vec![
                                // (us)：u = 0 表示 Glob 模式。
                                Value::Struct(vec![Value::U32(0), Value::Str("*.sgf".into())]),
                                Value::Struct(vec![Value::U32(0), Value::Str("*.SGF".into())]),
                            ],
                        },
                    ])],
                },
            ),
        ],
    };
    let reply = conn.call(
        SERVICE,
        OBJECT_PATH,
        FILECHOOSER,
        "OpenFile",
        &[
            Value::Str(String::new()),
            Value::Str(title.to_owned()),
            options,
        ],
        CALL_TIMEOUT,
    )?;
    match reply.into_iter().next() {
        Some(Value::Path(handle)) => Ok(handle),
        _ => Err(PortalError::Protocol("OpenFile 应答不是对象路径".into())),
    }
}

/// 关闭 portal 请求（尽力而为，不等应答）：等待超时兜底与探针联调用。
/// 不等应答是为了让随后的 Response 信号经匹配规则正常送达，不被吞掉。
pub fn close_request(conn: &mut Connection, handle: &str) {
    let _ = conn.call_no_reply(SERVICE, handle, REQUEST, "Close", &[]);
}

/// 预测 request 对象路径：`/org/freedesktop/portal/desktop/request/<发送方>/<token>`，
/// 其中发送方唯一名去掉前导冒号、点换成下划线（`:1.42` → `1_42`）。
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

/// Response 信号的匹配规则（限定到具体 request 路径）。
pub fn response_match_rule(path: &str) -> String {
    format!("type='signal',interface='{REQUEST}',member='Response',path='{path}'")
}

/// 解析 Response 信号体 `u response, a{sv} results`：
/// response 0 = 成功（results 带 uris），1 = 用户取消，其余为 portal 定义失败。
fn decode_response(body: &[Value]) -> Result<Option<PathBuf>, PortalError> {
    let code = match body.first() {
        Some(Value::U32(code)) => *code,
        _ => return Err(PortalError::Protocol("Response 缺少 response 码".into())),
    };
    match code {
        0 => {}
        1 => return Ok(None),
        other => {
            return Err(PortalError::CallFailed {
                name: REQUEST.into(),
                message: format!("portal 对话框返回失败码 {other}"),
            });
        }
    }
    let results = match body.get(1) {
        Some(Value::Array { items, .. }) => items,
        _ => return Err(PortalError::Protocol("Response 缺少 results 字典".into())),
    };
    let uris = results
        .iter()
        .find_map(|item| dict_array(item, "uris"))
        .ok_or_else(|| PortalError::Protocol("成功应答缺少 uris".into()))?;
    match uris.first() {
        Some(Value::Str(uri)) => uri_to_path(uri.as_str())
            .map(Some)
            .ok_or_else(|| PortalError::Protocol(format!("无法解析文件 URI：{uri}"))),
        _ => Err(PortalError::Protocol("uris 为空或元素不是字符串".into())),
    }
}

/// 取 `a{sv}` 条目中 key 对应的变体数组值。
fn dict_array<'a>(item: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    let Value::Struct(pair) = item else {
        return None;
    };
    if pair.len() != 2 {
        return None;
    }
    let Value::Str(name) = &pair[0] else {
        return None;
    };
    if name != key {
        return None;
    }
    let Value::Variant(inner) = &pair[1] else {
        return None;
    };
    let Value::Array { items, .. } = inner.as_ref() else {
        return None;
    };
    Some(items)
}

/// portal 返回 `file:///path` 形式的 URI：转成本地路径并做百分号解码
/// （路径含空格 / 中文时会被编码）。仅支持本地文件 URI。
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if let Some(stripped) = rest.strip_prefix('/') {
        stripped // 空主机名（file:///path）
    } else {
        rest.split_once('/')?.1 // 带主机名（file://host/path）
    };
    let decoded = percent_decode(path)?;
    Some(PathBuf::from(format!("/{decoded}")))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Some(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16).ok()
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 组装 `a{sv}` 的一个条目（字典条目在线上就是两元素结构）。
fn entry(key: &str, value: Value) -> Value {
    Value::Struct(vec![
        Value::Str(key.to_owned()),
        Value::Variant(Box::new(value)),
    ])
}

/// `handle_token` 只允许 `[A-Za-z0-9_]`；pid + 毫秒足以避免同进程内冲突。
fn handle_token() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    format!("guanqi_{}_{millis}", std::process::id())
}
