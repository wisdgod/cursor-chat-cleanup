//! 按会话删除。
//!
//! 删一个会话必须同步四处存储,否则会"复活":
//! 1. global `cursorDiskKV`: 该会话全部前缀行(全表扫 + 归属判定,
//!    规避 `agentKv:checkpoint:` 无尾冒号的前缀碰撞问题);
//! 2. global `composerHeaders` 表: `DELETE WHERE composerId = ?`;
//! 3. global `ItemTable['composer.composerHeaders']`: 从 `allComposers` 剔除
//!    (即使 gate 开着也必须做——表清空时会回落读这份 blob);
//! 4. 每个 workspace 库的 `ItemTable['composer.composerData']`: 剔除
//!    `allComposers` / `selectedComposerIds` / `lastFocusedComposerIds`。
//!
//! 硬约束: 不碰迁移锁;不碰 `agentKv:blob:`(交给 gc);全程要求 Cursor 退出。
//! 所有改动(行 + JSON 原文)先快照进 sidecar,`restore` 可整体回滚。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};
use rustc_hash::FxHashSet;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::scan::{Attribution, PrefixStat, attribute};
use crate::{headers, safety, sweep};

pub struct DeletePlan {
    /// 目标会话(已解析)。
    pub targets: Vec<Target>,
    /// 将删除的 cursorDiskKV key。
    pub keys: Vec<String>,
    pub per_prefix: BTreeMap<&'static str, PrefixStat>,
    pub total_bytes: u64,
}

pub struct Target {
    /// 解析即证明的会话 id(resolve 是唯一构造点)。
    pub composer_id: crate::types::ComposerId,
    pub name: Option<String>,
    pub in_header_table: bool,
    pub in_legacy_blob: bool,
}

/// 把用户输入(完整 id 或 >=6 位前缀)解析成唯一的 composerId。
pub fn resolve_targets(sessions: &[headers::SessionHeader], args: &[String]) -> Result<Vec<Target>> {
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();
    for arg in args {
        let arg = arg.trim();
        if arg.len() < 6 {
            return Err(crate::types::IdError::TooShort { raw: arg.into(), min: 6 }.into());
        }
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.composer_id == *arg || s.composer_id.starts_with(arg))
            .collect();
        match matches.len() {
            0 => return Err(Error::TargetNotFound { arg: arg.to_owned() }),
            1 => {
                let s = matches[0];
                if seen.insert(s.composer_id.clone()) {
                    out.push(Target {
                        composer_id: crate::types::ComposerId::parse(&s.composer_id)?,
                        name: s.name.clone(),
                        in_header_table: s.in_header_table,
                        in_legacy_blob: s.in_legacy_blob,
                    });
                }
            }
            _ => {
                return Err(Error::AmbiguousPrefix {
                    arg: arg.to_owned(),
                    candidates: matches.iter().map(|s| s.composer_id.clone()).collect(),
                });
            }
        }
    }
    Ok(out)
}

/// 收集目标会话在 cursorDiskKV 里的全部行。
pub fn plan(
    conn: &Connection,
    targets: Vec<Target>,
    p: &crate::progress::Progress,
) -> Result<DeletePlan> {
    let ids: FxHashSet<&str> = targets.iter().map(|t| t.composer_id.as_str()).collect();
    let (keys, per_prefix, total_bytes) = collect_session_keys(conn, &ids, None, p)?;
    Ok(DeletePlan { targets, keys, per_prefix, total_bytes })
}

/// 目标会话在各前缀下的查询形态。
/// `Colon` = `{前缀}{cid}:` 范围;`Exact` = 单 key;
/// `Bare` = `{前缀}{cid}` 起始的范围(官方 key 可能带后缀,如行高缓存)。
enum KeyShape {
    Colon,
    Exact,
    Bare,
}

