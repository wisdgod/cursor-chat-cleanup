mod db;
mod delete;
mod gc;
mod headers;
mod maintenance;
mod mark;
mod progress;
mod proto;
mod safety;
mod scan;
mod sweep;
mod trim;
mod tui;
mod types;
mod vacuum;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bytesize::ByteSize;
use clap::{Parser, Subcommand};

/// 分析与清理 Cursor 本地聊天存储的工具。不带子命令时进入 TUI。
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// state.vscdb 路径(默认按平台自动定位)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 输出只读分析报告
    Report {
        /// 深度扫描,统计每个会话占用的字节数(较慢)
        #[arg(long)]
        deep: bool,
    },
    /// 清扫孤儿数据(默认 dry-run,只报告不删除)
    Sweep {
        /// 真正执行删除(要求 Cursor 已完全退出)
        #[arg(long)]
        apply: bool,
        /// 不生成删除前的 sidecar 备份(不推荐)
        #[arg(long)]
        no_backup: bool,
    },
    /// 删除指定会话(默认 dry-run;同步四处存储,自动快照可回滚)
    Delete {
        /// 会话 id(完整或至少 6 位前缀),可多个
        ids: Vec<String>,
        /// 真正执行删除(要求 Cursor 已完全退出)
        #[arg(long)]
        apply: bool,
        /// 不生成删除前的 sidecar 备份(不推荐)
        #[arg(long)]
        no_backup: bool,
    },
    /// blob 垃圾回收: mark & sweep `agentKv:blob:`(默认 dry-run)
    Gc {
        /// 真正执行删除(要求 Cursor 已完全退出)
        #[arg(long)]
        apply: bool,
        /// 不生成删除前的 sidecar 备份(不推荐)
        #[arg(long)]
        no_backup: bool,
    },
    /// 部分清理: 删会话的文件快照类数据,保留可读历史(默认 dry-run)
    Trim {
        /// 会话 id(完整或至少 6 位前缀),可多个
        ids: Vec<String>,
        /// 只删纯 UI 行高缓存(最保守;默认还包含 checkpointId/ofsContent)
        #[arg(long)]
        ui_only: bool,
        /// 真正执行删除(要求 Cursor 已完全退出)
        #[arg(long)]
        apply: bool,
        /// 不生成删除前的 sidecar 备份(不推荐)
        #[arg(long)]
        no_backup: bool,
    },
    /// 检查 state.vscdb.backup 与 .corrupted.* 残留
    Backup {
        /// 删除 state.vscdb.backup
        #[arg(long)]
        delete: bool,
        /// 删除 .corrupted.* 残留
        #[arg(long)]
        delete_corrupted: bool,
    },
    /// 回收空闲页(默认按空闲量自动选择增量/全量)
    Vacuum {
        /// 强制全量 VACUUM: 顺序重建;需临时空间,不可中断
        #[arg(long, conflicts_with = "incremental")]
        full: bool,
        /// 强制增量收缩: 可 Ctrl-C 中断,但空闲量大时极慢
        #[arg(long)]
        incremental: bool,
    },
    /// 从 sweep 生成的 sidecar 备份整体回灌
    Restore {
        /// sweep-backup-*.sqlite 文件路径
        file: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = match cli.db {
        Some(p) => p,
        None => db::locate_db()?,
    };

    match cli.cmd {
        None => tui::run(path),
        Some(Cmd::Report { deep }) => report(&path, deep),
        Some(Cmd::Sweep { apply, no_backup }) => sweep_cmd(&path, apply, no_backup),
        Some(Cmd::Gc { apply, no_backup }) => gc_cmd(&path, apply, no_backup),
        Some(Cmd::Delete { ids, apply, no_backup }) => delete_cmd(&path, &ids, apply, no_backup),
        Some(Cmd::Trim { ids, ui_only, apply, no_backup }) => {
            trim_cmd(&path, &ids, ui_only, apply, no_backup)
        }
        Some(Cmd::Backup { delete, delete_corrupted }) => backup_cmd(&path, delete, delete_corrupted),
        Some(Cmd::Vacuum { full, incremental }) => {
            let force = match (full, incremental) {
                (true, _) => Some(vacuum::Strategy::Full),
                (_, true) => Some(vacuum::Strategy::Incremental),
                _ => None,
            };
            let msg = with_ticker(|p| vacuum::vacuum_cmd(&path, force, p))?;
            println!("{msg}");
            Ok(())
        }
        Some(Cmd::Restore { file }) => {
            let out = sweep::restore(&path, &file)?;
            println!("已回灌 cursorDiskKV {} 行、composerHeaders {} 行。", out.kv_rows, out.header_rows);
            for (db, key) in &out.item_snapshots {
                println!("  已恢复 ItemTable 快照: {key} @ {db}");
            }
            Ok(())
        }
    }
}

