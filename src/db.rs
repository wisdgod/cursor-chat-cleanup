//! 定位与打开 Cursor 的全局存储库。
//!
//! 分析模式用 `immutable=1` 只读打开:不加锁、不碰 `-shm`,可以在 Cursor
//! 运行时使用,代价是看到的是 checkpoint 之前的快照,且活库上会间歇性抛
//! `database disk image is malformed`(这不是真损坏,是 immutable 承诺被
//! Cursor 的 checkpoint 打破了)。对策是捕获后整个连接重试。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags};

/// 定位 `state.vscdb`。
///
/// - Windows: `%APPDATA%\Cursor\User\globalStorage\state.vscdb`
/// - macOS:   `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
/// - Linux:   `~/.config/Cursor/User/globalStorage/state.vscdb`
///
/// 有意不探测 WSL 下 Windows 侧的库: 跨 drvfs 操作 SQLite 的锁语义
/// 不可靠且极慢,该场景应在 Windows 侧运行本工具。坚持要跨,
/// 用 `--db` 显式指定并自担风险。
pub fn locate_db() -> Result<PathBuf> {
    if let Some(p) = dirs::config_dir()
        .map(|d| d.join("Cursor/User/globalStorage/state.vscdb"))
        .filter(|p| p.exists())
    {
        return Ok(p);
    }
    bail!("未找到 Cursor 数据库,请用 --db 指定 state.vscdb 路径");
}

/// 在一个只读分析连接上执行 `f`,遇到 immutable 快照失效导致的
/// `SQLITE_CORRUPT` 时重建连接重试(最多 3 次)。
pub fn with_analysis<T>(path: &Path, mut f: impl FnMut(&Connection) -> Result<T>) -> Result<T> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let conn = open_analysis(path)?;
        match f(&conn) {
            Err(e) if attempt < MAX_ATTEMPTS && is_snapshot_torn(&e) => continue,
            r => return r,
        }
    }
}

fn open_analysis(path: &Path) -> Result<Connection> {
    ensure!(path.exists(), "数据库不存在: {}", path.display());
    let uri = format!("file:{}?immutable=1", uri_escape(path));
    Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("打开数据库失败: {}", path.display()))
}

/// SQLite URI 文件名中 `%`、`?`、`#` 需要百分号转义。
fn uri_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            _ => out.push(c),
        }
    }
    out
}

/// `cursorDiskKV` 总行数(进度显示的总量;走索引,便宜)。
pub fn kv_row_count(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM cursorDiskKV", [], |r| r.get::<_, i64>(0))? as u64)
}

/// 连接级取消: 挂接 SQLite progress handler,任何正在执行的 SQL
/// (含 COUNT、大范围索引扫描)都会在 ~4096 条 VM 指令内被中断,
/// 不必等行迭代循环里的检查点。**只用于只读操作**——写事务被中断
/// 会整体回滚,那是浪费而非取消。
///
/// Drop 时卸载 handler,同一连接的后续操作(如维护模式的写)不受影响。
pub struct CancelGuard<'c> {
    conn: &'c Connection,
}

impl<'c> CancelGuard<'c> {
    pub fn install(conn: &'c Connection, p: std::sync::Arc<crate::progress::Progress>) -> Self {
        // 挂接失败(理论上只有 OOM)不致命: 退化为只靠循环检查点取消
        let _ = conn.progress_handler(4096, Some(move || p.is_cancelled()));
        Self { conn }
    }
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

fn is_snapshot_torn(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|re| re.sqlite_error_code())
            .is_some_and(|code| code == rusqlite::ErrorCode::DatabaseCorrupt)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_mem_db(rows: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        for i in 0..rows {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, 'v')",
                [format!("bubbleId:orphan-{:08}:b", i)],
            )
            .unwrap();
        }
        conn
    }

    /// 连接级取消必须能打断**正在执行**的逐行 SQL(循环检查点之外的兜底)。
    /// 注: 裸 `COUNT(*)` 走 B-tree 计数优化(单条 OP_Count),指令太少
    /// 触发不了 handler——但它因此本就是瞬时操作,不构成取消盲区。
    #[test]
    fn cancel_guard_interrupts_running_sql() {
        let conn = populated_mem_db(20_000);
        let p = std::sync::Arc::new(crate::progress::Progress::new());
        p.cancel();
        let _guard = CancelGuard::install(&conn, p.clone());
        let r: rusqlite::Result<i64> = conn.query_row(
            "SELECT SUM(LENGTH(key) + LENGTH(value)) FROM cursorDiskKV",
            [],
            |row| row.get(0),
        );
        let code = r.unwrap_err().sqlite_error_code();
        assert_eq!(code, Some(rusqlite::ErrorCode::OperationInterrupted));

        // guard 卸载后同一连接恢复正常(维护会话的后续操作依赖这一点)
        drop(_guard);
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM cursorDiskKV", [], |row| row.get(0)).unwrap();
        assert_eq!(n, 20_000);
    }

    /// 行迭代的循环检查点: 取消后在下一个检查点(1024 行)退出,不跑完全表。
    #[test]
    fn cancelled_scan_exits_early() {
        let conn = populated_mem_db(5_000);
        let live = crate::types::LiveSet::default();
        let p = crate::progress::Progress::new();
        p.cancel();
        let err = crate::scan::scan_keys(&conn, &live, false, false, &p).unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<crate::progress::Cancelled>().is_some()),
            "应以 Cancelled 退出,实际: {err:#}"
        );
    }
}