/// 全部归属前缀及其形态(与 `scan::attribute` 的命名一致)。
const SESSION_PREFIXES: &[(&str, &str, KeyShape)] = &[
    ("bubbleId", "bubbleId:", KeyShape::Colon),
    ("checkpointId", "checkpointId:", KeyShape::Colon),
    ("codeBlockDiff", "codeBlockDiff:", KeyShape::Colon),
    (
        "codeBlockPartialInlineDiffFates",
        "codeBlockPartialInlineDiffFates:",
        KeyShape::Colon,
    ),
    ("ofsContent", "ofsContent:", KeyShape::Colon),
    ("agentKv:bubbleCheckpoint", "agentKv:bubbleCheckpoint:", KeyShape::Colon),
    ("composerVirtualRowHeights", "composerVirtualRowHeights:", KeyShape::Bare),
    ("composerData", "composerData:", KeyShape::Exact),
    // 官方对该前缀的删除无尾部冒号,存在前缀碰撞隐患;Exact 匹配天然规避
    ("agentKv:checkpoint", "agentKv:checkpoint:", KeyShape::Exact),
];

/// 收集目标会话的 cursorDiskKV key: 逐前缀范围/单点查询,只触达目标行,
/// 不做全表扫描。每个命中 key 再过 `attribute()` 验证归属,规避
/// 变长 id 的前缀碰撞(如 `task-` 系 id 互为前缀的情形)。
///
/// `allowed`: 只收集指定前缀(trim 用);None = 全部。
pub fn collect_session_keys(
    conn: &Connection,
    ids: &FxHashSet<&str>,
    allowed: Option<&[&str]>,
    p: &crate::progress::Progress,
) -> Result<(Vec<String>, BTreeMap<&'static str, PrefixStat>, u64)> {
    let shapes: Vec<&(&str, &str, KeyShape)> = SESSION_PREFIXES
        .iter()
        .filter(|(name, _, _)| allowed.is_none_or(|a| a.contains(name)))
        .collect();
    p.stage("收集会话数据", (ids.len() * shapes.len()) as u64);

    let mut keys = Vec::new();
    let mut per_prefix: BTreeMap<&'static str, PrefixStat> = BTreeMap::new();
    let mut total_bytes = 0u64;

    let mut by_range = conn.prepare(
        "SELECT key, COALESCE(octet_length(value), 0) FROM cursorDiskKV
         WHERE key >= ?1 AND key < ?2",
    )?;
    let mut by_key = conn.prepare(
        "SELECT key, COALESCE(octet_length(value), 0) FROM cursorDiskKV WHERE key = ?1",
    )?;

    // 末字节 +1(官方前缀上界构造的等价实现;前缀与 id 均为 ASCII)
    fn upper(mut s: String) -> String {
        let last = s.pop().expect("non-empty by construction in caller") as u8;
        s.push((last + 1) as char);
        s
    }

    for cid in ids {
        for (_, prefix, shape) in &shapes {
            let mut on_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<()> {
                let ValueRef::Text(raw) = row.get_ref(0)? else { return Ok(()) };
                let Ok(key) = std::str::from_utf8(raw) else { return Ok(()) };
                // 归属必须恰为本次查询的 cid: 既防非目标会话的前缀碰撞,
                // 也防两个目标 id 互为前缀时同一行被计入两次
                let Attribution::Owned(name, owner) = attribute(key) else { return Ok(()) };
                if owner != *cid {
                    return Ok(());
                }
                let bytes = row.get::<_, i64>(1)?.max(0) as u64;
                let stat = per_prefix.entry(name).or_default();
                stat.rows += 1;
                stat.bytes += bytes;
                total_bytes += bytes;
                keys.push(key.to_owned());
                Ok(())
            };
            match shape {
                KeyShape::Colon => {
                    let lo = format!("{prefix}{cid}:");
                    let hi = format!("{prefix}{cid};"); // ':' + 1 = ';'
                    let mut rows = by_range.query(rusqlite::params![lo, hi])?;
                    while let Some(row) = rows.next()? {
                        on_row(row)?;
                    }
                }
                KeyShape::Bare => {
                    let lo = format!("{prefix}{cid}");
                    let hi = upper(lo.clone());
                    let mut rows = by_range.query(rusqlite::params![lo, hi])?;
                    while let Some(row) = rows.next()? {
                        on_row(row)?;
                    }
                }
                KeyShape::Exact => {
                    let key = format!("{prefix}{cid}");
                    let mut rows = by_key.query([key.as_str()])?;
                    while let Some(row) = rows.next()? {
                        on_row(row)?;
                    }
                }
            }
            p.add(1);
        }
    }
    keys.sort_unstable();
    Ok((keys, per_prefix, total_bytes))
}

