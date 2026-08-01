//! 物理空间回收。
//!
//! 两种方式,取舍完全不同,选择权交给调用方/用户:
//!
//! - **增量收缩**(`incremental_vacuum`): 把文件尾部页搬进空闲槽,逐批进行,
//!   **批间随时可取消**,已回收部分保留。代价是纯随机 I/O——空闲页巨大时
//!   (如整会话删除后的数 GB)在慢文件系统上会以小时计。
//! - **全量重建**(`VACUUM`): 顺序读写重写整库,空闲页占比高时**快一个量级**;
//!   但需要约等于有效数据量的临时空间,且单事务**不可取消**(中断即白跑)。
//!
//! 收缩不是数据安全操作: 跳过它只是文件偏大,Cursor 自带的自动 GC
//! 也会以每天 2000 页的速度缓慢回收。因此所有路径都允许"不收缩"。

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::progress::Progress;
use crate::safety;

/// 收缩的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shrink {
    /// 无空闲页,无事可做。
    Skipped,
    /// 增量收缩完成。
    Done { pages: u64 },
    /// 用户取消增量收缩。已回收部分保留,文件一致,随时可续做。
    Cancelled { done: u64, remaining: u64 },
    /// 全量 VACUUM 完成。
    Vacuumed { freed_bytes: u64 },
    /// 库未开启 incremental auto_vacuum,增量方式不可用(仅强制增量时出现)。
    Unsupported,
    /// 连续一批无进展(异常情形,按完成处理但如实报告)。
    Stalled { remaining: u64 },
}

impl Shrink {
    /// 单行人类可读结论。
    pub fn describe(&self) -> String {
        match self {
            Shrink::Skipped => "没有空闲页,无需收缩".into(),
            Shrink::Done { pages } => format!("增量收缩完成,回收 {pages} 页"),
            Shrink::Cancelled { done, remaining } => {
                format!("收缩已取消: 已回收 {done} 页,剩余 {remaining} 页(可随时续做)")
            }
            Shrink::Vacuumed { freed_bytes } => {
                format!("全量 VACUUM 完成,文件缩小 {}", bytesize::ByteSize::b(*freed_bytes))
            }
            Shrink::Unsupported => "库未开启 incremental auto_vacuum,需全量 VACUUM".into(),
            Shrink::Stalled { remaining } => format!("收缩无进展,剩余 {remaining} 页"),
        }
    }
}

/// 收缩方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Incremental,
    Full,
}

/// 增量的单位空闲字节成本相对顺序重写单位有效字节成本的惩罚系数。
///
/// 2026-08 在 ext4/NVMe 上实测(L=48 MiB 有效,F=8/16/32/48 MiB 空闲):
/// c_rand ≈ 310–380 ns/B,c_seq ≈ 8–9 ns/B,比值 35–49 且随 F 略超线性;
/// 取上界 50 留余量——选错方向的代价不对称: 大空闲误走增量是小时级,
/// 小空闲误走 VACUUM 只是多花几十秒顺序重写。
const RANDOM_IO_PENALTY: u64 = 50;

/// 按成本模型自动选择: 增量成本 ∝ 空闲量×随机惩罚,VACUUM 成本 ∝ 有效数据量。
/// 空闲量小(相对有效数据)时增量胜在可取消、零临时空间;
/// 空闲量大时增量会以小时计,必须走顺序重建。
pub fn choose(conn: &Connection) -> Result<(Strategy, String)> {
    let (pages, free_bytes) = freelist(conn)?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let live_bytes = (page_count.max(0) as u64 * page_size.max(0) as u64).saturating_sub(free_bytes);
    let av: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;

    if av != 2 {
        return Ok((Strategy::Full, "库未开启 incremental auto_vacuum".into()));
    }
    if free_bytes.saturating_mul(RANDOM_IO_PENALTY) < live_bytes {
        Ok((
            Strategy::Incremental,
            format!(
                "空闲 {pages} 页 / {},相对有效数据量小,增量更划算(且可随时取消)",
                bytesize::ByteSize::b(free_bytes)
            ),
        ))
    } else {
        Ok((
            Strategy::Full,
            format!(
                "空闲 {} 已达有效数据({})的 1/{RANDOM_IO_PENALTY} 以上,\
                 增量的随机 I/O 会以小时计,顺序重建快得多",
                bytesize::ByteSize::b(free_bytes),
                bytesize::ByteSize::b(live_bytes),
            ),
        ))
    }
}

/// 自动收缩: 按 [`choose`] 选择方式执行。
pub fn shrink_auto(conn: &Connection, p: &Progress) -> Result<Shrink> {
    if freelist(conn)?.0 == 0 {
        return Ok(Shrink::Skipped);
    }
    match choose(conn)?.0 {
        Strategy::Incremental => shrink_incremental(conn, p),
        Strategy::Full => Ok(Shrink::Vacuumed { freed_bytes: vacuum_full(conn, p)? }),
    }
}

/// 空闲页概况: (页数, 字节数)。
pub fn freelist(conn: &Connection) -> Result<(u64, u64)> {
    let pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok((pages.max(0) as u64, (pages.max(0) * page_size.max(0)) as u64))
}

