//! 孤儿清扫。
//!
//! 只删两类行,都是 Cursor 界面上不可见的死数据:
//! 1. 归属某个 composer、但该 composer 不在任何 header 来源里的行(孤儿);
//! 2. 会话前缀下 value 为 NULL 的行(官方 set-NULL 删除 bug 留下的墓碑)。
//!
//! `agentKv:blob:`(归属要 mark 才知道)、`composer.content.`(无 owner)、
//! `agentKv:artifact:` 与迁移锁(硬约束)一律不碰。
//!
//! 删除前把受影响行原样导出到 sidecar SQLite(保留 TEXT/BLOB 类型),
//! `restore` 子命令可整体回灌。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use rusqlite::types::Value;
use rustc_hash::FxHashSet;

use crate::scan::{Attribution, PrefixStat, attribute};
use crate::{headers, safety};

pub struct SweepPlan {
    /// 将要删除的 key(孤儿行 + 墓碑行)。
    pub keys: Vec<String>,
    pub per_prefix: BTreeMap<&'static str, PrefixStat>,
    pub tombstone_rows: u64,
    pub total_bytes: u64,
    pub orphan_composers: u64,
}

/// 计算清扫计划。`conn` 可以是只读分析连接(dry-run)或写连接(apply)。
/// `with_bytes=false` 时不读 `octet_length`(免触值页,快一个数量级),
/// `total_bytes`/per_prefix bytes 为 0——apply 内部重算计划时用。
pub fn plan(
    conn: &Connection,
    live: &crate::types::LiveSet,
    with_bytes: bool,
    p: &crate::progress::Progress,
) -> Result<SweepPlan> {
    p.stage("扫描孤儿数据", crate::db::kv_row_count(conn)?);
    let mut seen = 0u64;
    let mut keys = Vec::new();
    let mut per_prefix: BTreeMap<&'static str, PrefixStat> = BTreeMap::new();
    let mut orphans: FxHashSet<String> = FxHashSet::default();
    let mut tombstone_rows = 0u64;
    let mut total_bytes = 0u64;

    let sql = if with_bytes {
        "SELECT key, COALESCE(octet_length(value), 0), value IS NULL
         FROM cursorDiskKV WHERE key IS NOT NULL"
    } else {
        "SELECT key, 0, value IS NULL FROM cursorDiskKV WHERE key IS NOT NULL"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        seen += 1;
        if seen.is_multiple_of(1024) {
            p.set_done(seen);
            p.check()?;
        }
        let rusqlite::types::ValueRef::Text(raw) = row.get_ref(0)? else { continue };
        let Ok(key) = std::str::from_utf8(raw) else { continue };

        // 先判定归属再分配,绝大多数行在这里被跳过
        let Attribution::Owned(prefix, cid) = attribute(key) else { continue };
        let is_null: bool = row.get(2)?;
        let orphan = !live.contains(cid);
        if !orphan && !is_null {
            continue;
        }
        let bytes = row.get::<_, i64>(1)?.max(0) as u64;
        if orphan && !orphans.contains(cid) {
            orphans.insert(cid.to_owned());
        }
        if is_null {
            tombstone_rows += 1;
        }
        let stat = per_prefix.entry(prefix).or_default();
        stat.rows += 1;
        stat.bytes += bytes;
        total_bytes += bytes;
        keys.push(key.to_owned());
    }

    Ok(SweepPlan {
        keys,
        per_prefix,
        tombstone_rows,
        total_bytes,
        orphan_composers: orphans.len() as u64,
    })
}

pub struct SweepOutcome {
    pub plan: SweepPlan,
    pub deleted_rows: u64,
    pub backup_path: Option<PathBuf>,
    pub page_size: i64,
    pub page_count_before: i64,
    pub page_count_after: i64,
    /// None = 本次未做物理收缩。
    pub shrink: Option<crate::vacuum::Shrink>,
}

