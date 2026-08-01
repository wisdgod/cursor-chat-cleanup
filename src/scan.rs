//! cursorDiskKV 的 key 归属扫描与孤儿统计(只读 key 不读 value 的快速路径)。
//!
//! 归属规则(逆向自官方 key 编码): 除 `agentKv:blob:`、`composer.content.`
//! 和 `ai_*` 之外,key 的第二段(`agentKv:checkpoint`/`bubbleCheckpoint` 是第三段)
//! 就是 `composerId`,这是判定孤儿的唯一依据。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

/// key 的归属判定结果。
pub enum Attribution<'a> {
    /// 归属某个 composer 的行: (统计口径前缀, composerId)
    Owned(&'static str, &'a str),
    /// `agentKv:blob:` 内容寻址大对象,归属需要 mark 才知道。
    Blob,
    /// `composer.content.<sha256>` 内容寻址,不属于任何 composer。
    Content,
    /// 与聊天无关(`ai_*`、迁移锁等)。
    Other,
}

pub fn attribute(key: &str) -> Attribution<'_> {
    fn second_seg(rest: &str) -> &str {
        rest.split(':').next().unwrap_or(rest)
    }
    if let Some(rest) = key.strip_prefix("composerData:") {
        return Attribution::Owned("composerData", rest);
    }
    if let Some(rest) = key.strip_prefix("bubbleId:") {
        return Attribution::Owned("bubbleId", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("checkpointId:") {
        return Attribution::Owned("checkpointId", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("codeBlockDiff:") {
        return Attribution::Owned("codeBlockDiff", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("codeBlockPartialInlineDiffFates:") {
        return Attribution::Owned("codeBlockPartialInlineDiffFates", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("ofsContent:") {
        return Attribution::Owned("ofsContent", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("composerVirtualRowHeights:") {
        return Attribution::Owned("composerVirtualRowHeights", second_seg(rest));
    }
    if let Some(rest) = key.strip_prefix("agentKv:") {
        if rest.starts_with("blob:") {
            return Attribution::Blob;
        }
        if let Some(rest) = rest.strip_prefix("checkpoint:") {
            // 官方对该前缀无尾部冒号,cid 就是剩余整段。
            return Attribution::Owned("agentKv:checkpoint", rest);
        }
        if let Some(rest) = rest.strip_prefix("bubbleCheckpoint:") {
            return Attribution::Owned("agentKv:bubbleCheckpoint", second_seg(rest));
        }
        return Attribution::Other; // agentKv:artifact: 等
    }
    if key.starts_with("composer.content.") {
        return Attribution::Content;
    }
    Attribution::Other
}

#[derive(Debug, Default, Clone)]
pub struct PrefixStat {
    pub rows: u64,
    pub bytes: u64,
    pub orphan_rows: u64,
    pub orphan_bytes: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ComposerStat {
    pub rows: u64,
    pub bytes: u64,
    /// 该会话按前缀的构成(详情视图用;orphan 字段在此不使用)。
    pub per_prefix: BTreeMap<&'static str, PrefixStat>,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub total_rows: u64,
    pub per_prefix: BTreeMap<&'static str, PrefixStat>,
    /// 每个 composer 拥有的行数与字节数(含孤儿 composer;bytes 仅 deep 模式有值)。
    pub per_composer: FxHashMap<String, ComposerStat>,
    pub orphan_composers: BTreeSet<String>,
    pub blob: PrefixStat,
    pub content: PrefixStat,
    pub other: PrefixStat,
    /// 存活 composer 中没有 composerData 行的(幽灵 header)。
    pub ghost_headers: u64,
    /// 存活会话前缀下 value 为 NULL 的墓碑行(官方 set-NULL 删除 bug 的残留,
    /// sweep 会连同孤儿一起清)。孤儿的墓碑已计入孤儿行数,此处只计存活的。
    pub live_tombstone_rows: u64,
    /// 清扫应删的 key 全集(孤儿行 + 存活会话的墓碑行)。
    /// 仅当 `collect_condemned` 时填充——维护模式据此实现零重扫清扫。
    pub condemned_keys: Vec<String>,
}

/// 全表扫 key,按前缀与 composer 归属统计,并以 `live` 集合判定孤儿。
///
/// `deep` 模式额外读 `octet_length(value)` 统计字节数;`octet_length` 尽可能
/// 从记录头取长度而不加载 value 本体,比 `length()` 快。
pub fn scan_keys(
    conn: &Connection,
    live: &crate::types::LiveSet,
    deep: bool,
    collect_condemned: bool,
    p: &crate::progress::Progress,
) -> Result<ScanResult> {
    p.stage(
        if deep { "深度扫描" } else { "扫描 key" },
        crate::db::kv_row_count(conn)?,
    );
    let mut r = ScanResult::default();
    let mut has_composer_data: FxHashSet<String> = FxHashSet::default();

    // `value IS NULL` 只读记录头,快速模式也担得起
    let sql = if deep {
        "SELECT key, COALESCE(octet_length(value), 0), value IS NULL FROM cursorDiskKV"
    } else {
        "SELECT key, 0, value IS NULL FROM cursorDiskKV"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        r.total_rows += 1;
        if r.total_rows.is_multiple_of(1024) {
            p.set_done(r.total_rows);
            p.check()?;
        }
        let bytes = row.get::<_, i64>(1)?.max(0) as u64;
        // key 列没有 NOT NULL 约束,实测存在 NULL key 的行;
        // 连同非 UTF-8 的异常 key 一起归入"其它"。
        let key = match row.get_ref(0)? {
            rusqlite::types::ValueRef::Text(raw) => std::str::from_utf8(raw).ok(),
            _ => None,
        };
        let Some(key) = key else {
            r.other.rows += 1;
            r.other.bytes += bytes;
            continue;
        };
        match attribute(key) {
            Attribution::Owned(prefix, cid) => {
                let stat = r.per_prefix.entry(prefix).or_default();
                stat.rows += 1;
                stat.bytes += bytes;
                let is_null: bool = row.get(2)?;
                let is_live = live.contains(cid);
                if is_live && is_null {
                    r.live_tombstone_rows += 1;
                }
                // 与 sweep::plan 同一判据: 孤儿行,或存活会话下的 NULL 墓碑行
                if collect_condemned && (!is_live || is_null) {
                    r.condemned_keys.push(key.to_owned());
                }
                if !live.contains(cid) {
                    stat.orphan_rows += 1;
                    stat.orphan_bytes += bytes;
                    // 只在首见时分配
                    if !r.orphan_composers.contains(cid) {
                        r.orphan_composers.insert(cid.to_owned());
                    }
                }
                if prefix == "composerData" && !has_composer_data.contains(cid) {
                    has_composer_data.insert(cid.to_owned());
                }
                if let Some(c) = r.per_composer.get_mut(cid) {
                    c.rows += 1;
                    c.bytes += bytes;
                    let p = c.per_prefix.entry(prefix).or_default();
                    p.rows += 1;
                    p.bytes += bytes;
                } else {
                    let mut c = ComposerStat { rows: 1, bytes, per_prefix: BTreeMap::new() };
                    c.per_prefix.insert(prefix, PrefixStat { rows: 1, bytes, ..Default::default() });
                    r.per_composer.insert(cid.to_owned(), c);
                }
            }
            Attribution::Blob => {
                r.blob.rows += 1;
                r.blob.bytes += bytes;
            }
            Attribution::Content => {
                r.content.rows += 1;
                r.content.bytes += bytes;
            }
            Attribution::Other => {
                r.other.rows += 1;
                r.other.bytes += bytes;
            }
        }
    }

    r.ghost_headers = live
        .iter()
        .filter(|cid| !has_composer_data.contains(cid.as_str()))
        .count() as u64;
    p.finish();
    Ok(r)
}
