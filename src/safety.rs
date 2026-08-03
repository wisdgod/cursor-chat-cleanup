//! 写操作的安全门与周边文件检查。
//!
//! 背景: Cursor 打开主库失败时会把主库改名成
//! `.corrupted.<ts>` 并把 `.backup` 顶上去,静默回滚且不弹窗。所以写操作必须:
//! 1) 确认没有活跃连接(`BEGIN EXCLUSIVE` 探测 + `-wal` 新鲜度);
//! 2) 结束时 checkpoint 干净,不留 hot journal;
//! 3) 提醒用户处理陈旧的 `.backup` 与 `.corrupted.*` 残留。

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{Ctx as _, Error, Result};

/// `-wal` 文件最近一次修改距今的时长。不存在则返回 None。
pub fn wal_age(db_path: &Path) -> Option<Duration> {
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    let mtime = std::fs::metadata(wal).ok()?.modified().ok()?;
    mtime.elapsed().ok()
}

/// 打开写连接,先过安全门:
/// - `-wal` 在 15 秒内被写过 → 视为 Cursor 正在运行,拒绝;
/// - `BEGIN EXCLUSIVE`(busy_timeout=0)拿不到排他锁 → 有活跃连接,拒绝。
pub fn open_write_gated(db_path: &Path) -> Result<Connection> {
    if let Some(age) = wal_age(db_path)
        && age < Duration::from_secs(15)
    {
        return Err(Error::CursorRunning { age_secs: age.as_secs() });
    }

    let conn = Connection::open(db_path)
        .map_err(|source| Error::OpenDb { path: db_path.to_owned(), source })?;
    conn.busy_timeout(Duration::ZERO)?;
    if let Err(e) = conn.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;") {
        let busy = e
            .sqlite_error_code()
            .is_some_and(|c| matches!(c, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
        if busy {
            return Err(Error::DbBusy);
        }
        return Err(e).ctx("排他锁探测失败");
    }
    conn.busy_timeout(Duration::from_secs(30))?;
    Ok(conn)
}

/// 写操作收尾: 把 WAL checkpoint 回主库并截断,不留 hot journal。
pub fn checkpoint_truncate(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .ctx("WAL checkpoint 失败")?;
    Ok(())
}

pub struct BackupStatus {
    pub path: PathBuf,
    pub size: u64,
    /// backup 比主库旧多久。
    pub stale_for: Option<Duration>,
    /// `cursor.storage.disableSqliteStorageBackup` 是否为 "true"(true = 永不再更新)。
    pub disabled: Option<bool>,
}

/// 检查 `state.vscdb.backup`。返回 None 表示不存在。
pub fn backup_status(db_path: &Path, conn: &Connection) -> Result<Option<BackupStatus>> {
    let path = PathBuf::from(format!("{}.backup", db_path.display()));
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(None);
    };
    let stale_for = (|| {
        let main = std::fs::metadata(db_path).ok()?.modified().ok()?;
        let bk = meta.modified().ok()?;
        main.duration_since(bk).ok()
    })();
    let disabled = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'cursor.storage.disableSqliteStorageBackup'",
            [],
            |row| {
                let vr = row.get_ref(0)?;
                Ok(match vr {
                    rusqlite::types::ValueRef::Text(t) => Some(t == b"true"),
                    _ => None,
                })
            },
        )
        .unwrap_or(None);
    Ok(Some(BackupStatus { path, size: meta.len(), stale_for, disabled }))
}

/// 列出 `.corrupted.*` 残留(静默回滚发生过的信号,且官方永不清理)。
pub fn corrupted_residue(db_path: &Path) -> Vec<(PathBuf, u64)> {
    let Some(dir) = db_path.parent() else { return Vec::new() };
    let Some(base) = db_path.file_name().and_then(|s| s.to_str()) else { return Vec::new() };
    let prefix = format!("{base}.corrupted.");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&prefix)
            && let Ok(meta) = e.metadata()
        {
            out.push((e.path(), meta.len()));
        }
    }
    out.sort();
    out
}
