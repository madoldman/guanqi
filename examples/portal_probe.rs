//! Portal 诊断探针（联调验证用，不进入发布物）。
//!
//! 用法：
//! - `cargo run --example portal_probe`            —— 无弹窗体检：连接 + 认证 + Hello +
//!   version + 订阅往返 + 假路径等待超时；
//! - `cargo run --example portal_probe -- --dialog` —— 真实弹窗（公开 API 全链路，
//!   30s 无人操作即退出）。

use guanqi::portal::dbus::Connection;
use guanqi::portal::filechooser;
use guanqi::portal::{FileDialog, PortalError, PortalEvent, Waker};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    if std::env::args().any(|arg| arg == "--dialog") {
        dialog_flow();
    } else if std::env::args().any(|arg| arg == "--predict") {
        predict_flow();
    } else {
        check_flow();
    }
}

/// 无弹窗检查：连接 + 认证 + Hello + version + AddMatch/RemoveMatch + 超时机制。
fn check_flow() {
    let started = Instant::now();
    let version = match FileDialog::available() {
        Ok(version) => version,
        Err(error) => {
            eprintln!("探测失败（结构化错误）：{error:?}");
            std::process::exit(3);
        }
    };
    println!(
        "[1] 连接 + SASL 认证 + Hello + Properties.Get OK：FileChooser.version = {version}（耗时 {:?}）",
        started.elapsed()
    );

    let mut conn = match Connection::connect() {
        Ok(conn) => conn,
        Err(error) => {
            eprintln!("二次连接失败：{error:?}");
            std::process::exit(3);
        }
    };
    println!("[2] Hello 领取唯一名：{}", conn.unique_name());

    // 对假 request 路径订阅，验证匹配规则被总线接受。
    let rule = filechooser::response_match_rule(
        "/org/freedesktop/portal/desktop/request/1_0/guanqi_probe",
    );
    if let Err(error) = conn.add_match(&rule) {
        eprintln!("AddMatch 失败：{error:?}");
        std::process::exit(3);
    }
    println!("[3] AddMatch（假路径）OK：{rule}");

    // 等一个永远不会来的信号 → 验证超时机制（不悬挂、返回结构化错误）。
    let started = Instant::now();
    match conn.wait_signal(
        "org.freedesktop.portal.Request",
        "Response",
        &["/org/freedesktop/portal/desktop/request/1_0/guanqi_probe"],
        Duration::from_secs(3),
    ) {
        Err(PortalError::Timeout(wait)) => {
            println!(
                "[4] 假路径等待按预期超时（wait={wait:?}，实际耗时 {:?}），进程未悬挂",
                started.elapsed()
            );
        }
        Err(error) => {
            eprintln!("假路径等待出现意外错误：{error:?}");
            std::process::exit(3);
        }
        Ok(_) => {
            eprintln!("假路径不该收到信号");
            std::process::exit(3);
        }
    }
    conn.remove_match(&rule);
    println!("全部无弹窗检查通过");
}

/// 真实弹窗：走公开 API（open_file + waker + try_recv）。
fn dialog_flow() {
    let waker: Waker =
        Arc::new(|| println!("[waker] 结果已入队（此处应触发 ctx.request_repaint）"));
    let mut dialog = match FileDialog::open_file("观棋探针：选择棋谱 (SGF)", Some(waker)) {
        Ok(dialog) => dialog,
        Err(error) => {
            eprintln!("open_file 同步失败：{error:?}");
            std::process::exit(3);
        }
    };
    println!("对话框已发起（工作线程等待中），最长 30s …");
    let deadline = Instant::now() + Duration::from_secs(30);
    let event = loop {
        if let Some(event) = dialog.try_recv() {
            break event;
        }
        if Instant::now() >= deadline {
            println!("30s 无人操作，探针退出（发送端断开后 portal 会取消该请求）");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    match event {
        PortalEvent::Picked(path) => println!("[结果] 用户选中：{}", path.display()),
        PortalEvent::Cancelled => println!("[结果] 用户取消"),
        PortalEvent::Failed(error) => println!("[结果] 失败：{error}"),
    }
}

/// 路径预测实证：弹窗一闪即关（立即 Close）。验证 handle 与预测路径一致、
/// 且 Response 信号经「先于调用的预测路径匹配规则」送达。
fn predict_flow() {
    let mut conn = Connection::connect().expect("连接失败");
    let version = filechooser::probe(&mut conn).expect("probe 失败");
    println!("[predict] FileChooser.version = {version}");

    let token = format!("guanqi_proof_{}", std::process::id());
    let predicted = filechooser::request_path(conn.unique_name(), &token);
    let rule = filechooser::response_match_rule(&predicted);
    conn.add_match(&rule).expect("AddMatch 失败");

    let started = Instant::now();
    let handle =
        filechooser::open_file_call(&mut conn, "观棋探针：路径预测实证（自动关闭）", &token)
            .expect("OpenFile 失败");
    println!("[predict] OpenFile OK（耗时 {:?}）", started.elapsed());
    println!("[predict] handle   = {handle}");
    println!("[predict] 预测路径 = {predicted}");
    println!(
        "[predict] {}",
        if handle == predicted {
            "预测路径与实际 handle 一致"
        } else {
            "预测路径与实际 handle 不一致"
        }
    );

    filechooser::close_request(&mut conn, &handle);
    // 注：本机 xdg-desktop-portal-kde 在 Close 路径上会真正关闭对话框窗口，
    // 但不发 Response 信号（后端版本怪癖），故这里对信号到达不作断言。
    match conn.wait_signal(
        "org.freedesktop.portal.Request",
        "Response",
        &[handle.as_str(), predicted.as_str()],
        Duration::from_secs(2),
    ) {
        Ok(body) => println!("[predict] 收到 Response：{body:?}"),
        Err(_) => println!("[predict] Close 路径无 Response（本机后端怪癖，已实测确认）"),
    }
    conn.remove_match(&rule);
}
