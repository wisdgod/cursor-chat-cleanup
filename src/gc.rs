//! `agentKv:blob:` 的 mark & sweep。
//!
//! mark 见 `mark.rs`。本模块负责: 全量 blob 清单、孤儿差集、
//! 安全阀检查、备份 + 删除 + 物理收缩。`composer.content.` 的孤儿
//! (官方清理命令不覆盖的命名空间)一并回收;`agentKv:artifact:` 不碰。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use rustc_hash::FxHashMap;

use crate::error::{Error, Result};
use crate::{headers, mark, safety, sweep};

/// 根解码失败率超过该值即拒绝清理(数据可能损坏)。
const ROOT_ERROR_RATE_LIMIT: f64 = 0.02;

pub struct GcPlan {
    pub total_blobs: u64,
    pub total_bytes: u64,
    pub live_blobs: u64,
    pub live_bytes: u64,
    /// 将删除的孤儿 blob(小写 hex)。
    pub orphans: Vec<String>,
    pub orphan_bytes: u64,
    /// 被引用但不存在的 blob 数(官方 GC 历史损伤的信号)。
    pub dangling_refs: u64,
    /// `composer.content.` 命名空间(官方清理命令不覆盖,本工具一并回收)。
    pub content_total: u64,
    pub content_bytes: u64,
    /// 将删除的 content 完整 key。
    pub content_orphans: Vec<String>,
    pub content_orphan_bytes: u64,
    pub stats: mark::MarkStats,
    pub root_error_rate: f64,
    pub listing_phase: std::time::Duration,
}

pub fn plan(
    conn: &Connection,
    live: &crate::types::LiveSet,
    p: &crate::progress::Progress,
) -> Result<GcPlan> {
    // 全量 blob 清单(hex → 字节数)
    let listing_start = std::time::Instant::now();
    p.stage("清点 blob", 0);
    let mut all: FxHashMap<String, u64> = FxHashMap::default();
    {
        let mut stmt = conn.prepare(
            "SELECT key, COALESCE(octet_length(value), 0) FROM cursorDiskKV
             WHERE key >= 'agentKv:blob:' AND key < 'agentKv:blob;'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let ValueRef::Text(key_raw) = row.get_ref(0)? else { continue };
            let Ok(key) = std::str::from_utf8(key_raw) else { continue };
            let Some(hex) = key.strip_prefix("agentKv:blob:") else { continue };
            // 官方写入即为小写;仅在异常样本上付大小写转换的分配
            let hex = if hex.bytes().any(|b| b.is_ascii_uppercase()) {
                hex.to_ascii_lowercase()
            } else {
                hex.to_owned()
            };
            let bytes = row.get::<_, i64>(1)?.max(0) as u64;
            all.insert(hex, bytes);
            if all.len().is_multiple_of(1024) {
                p.add(1024);
                p.check()?;
            }
        }
    }

    // composer.content 清单(key → 字节数)
    let mut content_rows: Vec<(String, u64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT key, COALESCE(octet_length(value), 0) FROM cursorDiskKV
             WHERE key >= 'composer.content.' AND key < 'composer.content/'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let ValueRef::Text(key_raw) = row.get_ref(0)? else { continue };
            let Ok(key) = std::str::from_utf8(key_raw) else { continue };
            let bytes = row.get::<_, i64>(1)?.max(0) as u64;
            content_rows.push((key.to_owned(), bytes));
        }
    }
    let listing_phase = listing_start.elapsed();
    let outcome = mark::mark_live_blobs(conn, live, p)?;

    let mut orphans = Vec::new();
    let mut orphan_bytes = 0u64;
    let mut live_blobs = 0u64;
    let mut live_bytes = 0u64;
    let total_blobs = all.len() as u64;
    let total_bytes = all.values().sum();
    for (hex, bytes) in &all {
        if outcome.marked.contains(hex.as_str()) {
            live_blobs += 1;
            live_bytes += bytes;
        } else {
            orphans.push(hex.clone());
            orphan_bytes += bytes;
        }
    }
    orphans.sort();
    let dangling_refs =
        outcome.marked.iter().filter(|h| !all.contains_key(h.as_str())).count() as u64;

    let content_total = content_rows.len() as u64;
    let content_bytes = content_rows.iter().map(|(_, b)| b).sum();
    let mut content_orphans = Vec::new();
    let mut content_orphan_bytes = 0u64;
    for (key, bytes) in content_rows {
        if !outcome.content_keys.contains(&key) {
            content_orphan_bytes += bytes;
            content_orphans.push(key);
        }
    }
    content_orphans.sort();

    Ok(GcPlan {
        total_blobs,
        total_bytes,
        live_blobs,
        live_bytes,
        orphans,
        orphan_bytes,
        dangling_refs,
        content_total,
        content_bytes,
        content_orphans,
        content_orphan_bytes,
        root_error_rate: outcome.root_error_rate(),
        stats: outcome.stats,
        listing_phase,
    })
}