pub struct DeleteOutcome {
    pub plan: DeletePlan,
    pub deleted_rows: u64,
    pub deleted_header_rows: u64,
    pub legacy_blob_edited: bool,
    /// 被改写的 workspace 库。
    pub workspaces_edited: Vec<PathBuf>,
    pub backup_path: Option<PathBuf>,
}

pub fn apply(
    db_path: &Path,
    target_args: &[String],
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<DeleteOutcome> {
    let mut conn = safety::open_write_gated(db_path)?;
    apply_on(&mut conn, db_path, target_args, make_backup, true, p)
}

/// 在既有写连接上执行删除。`finalize=false` 时跳过 checkpoint 与物理收缩,
/// 由维护模式在会话结束统一收尾。
pub fn apply_on(
    conn: &mut Connection,
    db_path: &Path,
    target_args: &[String],
    make_backup: bool,
    finalize: bool,
    p: &crate::progress::Progress,
) -> Result<DeleteOutcome> {
    let sessions = headers::load_union(conn)?;
    let targets = resolve_targets(&sessions, target_args)?;
    if targets.is_empty() {
        return Err(Error::NoTargets);
    }
    if targets.len() >= sessions.len() {
        return Err(Error::RefuseWipeAll);
    }

    let plan = plan(conn, targets, p)?;
    let ids: FxHashSet<String> =
        plan.targets.iter().map(|t| t.composer_id.as_str().to_owned()).collect();

    // ---- 快照(先于任何改动) ----
    let backup_path = if make_backup {
        let ts = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
        let path = PathBuf::from(format!("{}.delete-backup-{ts}.sqlite", db_path.display()));
        sweep::write_backup(conn, &plan.keys, &path, p)?;
        snapshot_extras(conn, db_path, &ids, &path)?;
        Some(path)
    } else {
        None
    };

    // ---- 1. cursorDiskKV 行 ----
    let deleted_rows = sweep::delete_keys(conn, &plan.keys, p)?;

    // ---- 2. composerHeaders 表 ----
    let mut deleted_header_rows = 0u64;
    for id in &ids {
        deleted_header_rows +=
            conn.execute("DELETE FROM composerHeaders WHERE composerId = ?1", [id])? as u64;
    }

    // ---- 3. global ItemTable 的两个 blob ----
    let legacy_blob_edited = edit_item_table_json(conn, "composer.composerHeaders", &ids)?
        | edit_item_table_json(conn, "composer.composerData", &ids)?;

    // ---- 4. workspace 库 ----
    p.stage("清理 workspace 库", 0);
    let mut workspaces_edited = Vec::new();
    for ws_db in workspace_dbs(db_path) {
        match edit_workspace_db(&ws_db, &ids, backup_path.as_deref()) {
            Ok(true) => workspaces_edited.push(ws_db),
            Ok(false) => {}
            // workspace 库损坏/被占不应中止主流程,记下来提醒用户即可
            Err(e) => {
                eprintln!("警告: workspace 库处理失败 {}: {}", ws_db.display(), e.render())
            }
        }
    }

    if finalize {
        p.stage("checkpoint", 0);
        safety::checkpoint_truncate(conn)?;
        let _ = crate::vacuum::shrink_auto(conn, p)?;
        safety::checkpoint_truncate(conn)?;
        p.finish();
    }

    Ok(DeleteOutcome {
        plan,
        deleted_rows,
        deleted_header_rows,
        legacy_blob_edited,
        workspaces_edited,
        backup_path,
    })
}

/// 向 sweep 生成的 sidecar 里追加 header 行与 ItemTable JSON 快照。
fn snapshot_extras(conn: &Connection, db_path: &Path, ids: &FxHashSet<String>, backup: &Path) -> Result<()> {
    let bk = Connection::open(backup)?;
    bk.execute_batch(
        "CREATE TABLE IF NOT EXISTS deleted_headers (
           composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER,
           lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER,
           recency INTEGER, checkpointAt INTEGER, value TEXT);
         CREATE TABLE IF NOT EXISTS itemtable_snapshot (
           db_path TEXT, key TEXT, value, PRIMARY KEY (db_path, key));",
    )?;

    let mut get_header = conn.prepare(
        "SELECT composerId, workspaceId, createdAt, lastUpdatedAt, isArchived,
                isSubagent, recency, checkpointAt, value
         FROM composerHeaders WHERE composerId = ?1",
    )?;
    let mut ins_header = bk.prepare(
        "INSERT OR REPLACE INTO deleted_headers VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
    )?;
    for id in ids {
        let row = get_header
            .query_row([id.as_str()], |r| {
                (0..9)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()?;
        if let Some(vals) = row {
            ins_header.execute(rusqlite::params_from_iter(vals))?;
        }
    }

    let mut ins_item = bk.prepare("INSERT OR REPLACE INTO itemtable_snapshot VALUES (?1, ?2, ?3)")?;
    for key in ["composer.composerHeaders", "composer.composerData"] {
        if let Some(v) = read_item(conn, key)? {
            ins_item.execute(rusqlite::params![db_path.to_string_lossy(), key, v])?;
        }
    }
    Ok(())
}

fn read_item(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
        })
        .optional()?
        .flatten())
}

