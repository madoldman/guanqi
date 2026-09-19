//! 会话总线的最小客户端：地址解析、SASL EXTERNAL 认证、消息收发与信号等待。
//! 只用 std 的 `UnixStream`，不依赖任何 D-Bus 库。
//!
//! 线程模型：一个 [`Connection`] 绑定一个线程使用（serial 无并发保护）；
//! portal 模块中它归工作线程所有。

use super::PortalError;
use super::marshal::marshal_call;
use super::wire::{self, ERROR, FLAG_NO_REPLY_EXPECTED, METHOD_RETURN, Message, SIGNAL, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// 总线自身提供的标准接口（Hello / AddMatch / RemoveMatch）。
const BUS_DEST: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
/// 认证、Hello、AddMatch 等启动期与控制类往返的应答上限。
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// 一条已认证的会话总线连接。
pub struct Connection {
    stream: UnixStream,
    serial: u32,
    unique_name: String,
    /// 方法调用等待期间收到的信号（应答与信号可任意交织），留给
    /// [`Connection::wait_signal`] 先行消化——例如先于 Close 应答到达的
    /// Response(1)。
    inbox: Vec<Message>,
}

impl Connection {
    /// 连接会话总线：解析地址 → SASL EXTERNAL 认证 → `Hello` 领取唯一名。
    pub fn connect() -> Result<Self, PortalError> {
        let path = session_socket()?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| PortalError::NoDBus(format!("连接 {path} 失败：{e}")))?;
        stream
            .set_write_timeout(Some(SETUP_TIMEOUT))
            .map_err(PortalError::Io)?;
        let mut conn = Self {
            stream,
            serial: 0,
            unique_name: String::new(),
            inbox: Vec::new(),
        };
        conn.authenticate()?;
        conn.hello()?;
        Ok(conn)
    }

    /// 本连接的唯一名（`:1.N`），预测 portal request 路径时需要。
    pub fn unique_name(&self) -> &str {
        &self.unique_name
    }

    /// 发起方法调用并等待应答；ERROR 应答转为 [`PortalError::CallFailed`]。
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: &[Value],
        wait: Duration,
    ) -> Result<Vec<Value>, PortalError> {
        let serial = self.next_serial();
        let bytes = marshal_call(serial, 0, destination, path, interface, member, body)?;
        self.stream.write_all(&bytes).map_err(PortalError::Io)?;
        loop {
            let msg = self.read_message(wait)?;
            match msg.kind {
                METHOD_RETURN if msg.reply_serial == Some(serial) => return Ok(msg.body),
                ERROR if msg.reply_serial == Some(serial) => {
                    let message = match msg.body.first() {
                        Some(Value::Str(message)) => message.clone(),
                        _ => "（对端未附错误消息）".to_owned(),
                    };
                    return Err(PortalError::CallFailed {
                        name: msg.error_name.unwrap_or_else(|| "未知错误".to_owned()),
                        message,
                    });
                }
                _ if msg.kind == SIGNAL => self.inbox.push(msg), // 与应答交织的信号，留给 wait_signal
                _ => {}                                          // 无关广播 / 迟到的应答：丢弃
            }
        }
    }

    /// 发送方法调用但**不等应答**（`NO_REPLY_EXPECTED`）。
    /// 用于 Request.Close 这类"触发即可"的调用，也避免随后到达的信号被
    /// 等应答循环当作无关消息吞掉。
    pub fn call_no_reply(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: &[Value],
    ) -> Result<(), PortalError> {
        let serial = self.next_serial();
        let bytes = marshal_call(
            serial,
            FLAG_NO_REPLY_EXPECTED,
            destination,
            path,
            interface,
            member,
            body,
        )?;
        self.stream.write_all(&bytes).map_err(PortalError::Io)
    }

    /// 等待一条信号（接口 + 成员 + 任意匹配路径），返回其消息体。
    /// 先消化调用期间积压在收件箱里的信号，再阻塞读取；无关消息被忽略。
    pub fn wait_signal(
        &mut self,
        interface: &str,
        member: &str,
        paths: &[&str],
        wait: Duration,
    ) -> Result<Vec<Value>, PortalError> {
        if let Some(pos) = self
            .inbox
            .iter()
            .position(|msg| signal_matches(msg, interface, member, paths))
        {
            let msg = self.inbox.remove(pos);
            return Ok(msg.body);
        }
        loop {
            let msg = self.read_message(wait)?;
            if signal_matches(&msg, interface, member, paths) {
                return Ok(msg.body);
            }
        }
    }

    /// 订阅匹配规则（规范要求：等 Response 信号必须**先于**调用本身）。
    pub fn add_match(&mut self, rule: &str) -> Result<(), PortalError> {
        self.call(
            BUS_DEST,
            BUS_PATH,
            BUS_DEST,
            "AddMatch",
            &[Value::Str(rule.to_owned())],
            SETUP_TIMEOUT,
        )
        .map(|_| ())
    }

    /// 退订匹配规则（尽力而为；总线在连接断开时也会自行清理）。
    pub fn remove_match(&mut self, rule: &str) {
        let _ = self.call(
            BUS_DEST,
            BUS_PATH,
            BUS_DEST,
            "RemoveMatch",
            &[Value::Str(rule.to_owned())],
            SETUP_TIMEOUT,
        );
    }

    // ---- 内部 ----

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    /// SASL EXTERNAL：NUL → `AUTH EXTERNAL <uid 的 ASCII 十六进制>` → `OK` → `BEGIN`。
    fn authenticate(&mut self) -> Result<(), PortalError> {
        let uid = current_uid()?;
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.stream.write_all(b"\0").map_err(PortalError::Io)?;
        self.stream
            .write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())
            .map_err(PortalError::Io)?;
        let mut line = String::new();
        self.read_sasl_line(&mut line)?;
        if !line.starts_with("OK ") {
            return Err(PortalError::NoDBus(format!("会话总线拒绝了认证：{line}")));
        }
        self.stream
            .write_all(b"BEGIN\r\n")
            .map_err(PortalError::Io)?;
        Ok(())
    }

    /// 读一行 SASL 应答（ASCII，`\r\n` 结尾）。
    fn read_sasl_line(&mut self, out: &mut String) -> Result<(), PortalError> {
        loop {
            let mut byte = [0u8; 1];
            self.read_full(&mut byte, SETUP_TIMEOUT)?;
            match byte[0] {
                b'\n' => return Ok(()),
                b'\r' => {}
                byte => out.push(byte as char),
            }
        }
    }

    /// `Hello`：领取唯一名（后续预测 request 路径要用）。
    fn hello(&mut self) -> Result<(), PortalError> {
        let reply = self.call(BUS_DEST, BUS_PATH, BUS_DEST, "Hello", &[], SETUP_TIMEOUT)?;
        match reply.into_iter().next() {
            Some(Value::Str(name)) => {
                self.unique_name = name;
                Ok(())
            }
            _ => Err(PortalError::Protocol("Hello 应答不是字符串".into())),
        }
    }

    /// 读取并解析一条完整消息（限时 `wait`）。
    fn read_message(&mut self, wait: Duration) -> Result<Message, PortalError> {
        let mut head = [0u8; 16];
        self.read_full(&mut head, wait)?;
        let big = match head[0] {
            b'l' => false,
            b'B' => true,
            other => {
                return Err(PortalError::Protocol(format!(
                    "未知字节序标记 0x{other:02x}"
                )));
            }
        };
        let body_len = read_u32(&head, 4, big) as usize;
        let fields_len = read_u32(&head, 12, big) as usize;
        if body_len > wire::MAX_MESSAGE_BYTES || fields_len > wire::MAX_MESSAGE_BYTES {
            return Err(PortalError::Protocol("消息超过大小上限".into()));
        }
        let header_len = (16 + fields_len).next_multiple_of(8);
        let mut buf = vec![0u8; header_len + body_len];
        buf[..16].copy_from_slice(&head);
        self.read_full(&mut buf[16..], wait)?;
        wire::parse(&buf)
    }

    /// 限时读满 `buf`；超时报 [`PortalError::Timeout`]，连接中断报 NoDBus。
    fn read_full(&mut self, buf: &mut [u8], wait: Duration) -> Result<(), PortalError> {
        let deadline = Instant::now() + wait;
        let mut filled = 0;
        while filled < buf.len() {
            let now = Instant::now();
            if now >= deadline {
                return Err(PortalError::Timeout(wait));
            }
            self.stream
                .set_read_timeout(Some(deadline - now))
                .map_err(PortalError::Io)?;
            match self.stream.read(&mut buf[filled..]) {
                Ok(0) => return Err(PortalError::NoDBus("会话总线连接已断开".into())),
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // 读超时只是到点，回到循环顶部判定 deadline。
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(PortalError::Io(e)),
            }
        }
        Ok(())
    }
}