pub struct GcOutcome {
    pub plan: GcPlan,
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
    make_backup: bool,
    p: &crate::progress::Progress,
) -> Result<GcOutcome> {
    let mut conn = safety::open_write_gated(db_path)?;

    let sessions = headers::load_union(&conn)?;
    let live = headers::live_set(&sessions)?;
    if live.is_empty() {
        return Err(Error::EmptyLiveSet { action: "清理" });
    }

    let plan = plan(&conn, &live, p)?;
    apply_plan(&mut conn, db_path, plan, make_backup, true, p)
}

/// 按既有计划执行回收。`finalize=false` 时跳过 checkpoint 与物理收缩,
/// 由维护模式在会话结束统一收尾。
///
/// 计划的新鲜性由调用方保证: 独立路径在同一写连接上刚算出;
/// 维护模式下由排他锁保证库未被外部改动。
pub fn apply_plan(
    conn: &mut Connection,
    db_path: &Path,
    plan: GcPlan,
    make_backup: bool,
    finalize: bool,
    p: &crate::progress::Progress,
) -> Result<GcOutcome> {
    // 安全阀: 根解码失败率过高说明数据可能损坏,删除不可逆,拒绝。
    if plan.root_error_rate > ROOT_ERROR_RATE_LIMIT {
        return Err(Error::RootErrorRate {
            rate: plan.root_error_rate,
            limit: ROOT_ERROR_RATE_LIMIT,
        });
    }
    // mark 一个根都没解出来时删除等于全删,同样拒绝。
    if plan.stats.root_states == 0 && !plan.orphans.is_empty() {
        return Err(Error::NoDecodedRoots);
    }

    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count_before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    if plan.orphans.is_empty() && plan.content_orphans.is_empty() {
        return Ok(GcOutcome {
            plan,
            deleted_rows: 0,
            backup_path: None,
            page_size,
            page_count_before,
            page_count_after: page_count_before,
            shrink: None,
        });
    }

    let mut keys: Vec<String> =
        plan.orphans.iter().map(|h| format!("agentKv:blob:{h}")).collect();
    keys.extend(plan.content_orphans.iter().cloned());

    let (deleted_rows, backup_path) =
        sweep::remove_keys(conn, db_path, &keys, "gc", make_backup, p)?;

    let mut shrink = None;
    if finalize {
        p.stage("checkpoint", 0);
        safety::checkpoint_truncate(conn)?;
        shrink = Some(crate::vacuum::shrink_auto(conn, p)?);
        safety::checkpoint_truncate(conn)?;
        p.finish();
    }
    let page_count_after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

    Ok(GcOutcome {
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
    use crate::proto::agent::v1 as pb;
    use crate::types::hex_encode;
    use buffa::Message as _;

    fn temp_db(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ccc-gc-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn id(n: u8) -> Vec<u8> {
        vec![n; 16]
    }
    fn hexs(n: u8) -> String {
        hex_encode(&id(n))
    }

    /// 构建一个覆盖所有 mark 路径的库:
    ///
    /// composerData:live1 (hex 编码 state)
    ///   └─ turns[0] = blob 01 (Turn)
    ///        ├─ user_message = blob 02 (UserMessage)
    ///        └─ steps[0] = blob 03 (Step: read_tool content_blob_id → blob 04)
    ///   └─ summary_archives[0] = blob 05 (Archive) ← 官方 GC 的误删点
    ///        ├─ summarized_messages[0] = blob 06
    ///        └─ summary_message = blob 07
    /// bubbleId:live1:b1 (base64 编码 state) └─ todos[0] = blob 08
    /// agentKv:checkpoint:live1 = "0909…"(大写,测归一化)
    ///   └─ blob 09 (State) └─ todos[0] = blob 0a
    /// blob 99 = 无引用孤儿(唯一该删的)
    fn setup(db: &Path) {
        let conn = Connection::open(db).unwrap();
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
            "#,
        )
        .unwrap();

        let put_blob = |n: u8, bytes: &[u8]| {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES ('agentKv:blob:' || ?1, ?2)",
                rusqlite::params![hexs(n), bytes],
            )
            .unwrap();
        };

        let turn = pb::ConversationTurnStructure {
            turn: Some(pb::conversation_turn_structure::Turn::AgentConversationTurn(Box::new(
                pb::AgentConversationTurnStructure {
                    user_message: id(2),
                    steps: vec![id(3)],
                    ..Default::default()
                },
            ))),
        };
        put_blob(1, &turn.encode_to_vec());

        let um = pb::UserMessage { text: "hi".into(), ..Default::default() };
        put_blob(2, &um.encode_to_vec());

        // content key 引用: c1 在 blob 内容里(protobuf 字符串是裸 UTF-8),
        // c2 在 bubble JSON 明文里,c3 无引用(唯一该删的 content 行)
        let c1 = format!("composer.content.{}", "11".repeat(32));
        let c2 = format!("composer.content.{}", "22".repeat(32));
        let c3 = format!("composer.content.{}", "33".repeat(32));
        for key in [&c1, &c2, &c3] {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, 'file content')",
                [key.as_str()],
            )
            .unwrap();
        }

        let step = pb::ConversationStep {
            message: Some(pb::conversation_step::Message::ToolCall(Box::new(pb::ToolCall {
                tool: Some(pb::tool_call::Tool::ReadToolCall(Box::new(pb::ReadToolCall {
                    result: pb::ReadToolResult {
                        result: Some(pb::read_tool_result::Result::Success(Box::new(
                            pb::ReadToolSuccess {
                                output: Some(pb::read_tool_success::Output::ContentBlobId(id(4))),
                                ..Default::default()
                            },
                        ))),
                    }
                    .into(),
                    ..Default::default()
                }))),
                ..Default::default()
            }))),
        };
        put_blob(3, &step.encode_to_vec());
        put_blob(4, format!("edit result referencing {c1} inline").as_bytes());

        let archive = pb::ConversationSummaryArchive {
            summarized_messages: vec![id(6)],
            summary_message: id(7),
            ..Default::default()
        };
        put_blob(5, &archive.encode_to_vec());
        put_blob(6, b"archived message 1");
        put_blob(7, b"summary message");

        let bubble_state = pb::ConversationStateStructure { todos: vec![id(8)], ..Default::default() };
        put_blob(8, b"todo leaf");

        let ck_state = pb::ConversationStateStructure { todos: vec![id(0x0a)], ..Default::default() };
        put_blob(9, &ck_state.encode_to_vec());
        put_blob(0x0a, b"checkpoint todo leaf");

        put_blob(0x99, b"orphan leaf, must be deleted");

        let root_state = pb::ConversationStateStructure {
            turns: vec![id(1)],
            summary_archives: vec![id(5)],
            ..Default::default()
        };
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:live1', ?1)",
            [format!(
                r#"{{"conversationState":"{}"}}"#,
                hex_encode(&root_state.encode_to_vec())
            )],
        )
        .unwrap();

        use base64::Engine as _;
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:live1:b1', ?1)",
            [format!(
                r#"{{"conversationState":"~{}","toolFormerData":{{"beforeContentId":"{c2}"}}}}"#,
                base64::engine::general_purpose::STANDARD.encode(bubble_state.encode_to_vec())
            )],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('agentKv:checkpoint:live1', ?1)",
            [hexs(9).to_ascii_uppercase()],
        )
        .unwrap();
    }

    #[test]
    fn gc_keeps_archive_interior_and_deletes_only_orphan() {
        let db = temp_db("mark");
        setup(&db);

        let outcome = apply(&db, true, &crate::progress::Progress::new()).unwrap();

        assert_eq!(outcome.plan.orphans, vec![hexs(0x99)], "只有无引用的 99 是孤儿");
        assert_eq!(
            outcome.plan.content_orphans,
            vec![format!("composer.content.{}", "33".repeat(32))],
            "c1(blob 内引用)与 c2(JSON 明文引用)必须存活"
        );
        assert_eq!(outcome.deleted_rows, 2, "1 个孤儿 blob + 1 个孤儿 content");
        // 注: dangling_refs 不断言为 0——叶子 blob 可能被 try-all-types 策略
        // 偶然解码成功并吐出垃圾 id,这是保守化设计的预期噪音,只多标不误删。
        assert_eq!(outcome.plan.stats.root_states, 2, "composerData + bubble");
        assert_eq!(outcome.plan.stats.checkpoint_roots, 1);
        assert_eq!(outcome.plan.stats.root_decode_errors, 0);

        let conn = Connection::open(&db).unwrap();
        let count_blob = |n: u8| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'agentKv:blob:' || ?1",
                [hexs(n)],
                |r| r.get(0),
            )
            .unwrap()
        };
        // 官方 GC 会误删 06/07(archive 内部),我们必须保留
        for n in [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 0x0a] {
            assert_eq!(count_blob(n), 1, "blob {n:02x} 必须存活");
        }
        assert_eq!(count_blob(0x99), 0, "孤儿必须被删");
        let content_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cursorDiskKV
                 WHERE key >= 'composer.content.' AND key < 'composer.content/'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content_left, 2, "c1/c2 存活,c3 被删");

        let _ = std::fs::remove_file(outcome.backup_path.unwrap());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn gc_aborts_on_high_root_error_rate() {
        let db = temp_db("valve");
        setup(&db);
        // 把 composerData 的 conversationState 换成非法 hex,50% 根解码失败
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE cursorDiskKV SET value = '{\"conversationState\":\"zzzz\"}'
             WHERE key = 'composerData:live1'",
            [],
        )
        .unwrap();
        drop(conn);

        let err = match apply(&db, false, &crate::progress::Progress::new()) {
            Err(e) => e,
            Ok(_) => panic!("高失败率时应当中止"),
        };
        assert!(
            matches!(err, Error::RootErrorRate { .. }),
            "实际错误: {}",
            err.render()
        );
        let _ = std::fs::remove_file(&db);
    }
}