/// 增量收缩(可取消)。批间做 PASSIVE checkpoint 防 WAL 膨胀——
/// 页搬移全部经 WAL,不控制的话 WAL 会涨到与回收量同级,
/// 结束时的 TRUNCATE checkpoint 又要整体回写一遍,二次放大。
pub fn shrink_incremental(conn: &Connection, p: &Progress) -> Result<Shrink> {
    let av: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if av != 2 {
        return Ok(Shrink::Unsupported);
    }
    let start: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    p.stage("物理收缩(空闲页,可取消)", start.max(0) as u64);

    const BATCH_PAGES: u32 = 2000;
    const CHECKPOINT_EVERY: u32 = 25; // 每 ~200 MiB 页搬移回写一次
    let mut prev = start;
    let mut batches = 0u32;
    while prev > 0 {
        if p.is_cancelled() {
            return Ok(Shrink::Cancelled {
                done: (start - prev).max(0) as u64,
                remaining: prev.max(0) as u64,
            });
        }
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({BATCH_PAGES})"))?;
        batches += 1;
        if batches.is_multiple_of(CHECKPOINT_EVERY) {
            let _ = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        }
        let cur: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        p.set_done((start - cur).max(0) as u64);
        if cur >= prev {
            return Ok(Shrink::Stalled { remaining: cur.max(0) as u64 });
        }
        prev = cur;
    }
    Ok(Shrink::Done { pages: start.max(0) as u64 })
}

/// 全量 VACUUM: 顺序重建整库。空闲页占比高时远快于增量,
/// 但单事务不可取消,且需约等于有效数据量的临时磁盘空间。
/// 返回重建后缩小的字节数。
pub fn vacuum_full(conn: &Connection, p: &Progress) -> Result<u64> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    p.stage("全量 VACUUM(顺序重建,不可取消)", 0);
    conn.execute_batch("VACUUM").context("VACUUM 失败")?;
    let after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    Ok(((before - after).max(0) * page_size.max(0)) as u64)
}

/// CLI `vacuum` 子命令: 过安全门后回收空闲页,默认自动选择方式。
/// 顺带处理杀进程残留的大 WAL(写模式打开即自动恢复,结束时 TRUNCATE)。
pub fn vacuum_cmd(db_path: &Path, force: Option<Strategy>, p: &Progress) -> Result<String> {
    let conn = safety::open_write_gated(db_path)?;
    p.stage("checkpoint", 0);
    safety::checkpoint_truncate(&conn)?;

    let outcome = match force {
        Some(Strategy::Full) => Shrink::Vacuumed { freed_bytes: vacuum_full(&conn, p)? },
        Some(Strategy::Incremental) => shrink_incremental(&conn, p)?,
        None => {
            let (strategy, reason) = choose(&conn)?;
            eprintln!("自动选择: {strategy:?} —— {reason}");
            shrink_auto(&conn, p)?
        }
    };
    safety::checkpoint_truncate(&conn)?;
    p.finish();
    Ok(format!("{}。", outcome.describe()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_garbage(name: &str) -> (std::path::PathBuf, Connection) {
        let path = std::env::temp_dir()
            .join(format!("ccc-vac-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        for i in 0..2000 {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, zeroblob(4096))",
                [format!("k{i:06}")],
            )
            .unwrap();
        }
        conn.execute("DELETE FROM cursorDiskKV", []).unwrap();
        (path, conn)
    }

    /// 成本模型: 空闲量小走增量,空闲量大(或不支持增量)走全量。
    #[test]
    fn strategy_follows_cost_model() {
        let (path, conn) = db_with_garbage("strategy");
        // fixture: 全部数据被删,空闲量 ≫ 有效数据 → 全量
        let (s, _) = choose(&conn).unwrap();
        assert_eq!(s, Strategy::Full);

        // 回收后再造少量空闲(相对有效数据 < 1/10)→ 增量
        for i in 0..3000 {
            conn.execute("INSERT INTO cursorDiskKV VALUES (?1, zeroblob(4096))", [format!("k{i}")])
                .unwrap();
        }
        conn.execute("DELETE FROM cursorDiskKV WHERE key = 'k0'", []).unwrap();
        let (s, _) = choose(&conn).unwrap();
        assert_eq!(s, Strategy::Incremental);

        // 自动路径执行后空闲归零
        let p = Progress::new();
        assert!(!matches!(shrink_auto(&conn, &p).unwrap(), Shrink::Unsupported));
        assert_eq!(freelist(&conn).unwrap().0, 0);
        assert_eq!(shrink_auto(&conn, &p).unwrap(), Shrink::Skipped);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn incremental_shrink_is_cancellable_and_resumable() {
        let (path, conn) = db_with_garbage("cancel");
        let (pages, _) = freelist(&conn).unwrap();
        assert!(pages > 100, "fixture 应产生可观空闲页,实际 {pages}");

        // 预取消: 一页都不动,直接返回 Cancelled
        let p = Progress::new();
        p.cancel();
        let r = shrink_incremental(&conn, &p).unwrap();
        assert!(matches!(r, Shrink::Cancelled { done: 0, .. }), "实际: {r:?}");

        // 续做: 全部回收
        let p2 = Progress::new();
        let r2 = shrink_incremental(&conn, &p2).unwrap();
        assert!(matches!(r2, Shrink::Done { .. }), "实际: {r2:?}");
        assert_eq!(freelist(&conn).unwrap().0, 0);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