fn read_u32(buf: &[u8], at: usize, big: bool) -> u32 {
    let bytes = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
    if big {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

/// 信号是否命中等待条件（接口 + 成员 + 路径）。
fn signal_matches(msg: &Message, interface: &str, member: &str, paths: &[&str]) -> bool {
    msg.kind == SIGNAL
        && msg.interface.as_deref() == Some(interface)
        && msg.member.as_deref() == Some(member)
        && msg
            .path
            .as_deref()
            .is_some_and(|path| paths.contains(&path))
}

/// 解析会话总线地址：优先 `DBUS_SESSION_BUS_ADDRESS`（至少支持 `unix:path=`），
/// 回退 `$XDG_RUNTIME_DIR/bus`（systemd 会话的事实标准位置）。
fn session_socket() -> Result<String, PortalError> {
    if let Ok(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        for entry in address.split(';') {
            let Some(rest) = entry.trim().strip_prefix("unix:") else {
                continue;
            };
            for param in rest.split(',') {
                if let Some(path) = param.strip_prefix("path=") {
                    return Ok(path.to_owned());
                }
            }
            if rest.split(',').any(|param| param.starts_with("abstract=")) {
                return Err(PortalError::NoDBus("不支持 abstract socket 地址".into()));
            }
        }
        return Err(PortalError::NoDBus(format!(
            "无法解析 DBUS_SESSION_BUS_ADDRESS：{address}"
        )));
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let path = format!("{dir}/bus");
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }
    Err(PortalError::NoDBus(
        "未设置 DBUS_SESSION_BUS_ADDRESS / XDG_RUNTIME_DIR，找不到会话总线".into(),
    ))
}

/// 当前 uid（std 没有 getuid；`/proc/self/status` 的 Uid 行首字段是真实 uid）。
fn current_uid() -> Result<u32, PortalError> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| PortalError::NoDBus(format!("无法读取 /proc/self/status：{e}")))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:")
            && let Some(field) = rest.split_whitespace().next()
            && let Ok(uid) = field.parse()
        {
            return Ok(uid);
        }
    }
    Err(PortalError::NoDBus(
        "无法从 /proc/self/status 确定 uid".into(),
    ))
}
