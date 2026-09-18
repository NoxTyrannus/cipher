//! v0.5.5 子进程生命周期看门狗（方案 D）：一套策略代码，三平台语义一致。
//!
//! 机制：主进程持有管道写端；`cipher __watchdog` 辅助进程以读端为 stdin。
//! 主进程无论以何种方式死亡（正常退出 / panic / SIGKILL），内核都会关闭其
//! 文件描述符 → 管道读端收到 EOF（这是三平台唯一由内核保证的死亡信号）→
//! 看门狗按登记清单杀死全部子进程树。语义（用户拍板）：子代理派生的一切
//! 进程不比主进程长寿。
//!
//! 协议（stdin 逐行）：`add <pid>` 登记 / `reap <pid>` 撤销（子进程已正常
//! 退出时撤销，避免 pid 复用误伤）；EOF = 主进程死亡 → 清算。
//!
//! 平台差异被压缩到两个叶子函数（`kill_tree` / `apply_lifecycle_hooks`）：
//! unix 用进程组（spawn 时 setpgid，清算时 kill(-pgid)）；Windows 用
//! `taskkill /T /F` 按 pid 杀进程树。Linux 的 PDEATHSIG 仅作纵深加固，
//! 正确性不依赖它。看门狗自身不挂生命周期钩子（它必须活得比主进程久），
//! 也不 bootstrap（不碰数据库，stdin 之外全部关闭，不持有 DuckDB 文件锁）。

use std::io::{BufRead, Write};
use std::sync::{Mutex, OnceLock};

/// 看门狗主循环（子进程侧）。EOF 即主进程死亡，清算全部在册 pid 后返回。
/// 收到不可解析的行忽略，不致命。
pub fn run<R: BufRead>(mut input: R) -> std::io::Result<()> {
    let mut pids: Vec<u32> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("add ") {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                pids.push(pid);
            }
        } else if let Some(rest) = trimmed.strip_prefix("reap ") {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                if let Some(pos) = pids.iter().position(|&p| p == pid) {
                    pids.swap_remove(pos);
                }
            }
        }
    }
    for &pid in &pids {
        kill_tree(pid);
    }
    Ok(())
}

/// 主进程侧：登记一个子进程 pid（写端写协议行）。看门狗未启动则先拉起；
/// 已死则本次跳过（超时清理路径仍有效），下次调用自愈重启。
pub fn register_child(pid: u32) {
    let mut guard = link_lock();
    if guard.is_none() {
        *guard = start_watchdog().map(|writer| WatchdogLink { writer });
    }
    let Some(link) = guard.as_mut() else {
        return;
    };
    if link.write_all(format!("add {pid}\n").as_bytes()).is_err() {
        tracing::warn!(
            pid,
            "watchdog: 看门狗已退出，本次登记未送达（超时清理仍有效）"
        );
        *guard = None;
    }
}

/// 主进程侧：撤销登记（子进程已退出，防止 pid 复用后被清算误伤）。尽力而为。
pub fn reap_child(pid: u32) {
    let mut guard = link_lock();
    let Some(link) = guard.as_mut() else {
        return;
    };
    if link.write_all(format!("reap {pid}\n").as_bytes()).is_err() {
        *guard = None;
    }
}

/// 主进程侧：拉起看门狗进程（stdin=管道读端，输出静默）。失败返回 None，
/// 主流程不受影响——降级为"仅超时清理，无父退自动杀"。
fn start_watchdog() -> Option<std::io::PipeWriter> {
    let (read_end, write_end) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("watchdog: 创建管道失败，跳过父退自动杀: {e}");
            return None;
        }
    };
    let exe = std::env::current_exe();
    let spawn =
        std::process::Command::new(exe.as_deref().unwrap_or(std::path::Path::new("cipher")))
            .arg("__watchdog")
            .stdin(std::process::Stdio::from(read_end))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    match spawn {
        Ok(child) => {
            // 看门狗自行存活，父进程不持有其 Child 句柄等待；写端留在本进程。
            std::mem::forget(child);
            Some(write_end)
        }
        Err(e) => {
            tracing::warn!("watchdog: 拉起看门狗失败，跳过父退自动杀: {e}");
            None
        }
    }
}