/// 在 stderr 上行内刷新进度(仅 tty;管道输出时静默)。
fn with_ticker<T>(f: impl FnOnce(&progress::Progress) -> Result<T>) -> Result<T> {
    use std::io::{IsTerminal, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    let p = progress::Progress::new();
    let done = AtomicBool::new(false);
    let tty = std::io::stderr().is_terminal();
    std::thread::scope(|s| {
        if tty {
            s.spawn(|| {
                let mut printed = false;
                while !done.load(Ordering::Relaxed) {
                    if let Some(line) = p.render() {
                        eprint!("\r\x1b[K{line}");
                        let _ = std::io::stderr().flush();
                        printed = true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
                if printed {
                    eprint!("\r\x1b[K");
                    let _ = std::io::stderr().flush();
                }
            });
        }
        let r = f(&p);
        p.finish();
        done.store(true, Ordering::Relaxed);
        r
    })
}

fn sweep_cmd(path: &Path, apply: bool, no_backup: bool) -> Result<()> {
    println!("数据库: {}", path.display());

    if !apply {
        // dry-run: 只读分析连接,不加锁,Cursor 运行时也安全。
        let plan = with_ticker(|p| {
            db::with_analysis(path, |conn| {
                let sessions = headers::load_union(conn)?;
                let live = headers::live_set(&sessions)?;
                sweep::plan(conn, &live, true, p)
            })
        })?;
        print_plan(&plan);
        if !plan.keys.is_empty() {
            println!("\ndry-run 完成,没有删除任何数据。加 --apply 执行(需先完全退出 Cursor)。");
        }
        return Ok(());
    }

    println!("!! 执行期间请勿启动 Cursor(否则可能触发它的静默回滚机制) !!\n");
    let outcome = with_ticker(|p| sweep::apply(path, !no_backup, p))?;
    print_plan(&outcome.plan);
    println!("\n已删除 {} 行。", outcome.deleted_rows);
    if let Some(bk) = &outcome.backup_path {
        println!("删除前备份: {}", bk.display());
        println!("  如需回滚: cursor-chat-cleanup restore \"{}\"", bk.display());
    }
    print_shrink(outcome.shrink, outcome.page_size, outcome.page_count_before, outcome.page_count_after);

    // 收尾提醒: 陈旧 .backup 与 .corrupted.* 残留都与 Cursor 的静默回滚机制相关。
    let residue = safety::corrupted_residue(path);
    for (p, size) in &residue {
        println!("提示: 发现静默回滚残留 {} ({}),可用 backup --delete-corrupted 清理。", p.display(), ByteSize::b(*size));
    }
    let bk_status = db::with_analysis(path, |conn| safety::backup_status(path, conn))?;
    if let Some(bk) = bk_status {
        println!(
            "提示: 存在 {} ({}){},建议运行 backup 子命令评估是否删除。",
            bk.path.display(),
            ByteSize::b(bk.size),
            if bk.disabled == Some(true) { ",且备份开关已禁用(永不更新)" } else { "" },
        );
    }
    Ok(())
}

fn delete_cmd(path: &Path, ids: &[String], apply: bool, no_backup: bool) -> Result<()> {
    anyhow::ensure!(!ids.is_empty(), "请指定要删除的会话 id(report/TUI 里可以查到)");
    println!("数据库: {}", path.display());

    if !apply {
        let plan = with_ticker(|p| {
            db::with_analysis(path, |conn| {
                let sessions = headers::load_union(conn)?;
                let targets = delete::resolve_targets(&sessions, ids)?;
                delete::plan(conn, targets, p)
            })
        })?;
        print_delete_plan(&plan);
        println!("\ndry-run 完成,没有删除任何数据。加 --apply 执行(需先完全退出 Cursor)。");
        println!("(workspace 库的引用清理在 --apply 时进行)");
        return Ok(());
    }

    println!("!! 执行期间请勿启动 Cursor(否则可能触发它的静默回滚机制) !!\n");
    let outcome = with_ticker(|p| delete::apply(path, ids, !no_backup, p))?;
    print_delete_plan(&outcome.plan);
    println!(
        "\n已删除: cursorDiskKV {} 行,composerHeaders {} 行,旧 blob {},workspace 库改写 {} 个。",
        outcome.deleted_rows,
        outcome.deleted_header_rows,
        if outcome.legacy_blob_edited { "已同步" } else { "无需改动" },
        outcome.workspaces_edited.len(),
    );
    for ws in &outcome.workspaces_edited {
        println!("  改写: {}", ws.display());
    }
    if let Some(bk) = &outcome.backup_path {
        println!("删除前备份: {}", bk.display());
        println!("  如需回滚: cursor-chat-cleanup restore \"{}\"", bk.display());
    }
    println!("提示: 会话删除后其 blob 成为孤儿,跑一次 gc 可回收对应空间。");
    Ok(())
}

fn print_delete_plan(plan: &delete::DeletePlan) {
    println!("目标会话:");
    for t in &plan.targets {
        println!(
            "  {}  {}{}",
            t.composer_id.short(),
            t.name.as_deref().unwrap_or("(未命名)"),
            match (t.in_header_table, t.in_legacy_blob) {
                (true, true) => "  [表+旧blob]",
                (true, false) => "  [表]",
                (false, true) => "  [仅旧blob]",
                (false, false) => "",
            },
        );
    }
    println!(
        "将删除 cursorDiskKV {} 行 / {}:",
        plan.keys.len(),
        ByteSize::b(plan.total_bytes)
    );
    for (prefix, stat) in &plan.per_prefix {
        println!("  {prefix:<36} {:>8} 行  {:>10}", stat.rows, ByteSize::b(stat.bytes).to_string());
    }
}

fn trim_cmd(path: &Path, ids: &[String], ui_only: bool, apply: bool, no_backup: bool) -> Result<()> {
    anyhow::ensure!(!ids.is_empty(), "请指定要修剪的会话 id(report/TUI 里可以查到)");
    println!("数据库: {}", path.display());

    if !apply {
        let plan = with_ticker(|p| {
            db::with_analysis(path, |conn| {
                let sessions = headers::load_union(conn)?;
                let targets = delete::resolve_targets(&sessions, ids)?;
                trim::plan(conn, targets, ui_only, p)
            })
        })?;
        print_trim_plan(&plan);
        println!("\ndry-run 完成,没有删除任何数据。加 --apply 执行(需先完全退出 Cursor)。");
        return Ok(());
    }

    println!("!! 执行期间请勿启动 Cursor(否则可能触发它的静默回滚机制) !!\n");
    let outcome = with_ticker(|p| trim::apply(path, ids, ui_only, !no_backup, p))?;
    print_trim_plan(&outcome.plan);
    println!("\n已删除 {} 行,会话正文与列表不受影响。", outcome.deleted_rows);
    if let Some(bk) = &outcome.backup_path {
        println!("删除前备份: {}", bk.display());
        println!("  如需回滚: cursor-chat-cleanup restore \"{}\"", bk.display());
    }
    print_shrink(outcome.shrink, outcome.page_size, outcome.page_count_before, outcome.page_count_after);
    Ok(())
}

/// 统一的收缩结论输出(sweep / trim / gc 共用)。
fn print_shrink(
    shrink: Option<vacuum::Shrink>,
    page_size: i64,
    before: i64,
    after: i64,
) {
    let Some(shrink) = shrink else {
        println!("本次未做物理收缩;需要时运行 `vacuum` 子命令。");
        return;
    };
    let freed = (before - after).max(0) as u64 * page_size.max(0) as u64;
    match shrink {
        vacuum::Shrink::Done { .. } => {
            println!("物理收缩完成,文件缩小 {}。", ByteSize::b(freed))
        }
        vacuum::Shrink::Unsupported => println!(
            "库未开启 incremental auto_vacuum,空间需 `vacuum --full` 回收(数据已逻辑删除)。"
        ),
        other => println!("{}。", other.describe()),
    }
}

fn print_trim_plan(plan: &trim::TrimPlan) {
    println!("目标会话:");
    for t in &plan.targets {
        println!("  {}  {}", t.composer_id.short(), t.name.as_deref().unwrap_or("(未命名)"));
    }
    if plan.keys.is_empty() {
        println!("没有可修剪的数据。");
        return;
    }
    println!(
        "将删除快照数据 {} 行 / {}(正文保留,影响仅限\"恢复到检查点\"):",
        plan.keys.len(),
        ByteSize::b(plan.total_bytes)
    );
    for (prefix, stat) in &plan.per_prefix {
        println!("  {prefix:<36} {:>8} 行  {:>10}", stat.rows, ByteSize::b(stat.bytes).to_string());
    }
}

fn gc_cmd(path: &Path, apply: bool, no_backup: bool) -> Result<()> {
    println!("数据库: {}", path.display());

    if !apply {
        let plan = with_ticker(|p| {
            db::with_analysis(path, |conn| {
                let sessions = headers::load_union(conn)?;
                let live = headers::live_set(&sessions)?;
                gc::plan(conn, &live, p)
            })
        })?;
        print_gc_plan(&plan);
        if !plan.orphans.is_empty() {
            println!("\ndry-run 完成,没有删除任何数据。加 --apply 执行(需先完全退出 Cursor)。");
            println!("提示: 建议先跑 sweep 清掉孤儿会话行,再跑 gc,一次性回收全部空间。");
        }
        return Ok(());
    }

    println!("!! 执行期间请勿启动 Cursor(否则可能触发它的静默回滚机制) !!\n");
    let outcome = with_ticker(|p| gc::apply(path, !no_backup, p))?;
    print_gc_plan(&outcome.plan);
    println!("\n已删除 {} 个孤儿 blob。", outcome.deleted_rows);
    if let Some(bk) = &outcome.backup_path {
        println!("删除前备份: {}", bk.display());
        println!("  如需回滚: cursor-chat-cleanup restore \"{}\"", bk.display());
    }
    print_shrink(outcome.shrink, outcome.page_size, outcome.page_count_before, outcome.page_count_after);
    Ok(())
}

fn print_gc_plan(plan: &gc::GcPlan) {
    println!(
        "blob 总量: {} 个 / {}",
        plan.total_blobs,
        ByteSize::b(plan.total_bytes)
    );
    println!(
        "  存活(被引用): {} 个 / {}",
        plan.live_blobs,
        ByteSize::b(plan.live_bytes)
    );
    println!(
        "  孤儿(可回收): {} 个 / {}",
        plan.orphans.len(),
        ByteSize::b(plan.orphan_bytes)
    );
    println!(
        "composer.content: {} 个 / {},其中孤儿(可回收) {} 个 / {}",
        plan.content_total,
        ByteSize::b(plan.content_bytes),
        plan.content_orphans.len(),
        ByteSize::b(plan.content_orphan_bytes),
    );
    let s = &plan.stats;
    println!(
        "mark 统计: 根行 {} / 解出 state {} / 根解码失败 {}({:.2}%) / checkpoint 根 {} / cloudAgent 根 {} / 展开 blob {}",
        s.root_rows,
        s.root_states,
        s.root_decode_errors,
        plan.root_error_rate * 100.0,
        s.checkpoint_roots,
        s.cloud_agent_roots,
        s.expanded_blobs,
    );
    println!(
        "阶段耗时: blob 清单 {:.2}s | 根解码 {:.2}s | 指针根 {:.2}s | 展开 {:.2}s",
        plan.listing_phase.as_secs_f64(),
        s.root_phase.as_secs_f64(),
        s.pointer_phase.as_secs_f64(),
        s.expand_phase.as_secs_f64(),
    );
    if plan.dangling_refs > 0 {
        println!(
            "注意: {} 个被引用的 blob 已不存在(悬空引用)。可能是官方 GC 历史误删的痕迹(不可恢复),\
             也可能只是叶子数据被保守解码策略偶然误读,后者无害。",
            plan.dangling_refs
        );
    }
    if s.missing_blobs > 0 {
        println!("  (其中展开阶段遇到 {} 个缺失 blob)", s.missing_blobs);
    }
}

fn print_plan(plan: &sweep::SweepPlan) {
    if plan.keys.is_empty() {
        println!("没有可清扫的孤儿数据。");
        return;
    }
    if plan.total_bytes > 0 {
        println!(
            "可清扫: {} 行 / {},涉及 {} 个孤儿 composer,其中墓碑行 {}",
            plan.keys.len(),
            ByteSize::b(plan.total_bytes),
            plan.orphan_composers,
            plan.tombstone_rows,
        );
    } else {
        println!(
            "可清扫: {} 行,涉及 {} 个孤儿 composer,其中墓碑行 {}",
            plan.keys.len(),
            plan.orphan_composers,
            plan.tombstone_rows,
        );
    }
    for (prefix, stat) in &plan.per_prefix {
        println!("  {prefix:<36} {:>8} 行  {:>10}", stat.rows, ByteSize::b(stat.bytes).to_string());
    }
}

fn backup_cmd(path: &Path, delete: bool, delete_corrupted: bool) -> Result<()> {
    let status = db::with_analysis(path, |conn| safety::backup_status(path, conn))?;
    match status {
        None => println!("state.vscdb.backup 不存在。"),
        Some(bk) => {
            println!("{}: {}", bk.path.display(), ByteSize::b(bk.size));
            match bk.stale_for {
                Some(d) => println!("  比主库旧 {:.1} 小时", d.as_secs_f64() / 3600.0),
                None => println!("  无法比较主库与备份的修改时间"),
            }
            match bk.disabled {
                Some(true) => println!("  备份开关已禁用(disableSqliteStorageBackup=true): 此文件永不更新,是纯死重量,删除即净赚空间。"),
                Some(false) | None => println!("  备份开关未禁用: 文件会在 Cursor 退出时被覆盖更新,删除后下次退出会重新生成。"),
            }
            if delete {
                std::fs::remove_file(&bk.path)
                    .with_context(|| format!("删除失败: {}", bk.path.display()))?;
                println!("  已删除,释放 {}。", ByteSize::b(bk.size));
            } else {
                println!("  (加 --delete 删除)");
            }
        }
    }

    let residue = safety::corrupted_residue(path);
    if residue.is_empty() {
        println!(".corrupted.* 残留: 无。");
    } else {
        println!(".corrupted.* 残留(静默回滚发生过的信号,官方永不清理):");
        for (p, size) in &residue {
            println!("  {} ({})", p.display(), ByteSize::b(*size));
            if delete_corrupted {
                std::fs::remove_file(p).with_context(|| format!("删除失败: {}", p.display()))?;
                println!("    已删除。");
            }
        }
        if !delete_corrupted {
            println!("  (加 --delete-corrupted 删除)");
        }
    }
    Ok(())
}

fn report(path: &Path, deep: bool) -> Result<()> {
    println!("数据库: {}", path.display());
    let (sessions, scan) = with_ticker(|p| {
        db::with_analysis(path, |conn| {
            let sessions = headers::load_union(conn)?;
            let live = headers::live_set(&sessions)?;
            let scan = scan::scan_keys(conn, &live, deep, false, p)?;
            Ok((sessions, scan))
        })
    })?;

    let in_table = sessions.iter().filter(|s| s.in_header_table).count();
    let in_blob = sessions.iter().filter(|s| s.in_legacy_blob).count();
    let table_only = sessions.iter().filter(|s| s.in_header_table && !s.in_legacy_blob).count();
    let blob_only = sessions.iter().filter(|s| !s.in_header_table && s.in_legacy_blob).count();

    println!("== 存活会话来源 ==");
    println!("  composerHeaders 表:     {in_table}");
    println!("  ItemTable 旧 blob:      {in_blob}");
    println!("  并集(存活判定):         {}", sessions.len());
    println!("  仅在表里 / 仅在 blob 里: {table_only} / {blob_only}");

    println!("\n== header/data 一致性 ==");
    let tombstones = sessions.iter().filter(|s| s.is_archived && s.name.is_none()).count();
    println!("  疑似墓碑(isArchived=1 且无 name): {tombstones}");
    println!("  幽灵 header(无对应 composerData): {}", scan.ghost_headers);

    println!("\n== 孤儿行统计 == (cursorDiskKV 共 {} 行)", scan.total_rows);
    let mut prefixes: Vec<_> = scan.per_prefix.iter().collect();
    prefixes.sort_by_key(|(_, stat)| std::cmp::Reverse(stat.rows));
    for (prefix, stat) in prefixes {
        let pct = if stat.rows > 0 { stat.orphan_rows as f64 / stat.rows as f64 * 100.0 } else { 0.0 };
        if deep {
            println!(
                "  {prefix:<36} {:>8} {:>8} {pct:>5.1}%  {:>10}(孤儿 {})",
                stat.rows,
                stat.orphan_rows,
                ByteSize::b(stat.bytes).to_string(),
                ByteSize::b(stat.orphan_bytes),
            );
        } else {
            println!("  {prefix:<36} {:>8} {:>8} {pct:>5.1}%", stat.rows, stat.orphan_rows);
        }
    }
    println!("  孤儿 composerId 数: {}", scan.orphan_composers.len());
    println!(
        "  agentKv:blob / composer.content / 其它: {} / {} / {} 行",
        scan.blob.rows, scan.content.rows, scan.other.rows
    );
    if deep {
        let owned_bytes: u64 = scan.per_prefix.values().map(|s| s.bytes).sum();
        println!(
            "  体积: 会话自有 {} + agentKv:blob {} + composer.content {} + 其它 {}",
            ByteSize::b(owned_bytes),
            ByteSize::b(scan.blob.bytes),
            ByteSize::b(scan.content.bytes),
            ByteSize::b(scan.other.bytes),
        );

        println!("\n== 会话体积 Top 15 ==");
        let mut by_size: Vec<_> = scan.per_composer.iter().collect();
        by_size.sort_by_key(|(_, s)| std::cmp::Reverse(s.bytes));
        let name_of: rustc_hash::FxHashMap<&str, &headers::SessionHeader> =
            sessions.iter().map(|s| (s.composer_id.as_str(), s)).collect();
        for (cid, stat) in by_size.iter().take(15) {
            let (name, live_mark) = match name_of.get(cid.as_str()) {
                Some(s) => (s.name.as_deref().unwrap_or("(未命名)"), ""),
                None => ("(孤儿)", "*"),
            };
            println!(
                "  {:>10}  {:>7} 行  {}{}  {}",
                ByteSize::b(stat.bytes).to_string(),
                stat.rows,
                live_mark,
                name,
                &cid[..8.min(cid.len())],
            );
        }
    }

    println!("\n最近 10 个会话:");
    for s in sessions.iter().take(10) {
        let when = jiff::Timestamp::from_millisecond(s.recency)
            .map(|t| {
                t.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|_| "?".into());
        println!(
            "  {when}  {}{}  {}",
            if s.is_archived { "[归档] " } else { "" },
            s.name.as_deref().unwrap_or("(未命名)"),
            &s.composer_id[..8.min(s.composer_id.len())],
        );
    }
    Ok(())
}