/// 从 ItemTable 的 JSON blob 里剔除目标 id。返回是否有改动。
/// 覆盖三种形态: `allComposers`(header 对象数组)、
/// `selectedComposerIds` / `lastFocusedComposerIds`(id 字符串数组)。
fn edit_item_table_json(conn: &Connection, key: &str, ids: &FxHashSet<String>) -> Result<bool> {
    let Some(raw) = read_item(conn, key)? else { return Ok(false) };
    let Ok(mut json) = serde_json::from_str::<Value>(&raw) else {
        return Ok(false); // 解析不了就不动,宁可留旧数据也不写坏
    };
    if !strip_ids_from_json(&mut json, ids) {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, to_json_string(&json)?],
    )?;
    Ok(true)
}

/// 序列化清洗后的 ItemTable JSON(输入来自 `serde_json::Value`,
/// 失败只可能是极端情形,但仍按契约传播而非 unwrap)。
fn to_json_string(v: &Value) -> Result<String> {
    serde_json::to_string(v).map_err(|source| Error::Json { what: "序列化 ItemTable", source })
}

/// 就地清洗 JSON。返回是否有改动。
fn strip_ids_from_json(json: &mut Value, ids: &FxHashSet<String>) -> bool {
    let Some(obj) = json.as_object_mut() else { return false };
    let mut changed = false;
    if let Some(all) = obj.get_mut("allComposers").and_then(Value::as_array_mut) {
        let before = all.len();
        all.retain(|h| {
            h.get("composerId")
                .and_then(Value::as_str)
                .is_none_or(|id| !ids.contains(id))
        });
        changed |= all.len() != before;
    }
    for list_key in ["selectedComposerIds", "lastFocusedComposerIds"] {
        if let Some(list) = obj.get_mut(list_key).and_then(Value::as_array_mut) {
            let before = list.len();
            list.retain(|v| v.as_str().is_none_or(|id| !ids.contains(id)));
            changed |= list.len() != before;
        }
    }
    changed
}