struct WatchdogLink {
    writer: std::io::PipeWriter,
}

impl WatchdogLink {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }
}

static LINK: OnceLock<Mutex<Option<WatchdogLink>>> = OnceLock::new();

fn link_lock() -> std::sync::MutexGuard<'static, Option<WatchdogLink>> {
    LINK.get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 杀整棵子进程树（叶子函数，平台实现）。
#[cfg(unix)]
pub fn kill_tree(pid: u32) {
    // pgid = pid（spawn 钩子 setpgid 保证）；先杀组覆盖孙进程，单杀兜底未成组者。
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    // /T 连孙进程一起、/F 强杀；无句柄需求，看门狗进程也可调用。
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 给子进程挂生命周期钩子（叶子函数，平台实现）。必须在 spawn 前调用。
#[cfg(unix)]
pub fn apply_lifecycle_hooks(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(child_session_setup);
    }
}

#[cfg(windows)]
pub fn apply_lifecycle_hooks(_cmd: &mut std::process::Command) {}

/// unix：子进程自成进程组（组杀的前提）。Linux 额外设 PDEATHSIG——
/// 内核级双保险，即使看门狗缺席也能随父退；非 Linux 无此机制，交给看门狗。
#[cfg(unix)]
fn child_session_setup() -> std::io::Result<()> {
    unsafe {
        if libc::setpgid(0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        if libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成一个挂好钩子、自成进程组的 sleep 子进程（unix 测试通用件）。
    #[cfg(unix)]
    fn spawn_sleeper() -> std::process::Child {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        apply_lifecycle_hooks(&mut cmd);
        cmd.spawn().unwrap()
    }

    #[cfg(unix)]
    fn wait_gone(pid: u32) {
        for _ in 0..100 {
            let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
            if !alive {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("pid {pid} 未在 5s 内死亡");
    }

    #[cfg(unix)]
    #[test]
    fn eof_kills_registered_child() {
        // EOF（主进程死亡）→ 在册 pid 被清算。
        let mut child = spawn_sleeper();
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer
            .write_all(format!("add {}\n", child.id()).as_bytes())
            .unwrap();
        drop(writer); // 模拟主进程死亡：写端全部关闭
        run(std::io::BufReader::new(reader)).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "EOF 后在册子进程应被杀: {status:?}");
        wait_gone(child.id());
    }

    #[cfg(unix)]
    #[test]
    fn reap_before_eof_spares_child() {
        // 正常退出的子进程先 reap → EOF 清算不误伤（pid 复用防线）。
        let mut child = spawn_sleeper();
        let script = format!("add {}\nreap {}\n", child.id(), child.id());
        run(std::io::Cursor::new(script)).unwrap();
        let alive = unsafe { libc::kill(child.id() as libc::pid_t, 0) == 0 };
        assert!(alive, "已 reap 的子进程不应被 EOF 清算");
        kill_tree(child.id());
        child.wait().unwrap();
        wait_gone(child.id());
    }

    #[cfg(unix)]
    #[test]
    fn hooks_make_child_own_group_leader_and_kill_tree_reaps() {
        // 钩子后 pgid == pid；kill_tree 组杀正常收尸（继承自 C3 的组语义验证）。
        // getpgid 是 POSIX API（macOS 无 /proc，不可走 /proc/<pid>/stat 解析）。
        let mut child = spawn_sleeper();
        let pid = child.id();
        let pgrp = unsafe { libc::getpgid(pid as libc::pid_t) };
        assert_eq!(
            pgrp, pid as libc::pid_t,
            "setpgid(0,0) 后子进程应自成进程组 (pgid=pid)"
        );
        kill_tree(pid);
        let status = child.wait().unwrap();
        assert!(!status.success(), "组杀应致非正常退出: {status:?}");
    }

    #[test]
    fn unknown_protocol_lines_are_ignored() {
        // 脏输入不致命、不误杀；EOF 后无在册 pid 则空清算。
        run(std::io::Cursor::new("garbage\nadd notanumber\n\nreap 1\n")).unwrap();
    }
}
