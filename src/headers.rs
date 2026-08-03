//! 会话列表的两套权威来源与并集。
//!
//! Cursor 由 feature gate 决定用 `composerHeaders` 表(新)还是
//! `ItemTable['composer.composerHeaders']` blob(旧),两边都可能比另一边新,
//! 必须取并集,否则会把只存在于另一套里的会话误判为孤儿。

use rusqlite::Connection;
use rustc_hash::FxHashMap;
use serde_json::Value;

use crate::error::{Ctx as _, Error, Result};

/// 存活集合(两套来源并集,再排除孤儿子代理)。任何 id 解析失败都整体报错:
/// 静默跳过会把该会话的全部行错误地判为孤儿,是不可接受的方向。
///
/// 孤儿子代理(父链断裂的真子代理,见 [`crate::lineage`])不算存活:
/// 其数据行由此自然成为孤儿行,清扫/GC 全链路无需特判。
pub fn live_set(sessions: &[SessionHeader]) -> Result<crate::types::LiveSet> {
    let lineage = crate::lineage::Lineage::build(sessions);
    let ids = sessions
        .iter()
        .filter(|s| !lineage.is_dangling(&s.composer_id))
        .map(|s| {
            crate::types::ComposerId::parse(&s.composer_id).map_err(|source| {
                Error::BadHeaderId { raw: s.composer_id.clone(), source }
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(crate::types::LiveSet::from_ids(ids))
}

#[derive(Debug, Clone)]
pub struct SessionHeader {
    pub composer_id: String,
    pub name: Option<String>,
    pub created_at: Option<i64>,
    pub last_updated_at: Option<i64>,
    /// `lastUpdatedAt ?? createdAt ?? 0`,官方列表的排序键。
    pub recency: i64,
    pub is_archived: bool,
    pub is_subagent: bool,
    /// Best-of-N 并行子会话(官方 isSubagent 判定明确排除它,单列)。
    pub is_best_of_n: bool,
    pub workspace_id: Option<String>,
    /// header JSON `workspaceIdentifier.uri` 里的项目路径(优先 `external`,回落 `path`)。
    /// 多数会话由此直接得到人类可读的 workspace 归属,无需查 workspace.json。
    pub workspace_folder: Option<String>,
    pub parent_composer_id: Option<String>,
    /// Best-of-N 父方持有的子会话 id 列表(`subComposerIds`)。
    pub sub_composer_ids: Vec<String>,
    pub in_header_table: bool,
    pub in_legacy_blob: bool,
}

/// 从 header JSON 提取 `workspaceIdentifier.uri` 的项目路径。
/// `external` 是完整 URI(如 `vscode-remote://<host>/home/...`),
/// 缺失时回落到 `path`(纯路径段)。
fn workspace_folder_of(header: &Value) -> Option<String> {
    let uri = header.pointer("/workspaceIdentifier/uri")?;
    uri.get("external")
        .and_then(Value::as_str)
        .or_else(|| uri.get("path").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Best-of-N 父方的子会话 id 列表(缺失时为空)。
fn sub_composer_ids_of(header: &Value) -> Vec<String> {
    header
        .get("subComposerIds")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// 读两套来源并按 `composerId` 归并。
pub fn load_union(conn: &Connection) -> Result<Vec<SessionHeader>> {
    let mut map: FxHashMap<String, SessionHeader> = FxHashMap::default();

    for h in load_header_table(conn)? {
        map.insert(h.composer_id.clone(), h);
    }
    for h in load_legacy_blob(conn)? {
        map.entry(h.composer_id.clone())
            .and_modify(|e| e.in_legacy_blob = true)
            .or_insert(h);
    }

    let mut list: Vec<_> = map.into_values().collect();
    list.sort_by_key(|h| std::cmp::Reverse(h.recency));
    Ok(list)
}

fn load_header_table(conn: &Connection) -> Result<Vec<SessionHeader>> {
    // 表可能不存在(未迁移的旧安装)。
    let table_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='composerHeaders')",
            [],
            |r| r.get(0),
        )
        .ctx("检查 composerHeaders 表失败")?;
    if !table_exists {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT composerId, workspaceId, createdAt, lastUpdatedAt,
                isArchived, isSubagent, recency, value
         FROM composerHeaders",
    )?;
    let rows = stmt.query_map([], |row| {
        let composer_id: String = row.get(0)?;
        let workspace_id: Option<String> = row.get(1)?;
        let created_at: Option<i64> = row.get(2)?;
        let last_updated_at: Option<i64> = row.get(3)?;
        let is_archived: Option<i64> = row.get(4)?;
        let is_subagent: Option<i64> = row.get(5)?;
        let recency: Option<i64> = row.get(6)?;
        let value: Option<String> = row.get(7)?;
        Ok((
            composer_id,
            workspace_id,
            created_at,
            last_updated_at,
            is_archived,
            is_subagent,
            recency,
            value,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (composer_id, workspace_id, created_at, last_updated_at, is_archived, is_subagent, recency, value) =
            row.ctx("读 composerHeaders 行失败")?;
        let json: Option<Value> = value.and_then(|v| serde_json::from_str(&v).ok());
        let name = json
            .as_ref()
            .and_then(|j| j.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let parent_composer_id = json
            .as_ref()
            .and_then(|j| j.pointer("/subagentInfo/parentComposerId"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let workspace_folder = json.as_ref().and_then(workspace_folder_of);
        let is_best_of_n = json
            .as_ref()
            .and_then(|j| j.get("isBestOfNSubcomposer"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let sub_composer_ids =
            json.as_ref().map(sub_composer_ids_of).unwrap_or_default();
        out.push(SessionHeader {
            recency: recency.or(last_updated_at).or(created_at).unwrap_or(0),
            composer_id,
            name,
            created_at,
            last_updated_at,
            is_archived: is_archived.unwrap_or(0) != 0,
            is_subagent: is_subagent.unwrap_or(0) != 0,
            is_best_of_n,
            workspace_id,
            workspace_folder,
            parent_composer_id,
            sub_composer_ids,
            in_header_table: true,
            in_legacy_blob: false,
        });
    }
    Ok(out)
}

fn load_legacy_blob(conn: &Connection) -> Result<Vec<SessionHeader>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'composer.composerHeaders'",
            [],
            |row| {
                // value 列声明为 BLOB,但这里实际存 TEXT;两种都兼容。
                let vr = row.get_ref(0)?;
                Ok(match vr {
                    rusqlite::types::ValueRef::Text(t) => {
                        Some(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        Some(String::from_utf8_lossy(b).into_owned())
                    }
                    _ => None,
                })
            },
        )
        .unwrap_or(None);
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let json: Value = serde_json::from_str(&raw)
        .map_err(|source| Error::Json { what: "旧 header blob", source })?;
    let Some(all) = json.get("allComposers").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for h in all {
        // 官方反序列化的 zod 校验: type=="head" 且 composerId/createdAt 必须存在。
        if h.get("type").and_then(Value::as_str) != Some("head") {
            continue;
        }
        let Some(composer_id) = h.get("composerId").and_then(Value::as_str) else {
            continue;
        };
        let Some(created_at) = h.get("createdAt").and_then(Value::as_i64) else {
            continue;
        };
        let last_updated_at = h.get("lastUpdatedAt").and_then(Value::as_i64);
        let parent_composer_id = h
            .pointer("/subagentInfo/parentComposerId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let is_best_of_n = h
            .get("isBestOfNSubcomposer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 官方 isSubagent 判定。
        let is_subagent =
            (composer_id.starts_with("task-") || parent_composer_id.is_some()) && !is_best_of_n;
        out.push(SessionHeader {
            composer_id: composer_id.to_owned(),
            name: h.get("name").and_then(Value::as_str).map(str::to_owned),
            created_at: Some(created_at),
            last_updated_at,
            recency: last_updated_at.unwrap_or(created_at),
            is_archived: h.get("isArchived").and_then(Value::as_bool).unwrap_or(false),
            is_subagent,
            is_best_of_n,
            workspace_id: h
                .pointer("/workspaceIdentifier/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            workspace_folder: workspace_folder_of(h),
            parent_composer_id,
            sub_composer_ids: sub_composer_ids_of(h),
            in_header_table: false,
            in_legacy_blob: true,
        });
    }
    Ok(out)
}
