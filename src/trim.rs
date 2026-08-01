//! 巨型会话的部分清理。
//!
//! 与 delete 不同,trim 后会话仍然存活可读,只删"文件快照"类数据:
//! - `composerVirtualRowHeights:`: 纯 UI 行高缓存,零影响(最保守档);
//! - `checkpointId:` / `ofsContent:`: 检查点文件快照,只影响"恢复到检查点",
//!   不影响阅读历史(默认档)。这类快照往往是巨型会话体积的大头。
//!
//! 有意不碰: `bubbleId:`/`composerData:`(正文)、`agentKv:checkpoint`
//! 指针(新检查点系统,留给官方语义)、`codeBlock*`(体积小且影响 UI 状态)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use rusqlite::Connection;
use rustc_hash::FxHashSet;

use crate::delete::{Target, resolve_targets};
use crate::scan::PrefixStat;
use crate::{headers, safety, sweep};

/// 可修剪的前缀集合。
fn trimmable(ui_only: bool) -> &'static [&'static str] {
    if ui_only {
        &["composerVirtualRowHeights"]
    } else {
        &["composerVirtualRowHeights", "checkpointId", "ofsContent"]
    }
}

pub struct TrimPlan {
    pub targets: Vec<Target>,
    pub keys: Vec<String>,
    pub per_prefix: BTreeMap<&'static str, PrefixStat>,
    pub total_bytes: u64,
}

pub fn plan(
    conn: &Connection,
    targets: Vec<Target>,
    ui_only: bool,
    p: &crate::progress::Progress,
) -> Result<TrimPlan> {
    let ids: FxHashSet<&str> = targets.iter().map(|t| t.composer_id.as_str()).collect();
    let (keys, per_prefix, total_bytes) =
        crate::delete::collect_session_keys(conn, &ids, Some(trimmable(ui_only)), p)?;
    Ok(TrimPlan { targets, keys, per_prefix, total_bytes })
}

pub struct TrimOutcome {
    pub plan: TrimPlan,
    pub deleted_rows: u64,
    pub backup_path: Option<PathBuf>,
    pub page_size: i64,
    pub page_count_before: i64,
    pub page_count_after: i64,
    /// None = 本次未做物理收缩(维护模式推迟到显式的 vacuum 动作)。
    pub shrink: Option<crate::vacuum::Shrink>,
}

pub fn apply(
    db_path: &Path,
    target_args: &[String],
    ui_only: bool,
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<TrimOutcome> {
    let mut conn = safety::open_write_gated(db_path)?;
    apply_on(&mut conn, db_path, target_args, ui_only, make_backup, true, p)
}

/// 在既有写连接上执行修剪。`finalize=false` 时跳过收尾(维护模式)。
pub fn apply_on(
    conn: &mut Connection,
    db_path: &Path,
    target_args: &[String],
    ui_only: bool,
    make_backup: bool,
    finalize: bool,
    p: &crate::progress::Progress,
) -> Result<TrimOutcome> {
    let sessions = headers::load_union(conn)?;
    let targets = resolve_targets(&sessions, target_args)?;
    ensure!(!targets.is_empty(), "没有要修剪的会话");

    let plan = plan(conn, targets, ui_only, p)?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count_before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    if plan.keys.is_empty() {
        return Ok(TrimOutcome {
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
        sweep::remove_keys(conn, db_path, &plan.keys, "trim", make_backup, p)?;

    let mut shrink = None;
    if finalize {
        p.stage("checkpoint", 0);
        safety::checkpoint_truncate(conn)?;
        shrink = Some(crate::vacuum::shrink_auto(conn, p)?);
        safety::checkpoint_truncate(conn)?;
        p.finish();
    }
    let page_count_after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    Ok(TrimOutcome {
        plan,
        deleted_rows,
        backup_path,
        page_size,
        page_count_before,
        page_count_after,
        shrink,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const L1: &str = "live1-aaaaaa-0000";

    fn temp_db(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ccc-trim-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn setup(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            r#"
            CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
            CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
            CREATE TABLE composerHeaders (
              composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
              lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER,
              recency INTEGER, checkpointAt INTEGER, value TEXT
            );
            INSERT INTO composerHeaders (composerId, createdAt, recency, value)
              VALUES ('{L1}', 1000, 1000, '{{"composerId":"{L1}","name":"big"}}');

            INSERT INTO cursorDiskKV VALUES ('composerData:{L1}', 'BODY');
            INSERT INTO cursorDiskKV VALUES ('bubbleId:{L1}:b1', 'MSG');
            INSERT INTO cursorDiskKV VALUES ('checkpointId:{L1}:c1', 'SNAPSHOT');
            INSERT INTO cursorDiskKV VALUES ('ofsContent:{L1}:f1', 'FILE');
            INSERT INTO cursorDiskKV VALUES ('composerVirtualRowHeights:{L1}', '{{}}');
            INSERT INTO cursorDiskKV VALUES ('agentKv:checkpoint:{L1}', 'ffee');
            "#,
        ))
        .unwrap();
    }

    fn remaining(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT key FROM cursorDiskKV ORDER BY key").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn trim_removes_snapshots_keeps_history() {
        let db = temp_db("default");
        setup(&db);

        let outcome =
            apply(&db, &[L1.to_string()], false, true, &crate::progress::Progress::new()).unwrap();
        assert_eq!(outcome.deleted_rows, 3, "checkpoint + ofs + rowHeights");

        let conn = Connection::open(&db).unwrap();
        assert_eq!(
            remaining(&conn),
            vec![
                format!("agentKv:checkpoint:{L1}"),
                format!("bubbleId:{L1}:b1"),
                format!("composerData:{L1}"),
            ],
            "正文与 agentKv 指针必须保留"
        );
        // header 不动,会话仍存活
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM composerHeaders", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        drop(conn);

        // 可回滚
        let restored = sweep::restore(&db, &outcome.backup_path.unwrap()).unwrap();
        assert_eq!(restored.kv_rows, 3);

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn trim_ui_only_removes_row_heights_only() {
        let db = temp_db("ui");
        setup(&db);

        let outcome =
            apply(&db, &[L1.to_string()], true, false, &crate::progress::Progress::new()).unwrap();
        assert_eq!(outcome.deleted_rows, 1);

        let conn = Connection::open(&db).unwrap();
        assert!(!remaining(&conn).iter().any(|k| k.starts_with("composerVirtualRowHeights:")));
        assert!(remaining(&conn).iter().any(|k| k.starts_with("checkpointId:")));

        let _ = std::fs::remove_file(&db);
    }
}