/// 执行清扫。过安全门、在写连接上重算计划(不信任 dry-run 的旧快照)、
/// 备份、分批删除、checkpoint、增量 vacuum。
pub fn apply(
    db_path: &Path,
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<SweepOutcome> {
    let mut conn = safety::open_write_gated(db_path)?;

    let sessions = headers::load_union(&conn)?;
    let live = headers::live_set(&sessions)?;
    // 安全阀: 存活集合为空基本只可能是读错了库;清扫会删掉所有会话数据,拒绝。
    ensure!(!live.is_empty(), "存活会话集合为空,拒绝清扫(库不对或已损坏?)");

    // apply 只需要 key,跳过值页读取
    let plan = plan(&conn, &live, false, p)?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count_before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    if plan.keys.is_empty() {
        return Ok(SweepOutcome {
            plan,
            deleted_rows: 0,
            backup_path: None,
            page_size,
            page_count_before,
            page_count_after: page_count_before,
            shrink: None,
        });
    }

    let (deleted_rows, backup_path) =
        remove_keys(&mut conn, db_path, &plan.keys, "sweep", make_backup, p)?;

    p.stage("checkpoint", 0);
    safety::checkpoint_truncate(&conn)?;
    let shrink = Some(crate::vacuum::shrink_auto(&conn, p)?);
    safety::checkpoint_truncate(&conn)?;
    p.finish();
    let page_count_after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    Ok(SweepOutcome {
        plan,
        deleted_rows,
        backup_path,
        page_size,
        page_count_before,
        page_count_after,
        shrink,
    })
}

/// 按已知 key 清单清扫(维护模式专用)。
///
/// 清单来自持锁会话内的那次扫描: 排他锁保证库不被外部修改,
/// 因此清单恒为精确,无需重扫。同样不做收尾。
pub fn apply_keys(
    conn: &mut Connection,
    db_path: &Path,
    keys: &[String],
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<(u64, Option<PathBuf>)> {
    remove_keys(conn, db_path, keys, "sweep", make_backup, p)
}

/// 备份(可选)+ 分批删除。**不做** checkpoint 与物理收缩:
/// 独立操作由各自的 `apply` 收尾,维护模式则推迟到会话结束统一收尾。
pub fn remove_keys(
    conn: &mut Connection,
    db_path: &Path,
    keys: &[String],
    label: &str,
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<(u64, Option<PathBuf>)> {
    let backup_path = if make_backup {
        let ts = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
        let path = PathBuf::from(format!("{}.{label}-backup-{ts}.sqlite", db_path.display()));
        write_backup(conn, keys, &path, p)?;
        Some(path)
    } else {
        None
    };
    let deleted = delete_keys(conn, keys, p)?;
    Ok((deleted, backup_path))
}

/// 分批事务删除 key(每 5000 行提交一次,避免巨事务撑大 WAL)。
pub fn delete_keys(
    conn: &mut Connection,
    keys: &[String],
    p: &crate::progress::Progress,
) -> Result<u64> {
    p.stage("删除", keys.len() as u64);
    let mut deleted = 0u64;
    for chunk in keys.chunks(5000) {
        let tx = conn.transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM cursorDiskKV WHERE key = ?1")?;
            for key in chunk {
                deleted += del.execute([key.as_str()])? as u64;
            }
        }
        tx.commit()?;
        p.add(chunk.len() as u64);
    }
    Ok(deleted)
}

/// 把将删行原样(保留 TEXT/BLOB 类型)写进 sidecar SQLite。
pub fn write_backup(
    conn: &Connection,
    keys: &[String],
    path: &Path,
    p: &crate::progress::Progress,
) -> Result<()> {
    p.stage("备份将删数据", keys.len() as u64);
    ensure!(!path.exists(), "备份文件已存在: {}", path.display());
    let bk = Connection::open(path)
        .with_context(|| format!("创建备份库失败: {}", path.display()))?;
    bk.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         CREATE TABLE deleted (key TEXT PRIMARY KEY, value);
         CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);",
    )?;
    bk.execute(
        "INSERT INTO meta (k, v) VALUES ('created_at', ?1), ('tool', ?2)",
        [jiff::Zoned::now().to_string(), format!("cursor-chat-cleanup {}", env!("CARGO_PKG_VERSION"))],
    )?;

    let mut get = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;
    let mut ins = bk.prepare("INSERT OR REPLACE INTO deleted (key, value) VALUES (?1, ?2)")?;
    bk.execute_batch("BEGIN")?;
    for (i, key) in keys.iter().enumerate() {
        if i.is_multiple_of(512) {
            p.set_done(i as u64);
        }
        // 保留原始类型: Value 枚举原样搬运。官方在同一列混存 TEXT 与 BLOB
        // 并用类型本身区分编码,类型失真即数据损坏。
        match get.query_row([key.as_str()], |r| r.get::<_, Value>(0)) {
            Ok(v) => {
                ins.execute(rusqlite::params![key, v])?;
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e).context("读取待备份行失败"),
        }
    }
    bk.execute_batch("COMMIT")?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct RestoreOutcome {
    pub kv_rows: u64,
    pub header_rows: u64,
    /// (库路径, key) 恢复的 ItemTable 快照。
    pub item_snapshots: Vec<(String, String)>,
}

/// 从 sidecar 备份整体回灌。sweep/gc 的备份只有 cursorDiskKV 行;
/// delete 的备份还带 header 行与 ItemTable JSON 快照(含 workspace 库),一并恢复。
pub fn restore(db_path: &Path, backup_path: &Path) -> Result<RestoreOutcome> {
    ensure!(backup_path.exists(), "备份文件不存在: {}", backup_path.display());
    let conn = safety::open_write_gated(db_path)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS bk",
        [backup_path.to_str().context("备份路径不是合法 UTF-8")?],
    )?;
    let kv_rows = conn.execute(
        "INSERT OR REPLACE INTO cursorDiskKV (key, value) SELECT key, value FROM bk.deleted",
        [],
    )? as u64;
    let mut out = RestoreOutcome { kv_rows, ..Default::default() };

    let has_table = |name: &str| -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM bk.sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |r| r.get(0),
        )?)
    };

    if has_table("deleted_headers")? {
        out.header_rows = conn.execute(
            "INSERT OR REPLACE INTO composerHeaders SELECT * FROM bk.deleted_headers",
            [],
        )? as u64;
    }

    if has_table("itemtable_snapshot")? {
        let snapshots: Vec<(String, String, rusqlite::types::Value)> = {
            let mut stmt = conn.prepare("SELECT db_path, key, value FROM bk.itemtable_snapshot")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let this_db = db_path.to_string_lossy().to_string();
        for (target_db, key, value) in snapshots {
            if target_db == this_db {
                conn.execute(
                    "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, value],
                )?;
            } else {
                // workspace 库
                let ws = Connection::open(&target_db)
                    .with_context(|| format!("打开 workspace 库失败: {target_db}"))?;
                ws.execute(
                    "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, value],
                )?;
                safety::checkpoint_truncate(&ws)?;
            }
            out.item_snapshots.push((target_db, key));
        }
    }

    conn.execute_batch("DETACH DATABASE bk")?;
    safety::checkpoint_truncate(&conn)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ccc-test-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn setup_fixture(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA auto_vacuum = INCREMENTAL;
            CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
            CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
            CREATE TABLE composerHeaders (
              composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
              lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER,
              recency INTEGER, checkpointAt INTEGER, value TEXT
            );

            INSERT INTO composerHeaders (composerId, createdAt, recency, value)
              VALUES ('live1', 1000, 1000, '{"composerId":"live1","name":"a"}');
            INSERT INTO ItemTable VALUES ('composer.composerHeaders',
              '{"allComposers":[{"type":"head","composerId":"live2","createdAt":2000}]}');

            -- 存活数据: 必须保留
            INSERT INTO cursorDiskKV VALUES ('composerData:live1', 'DATA1');
            INSERT INTO cursorDiskKV VALUES ('bubbleId:live2:b1', x'01020304');
            -- 孤儿: 必须删除
            INSERT INTO cursorDiskKV VALUES ('composerData:dead1', 'DEAD-TEXT');
            INSERT INTO cursorDiskKV VALUES ('bubbleId:dead1:b1', x'aabb');
            INSERT INTO cursorDiskKV VALUES ('agentKv:checkpoint:dead1', 'ffee');
            INSERT INTO cursorDiskKV VALUES ('agentKv:bubbleCheckpoint:dead1:b1', 'ffee');
            INSERT INTO cursorDiskKV VALUES ('composerVirtualRowHeights:dead1', '{}');
            -- 墓碑(value 为 NULL,归属存活会话也要删)
            INSERT INTO cursorDiskKV VALUES ('composerData:live2', NULL);
            -- 硬约束: 以下都不能碰
            INSERT INTO cursorDiskKV VALUES ('agentKv:blob:aabbcc', x'ff');
            INSERT INTO cursorDiskKV VALUES ('agentKv:artifact:x:/p', 'v');
            INSERT INTO cursorDiskKV VALUES ('composer.composerHeaders.migratedToTable', '1');
            INSERT INTO cursorDiskKV VALUES ('composer.content.deadbeef', 'file content');
            INSERT INTO cursorDiskKV VALUES ('ai_hashes.2026-01-01', 'x');
            "#,
        )
        .unwrap();
    }

    fn remaining_keys(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT key FROM cursorDiskKV ORDER BY key").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn sweep_deletes_orphans_and_tombstones_only() {
        let db = temp_db("sweep");
        setup_fixture(&db);

        let outcome = apply(&db, true, &crate::progress::Progress::new()).unwrap();
        assert_eq!(outcome.deleted_rows, 6, "5 孤儿行 + 1 墓碑行");
        assert_eq!(outcome.plan.tombstone_rows, 1);
        assert_eq!(outcome.plan.orphan_composers, 1);

        let conn = Connection::open(&db).unwrap();
        assert_eq!(
            remaining_keys(&conn),
            vec![
                "agentKv:artifact:x:/p",
                "agentKv:blob:aabbcc",
                "ai_hashes.2026-01-01",
                "bubbleId:live2:b1",
                "composer.composerHeaders.migratedToTable",
                "composer.content.deadbeef",
                "composerData:live1",
            ]
        );

        // 备份里有全部被删行,且 TEXT/BLOB 类型保真
        let bk_path = outcome.backup_path.unwrap();
        let bk = Connection::open(&bk_path).unwrap();
        let n: i64 = bk.query_row("SELECT COUNT(*) FROM deleted", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 6);
        let t: String = bk
            .query_row("SELECT typeof(value) FROM deleted WHERE key='composerData:dead1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(t, "text");
        let t: String = bk
            .query_row("SELECT typeof(value) FROM deleted WHERE key='bubbleId:dead1:b1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(t, "blob");

        // restore 整体回灌
        let restored = restore(&db, &bk_path).unwrap();
        assert_eq!(restored.kv_rows, 6);
        assert_eq!(restored.header_rows, 0, "sweep 备份不含 header");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM cursorDiskKV", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 13);
        let t: String = conn
            .query_row("SELECT typeof(value) FROM cursorDiskKV WHERE key='composerData:dead1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(t, "text");

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&bk_path);
    }

    #[test]
    fn scan_counts_live_tombstones() {
        let db = temp_db("scan-tomb");
        setup_fixture(&db);
        let conn = Connection::open(&db).unwrap();
        let sessions = crate::headers::load_union(&conn).unwrap();
        let live = crate::headers::live_set(&sessions).unwrap();
        let scan =
            crate::scan::scan_keys(&conn, &live, false, true, &crate::progress::Progress::new())
                .unwrap();
        assert_eq!(scan.live_tombstone_rows, 1, "composerData:live2 的 NULL 墓碑");
        assert_eq!(scan.orphan_composers.len(), 1);

        // 维护模式零重扫的正确性基石: 扫描收集的待删清单必须与
        // sweep::plan 独立算出的清单逐项一致
        let plan = plan(&conn, &live, false, &crate::progress::Progress::new()).unwrap();
        let mut from_scan = scan.condemned_keys.clone();
        let mut from_plan = plan.keys.clone();
        from_scan.sort();
        from_plan.sort();
        assert_eq!(from_scan, from_plan, "condemned_keys 必须等价于 sweep::plan 的结果");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn apply_refuses_when_db_locked() {
        let db = temp_db("locked");
        setup_fixture(&db);

        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let err = match apply(&db, false, &crate::progress::Progress::new()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("库被独占时 apply 应当失败"),
        };
        assert!(err.contains("占用"), "实际错误: {err}");
        holder.execute_batch("ROLLBACK").unwrap();

        let _ = std::fs::remove_file(&db);
    }
}
