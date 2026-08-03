//! 维护模式: 以 `locking_mode=EXCLUSIVE` 持有整库排他锁的长会话。
//!
//! 锁在手 ⇒ 库在会话期间不可能被外部修改 ⇒ 一次扫描建立的内存模型
//! **恒为精确**,后续操作直接按模型执行,无需任何重扫;checkpoint 与
//! 物理收缩推迟到会话结束做一次。
//!
//! 风险(必须在 UI 常驻警示): 持锁期间启动 Cursor 会触发它的静默回滚
//! (连续两次 SQLITE_BUSY 即把主库改名、拿 .backup 顶替)。
//! 这与普通 apply 期间的风险同源,维护模式只是把窗口拉长。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Ctx as _, Result};
use crate::safety;

/// 持锁的维护会话。`drop` 即释放锁(连接关闭),
/// 但正常路径应走 [`Maintenance::release`] 以完成 checkpoint 与收缩。
pub struct Maintenance {
    conn: Connection,
    db_path: PathBuf,
    /// 会话内已执行的写操作数(gc 计划缓存的有效性凭据)。
    writes: u64,
}

impl Maintenance {
    /// 过安全门并取得排他锁。锁在第一笔写事务后被持续持有。
    pub fn acquire(db_path: &Path) -> Result<Self> {
        let conn = safety::open_write_gated(db_path)?;
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .ctx("设置 locking_mode=EXCLUSIVE 失败")?;
        // locking_mode 在下一次访问时生效;做一笔空写事务实际取得并持有锁
        conn.execute_batch("BEGIN IMMEDIATE; COMMIT;").ctx("取得排他锁失败")?;
        Ok(Self { conn, db_path: db_path.to_owned(), writes: 0 })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// 会话写计数(每次成功的删除类操作后调用)。
    pub fn bump_writes(&mut self) {
        self.writes += 1;
    }

    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// 结束会话: checkpoint(不留 hot journal)、恢复 NORMAL 锁模式并关闭。
    ///
    /// **有意不做物理收缩**——退出的硬要求只有 WAL 干净;收缩是独立的
    /// 维护动作(体量可达数 GB、慢文件系统上以小时计),必须由用户显式
    /// 触发且可取消,不能把"退出"卡在它后面。
    /// 返回仍待回收的空闲字节数(供退出提示)。
    pub fn release(self, p: &crate::progress::Progress) -> Result<u64> {
        let conn = self.conn;
        p.stage("checkpoint", 0);
        safety::checkpoint_truncate(&conn)?;
        let (_, freelist_bytes) = crate::vacuum::freelist(&conn)?;
        // 恢复 NORMAL 并做一次访问以真正放锁,随后关闭连接
        conn.pragma_update(None, "locking_mode", "NORMAL")?;
        let _ = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?;
        safety::checkpoint_truncate(&conn)?;
        drop(conn);
        p.finish();
        Ok(freelist_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_db(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("ccc-maint-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             INSERT INTO cursorDiskKV VALUES ('k', 'v');",
        )
        .unwrap();
        p
    }

    #[test]
    fn exclusive_lock_blocks_others_until_release() {
        let db = temp_db("lock");
        let maint = Maintenance::acquire(&db).unwrap();

        // 持锁期间: 第二个连接连读都进不来(EXCLUSIVE 对 WAL 是全排他)
        let other = Connection::open(&db).unwrap();
        other.busy_timeout(Duration::ZERO).unwrap();
        let blocked = other
            .query_row("SELECT value FROM cursorDiskKV WHERE key='k'", [], |r| {
                r.get::<_, String>(0)
            })
            .is_err();
        assert!(blocked, "排他锁必须阻塞外部访问");

        // 会话内自己的写事务正常工作
        maint.conn().execute("INSERT INTO cursorDiskKV VALUES ('k2', 'v2')", []).unwrap();

        // 释放后外部恢复访问
        maint.release(&crate::progress::Progress::new()).unwrap();
        let v: String = other
            .query_row("SELECT value FROM cursorDiskKV WHERE key='k2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v2");

        let _ = std::fs::remove_file(&db);
    }
}