/// 枚举同一 User 目录下的所有 workspace 库。
fn workspace_dbs(global_db: &Path) -> Vec<PathBuf> {
    let Some(user_dir) = global_db.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(user_dir.join("workspaceStorage")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path().join("state.vscdb");
        if p.exists() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// 清洗一个 workspace 库。返回是否有改动。改动前把原 JSON 快照进 sidecar。
fn edit_workspace_db(ws_db: &Path, ids: &FxHashSet<String>, backup: Option<&Path>) -> Result<bool> {
    let conn = Connection::open(ws_db)
        .map_err(|source| Error::OpenWorkspaceDb { path: ws_db.to_owned(), source })?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    // 有的 workspace 库连 ItemTable 都没有
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ItemTable')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(false);
    }

    let key = "composer.composerData";
    let Some(raw) = read_item(&conn, key)? else { return Ok(false) };
    let Ok(mut json) = serde_json::from_str::<Value>(&raw) else { return Ok(false) };
    if !strip_ids_from_json(&mut json, ids) {
        return Ok(false);
    }

    if let Some(backup) = backup {
        let bk = Connection::open(backup)?;
        bk.execute(
            "INSERT OR REPLACE INTO itemtable_snapshot VALUES (?1, ?2, ?3)",
            rusqlite::params![ws_db.to_string_lossy(), key, raw],
        )?;
    }

    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, to_json_string(&json)?],
    )?;
    safety::checkpoint_truncate(&conn)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep;

    const L1: &str = "live1-aaaaaa-0000";
    const L2: &str = "live2-bbbbbb-0000";

    /// User/globalStorage/state.vscdb + User/workspaceStorage/w1/state.vscdb
    fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("ccc-del-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let global_dir = root.join("User/globalStorage");
        let ws_dir = root.join("User/workspaceStorage/w1");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::create_dir_all(&ws_dir).unwrap();
        let global = global_dir.join("state.vscdb");
        let ws = ws_dir.join("state.vscdb");

        let conn = Connection::open(&global).unwrap();
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
              VALUES ('{L1}', 1000, 1000, '{{"composerId":"{L1}","name":"keep"}}'),
                     ('{L2}', 2000, 2000, '{{"composerId":"{L2}","name":"kill"}}');
            INSERT INTO ItemTable VALUES ('composer.composerHeaders',
              '{{"allComposers":[{{"type":"head","composerId":"{L1}","createdAt":1000}},{{"type":"head","composerId":"{L2}","createdAt":2000}}]}}');
            INSERT INTO ItemTable VALUES ('composer.composerData',
              '{{"selectedComposerIds":["{L2}"],"lastFocusedComposerIds":["{L2}","{L1}"],"hasMigratedComposerData":true}}');

            INSERT INTO cursorDiskKV VALUES ('composerData:{L1}', 'KEEP');
            INSERT INTO cursorDiskKV VALUES ('bubbleId:{L1}:b1', 'KEEP');
            INSERT INTO cursorDiskKV VALUES ('composerData:{L2}', 'KILL');
            INSERT INTO cursorDiskKV VALUES ('bubbleId:{L2}:b1', x'aabb');
            INSERT INTO cursorDiskKV VALUES ('checkpointId:{L2}:c1', 'KILL');
            INSERT INTO cursorDiskKV VALUES ('agentKv:checkpoint:{L2}', 'ffee');
            INSERT INTO cursorDiskKV VALUES ('agentKv:blob:aabbcc', x'ff');
            INSERT INTO cursorDiskKV VALUES ('composer.composerHeaders.migratedToTable', '1');
            "#,
        ))
        .unwrap();
        drop(conn);

        let wconn = Connection::open(&ws).unwrap();
        wconn
            .execute_batch(&format!(
                r#"
            CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
            INSERT INTO ItemTable VALUES ('composer.composerData',
              '{{"allComposers":[{{"type":"head","composerId":"{L2}","createdAt":2000}}],"selectedComposerIds":["{L2}"],"lastFocusedComposerIds":["{L1}"]}}');
            "#,
            ))
            .unwrap();
        drop(wconn);

        (root, global, ws)
    }

    fn kv_count(conn: &Connection, like: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE ?1",
            [like],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn delete_syncs_all_four_stores_and_restores() {
        let (root, global, ws) = setup("sync");

        // 用 6 位前缀解析
        let outcome =
            apply(&global, &[L2[..8].to_string()], true, &crate::progress::Progress::new())
                .unwrap();
        assert_eq!(outcome.deleted_rows, 4, "composerData+bubble+checkpoint+agentKv:checkpoint");
        assert_eq!(outcome.deleted_header_rows, 1);
        assert!(outcome.legacy_blob_edited);
        assert_eq!(outcome.workspaces_edited, vec![ws.clone()]);

        let conn = Connection::open(&global).unwrap();
        // 1: cursorDiskKV — L2 全没,L1 与硬约束行都在
        assert_eq!(kv_count(&conn, &format!("%{L2}%")), 0);
        assert_eq!(kv_count(&conn, &format!("%{L1}%")), 2);
        assert_eq!(kv_count(&conn, "agentKv:blob:%"), 1);
        assert_eq!(kv_count(&conn, "composer.composerHeaders.migratedToTable"), 1);
        // 2: header 表
        let headers_left: i64 =
            conn.query_row("SELECT COUNT(*) FROM composerHeaders", [], |r| r.get(0)).unwrap();
        assert_eq!(headers_left, 1);
        // 3: 全局旧 blob 与选中态
        let blob = read_item(&conn, "composer.composerHeaders").unwrap().unwrap();
        assert!(blob.contains(L1) && !blob.contains(L2));
        let data = read_item(&conn, "composer.composerData").unwrap().unwrap();
        assert!(!data.contains(L2) && data.contains(L1));
        assert!(data.contains("hasMigratedComposerData"), "无关字段必须保留");
        // 4: workspace 库
        let wconn = Connection::open(&ws).unwrap();
        let wdata = read_item(&wconn, "composer.composerData").unwrap().unwrap();
        assert!(!wdata.contains(L2) && wdata.contains(L1));
        drop(wconn);
        drop(conn);

        // 回滚: 四处全部还原
        let bk = outcome.backup_path.unwrap();
        let restored = sweep::restore(&global, &bk).unwrap();
        assert_eq!(restored.kv_rows, 4);
        assert_eq!(restored.header_rows, 1);
        assert_eq!(restored.item_snapshots.len(), 3, "全局两个 key + workspace 一个");

        let conn = Connection::open(&global).unwrap();
        assert_eq!(kv_count(&conn, &format!("%{L2}%")), 4);
        let blob = read_item(&conn, "composer.composerHeaders").unwrap().unwrap();
        assert!(blob.contains(L2), "旧 blob 已还原");
        let wconn = Connection::open(&ws).unwrap();
        let wdata = read_item(&wconn, "composer.composerData").unwrap().unwrap();
        assert!(wdata.contains(L2), "workspace 库已还原");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_refuses_wiping_all_sessions() {
        let (root, global, _ws) = setup("refuse");
        let err = match apply(
            &global,
            &[L1.to_string(), L2.to_string()],
            false,
            &crate::progress::Progress::new(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("删除全部会话应当被拒绝"),
        };
        assert!(matches!(err, Error::RefuseWipeAll), "实际错误: {}", err.render());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_rejects_ambiguous_prefix() {
        let sessions = vec![
            headers::SessionHeader {
                composer_id: "abcdef-1".into(),
                name: None,
                created_at: None,
                last_updated_at: None,
                recency: 0,
                is_archived: false,
                is_subagent: false,
                workspace_id: None,
                workspace_folder: None,
                parent_composer_id: None,
                in_header_table: true,
                in_legacy_blob: false,
            },
            headers::SessionHeader {
                composer_id: "abcdef-2".into(),
                name: None,
                created_at: None,
                last_updated_at: None,
                recency: 0,
                is_archived: false,
                is_subagent: false,
                workspace_id: None,
                workspace_folder: None,
                parent_composer_id: None,
                in_header_table: true,
                in_legacy_blob: false,
            },
        ];
        assert!(resolve_targets(&sessions, &["abcdef".into()]).is_err());
        assert!(resolve_targets(&sessions, &["abcdef-1".into()]).is_ok());
    }
}
