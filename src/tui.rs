//! TUI 工作台: 三栏布局(工作区 | 会话 | 详情/子代理)+ 全部清理操作的
//! 预览-确认-执行闭环。
//!
//! 交互模型:
//! - 启动即渲染(headers 同步读,毫秒级),全表扫描在后台填充行数/体积;
//! - 左栏列工作区(重名已消歧),选中即过滤中栏;中栏只列主代理
//!   (用户手动 new 的会话)与孤儿数据;挂靠的子代理不论层级都在右栏树里,
//!   `h/l` 在三栏间移动焦点;
//! - 所有清理动作(delete/trim/sweep/gc)都是两段式: 后台跑 dry-run 预览 →
//!   弹确认层显示将删内容 → y 后后台执行(内部过写安全门并自动备份)→
//!   完成后刷新列表并重扫;删除会话必须连带其全部子代理;
//! - 任何时刻只允许一个前台动作(预览或执行),后台深扫可并行。

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::error::Result;
use bytesize::ByteSize;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};
use rustc_hash::FxHashSet;

use crate::lineage::{Attach, Lineage};
use crate::maintenance::Maintenance;
use crate::{db, delete, gc, headers, scan, sweep, trim};

#[derive(Clone)]
struct Entry {
    composer_id: String,
    name: Option<String>,
    recency: i64,
    is_archived: bool,
    /// 归属分类;None = 无 header 的孤儿数据行。
    attach: Option<Attach>,
    rows: u64,
    bytes: Option<u64>,
    /// 全部后代子代理数(挂在右栏树里,不占中栏行)。
    descendants: usize,
    /// header 里的 workspaceId。孤儿数据没有 header,不可归属,恒为 None。
    workspace_id: Option<String>,
    /// 解析后的工作区短标签(列表列与过滤用)。
    ws_label: Option<String>,
}

impl Entry {
    /// 是否有 header(可按会话操作;孤儿数据行只能走清扫)。
    fn has_header(&self) -> bool {
        self.attach.is_some()
    }

    /// 中栏可见: 主代理、保守保留的无归属子代理、孤儿子代理、孤儿数据。
    /// 挂靠的子代理在右栏树里显示。
    fn in_chat_list(&self) -> bool {
        !matches!(self.attach, Some(Attach::Attached))
    }

    fn is_dangling(&self) -> bool {
        self.attach == Some(Attach::Dangling)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Bytes,
    Rows,
    Recency,
    Name,
    Workspace,
}

impl SortKey {
    fn next(self) -> Self {
        match self {
            SortKey::Bytes => SortKey::Rows,
            SortKey::Rows => SortKey::Recency,
            SortKey::Recency => SortKey::Name,
            SortKey::Name => SortKey::Workspace,
            SortKey::Workspace => SortKey::Bytes,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SortKey::Bytes => "体积",
            SortKey::Rows => "行数",
            SortKey::Recency => "时间",
            SortKey::Name => "名称",
            SortKey::Workspace => "工作区",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    All,
    Orphans,
    Archived,
}

impl View {
    fn next(self) -> Self {
        match self {
            View::All => View::Orphans,
            View::Orphans => View::Archived,
            View::Archived => View::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            View::All => "全部",
            View::Orphans => "孤儿",
            View::Archived => "归档",
        }
    }
}

#[derive(Clone)]
enum Action {
    Delete(Vec<String>),
    Trim(Vec<String>),
    Sweep,
    Gc,
    /// 增量收缩空闲页(可随时取消,已回收部分保留)。
    Shrink,
    /// 全量 VACUUM 重建(快得多,但不可中断)。
    VacuumFull,
}

impl Action {
    fn verb(&self) -> &'static str {
        match self {
            Action::Delete(_) => "删除会话",
            Action::Trim(_) => "修剪快照",
            Action::Sweep => "清扫孤儿",
            Action::Gc => "blob GC",
            Action::Shrink => "增量收缩",
            Action::VacuumFull => "全量 VACUUM",
        }
    }

    /// 是否为可随时中断的操作(影响执行中能否响应取消键)。
    fn interruptible(&self) -> bool {
        matches!(self, Action::Shrink)
    }
}

/// 预览结果。维护模式下 gc 会把算好的计划一并带回,
/// 确认执行时直接复用,省掉第二次 mark(全库 mark 是分钟级操作)。
struct Preview {
    summary: String,
    gc_plan: Option<Box<gc::GcPlan>>,
}

/// 扫描类任务的代际标记。新任务开始时递增,过期结果一律丢弃——
/// 否则被取代的旧扫描回流时会覆盖新结果、并让进度显示来回跳。
type ScanGen = u64;

enum TaskMsg {
    Scan(ScanGen, Result<scan::ScanResult>),
    Preview(Action, Result<Preview>, Option<Box<Maintenance>>),
    Applied(Result<(String, ScanPatch)>, Option<Box<Maintenance>>),
    /// 维护会话已建立: 会话 + 锁内首次全量扫描的结果。
    Entered(
        ScanGen,
        Result<(Box<Maintenance>, Vec<headers::SessionHeader>, Box<scan::ScanResult>)>,
    ),
    /// 维护会话已释放,附带物理收缩的字节数。
    Released(Result<u64>),
}

/// 操作完成后对内存统计的增量修正,替代整库重扫(r 键可随时全量校准)。
enum ScanPatch {
    /// 会话被整体删除。
    RemoveComposers(Vec<String>),
    /// 会话的快照前缀被修剪。
    TrimPrefixes(Vec<String>),
    /// 全部孤儿 composer 的行被清扫。
    SweepOrphans,
    /// blob / content 命名空间被回收。
    Gc { blob_rows: u64, blob_bytes: u64, content_rows: u64, content_bytes: u64 },
    /// 逻辑数据未变(仅物理布局变化,如收缩)。
    None,
}

/// 输入焦点。
#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    Normal,
    Filter,
}

/// 前台操作生命周期。任何时刻恰处一态,
/// "预览未回就确认""执行中再触发"这类非法流程不可表达。
enum OpState {
    Idle,
    Previewing { action: Action, started: Instant },
    Confirming { action: Action, summary: String, gc_plan: Option<Box<gc::GcPlan>> },
    Applying { action: Action, started: Instant },
}

/// 维护模式: 持整库排他锁的长会话。
///
/// 锁在手 ⇒ 库不可能被外部修改 ⇒ 一次扫描建立的模型恒为精确,
/// 后续预览/执行全部零重扫,checkpoint 与物理收缩推迟到退出时做一次。
/// 会话对象在需要后台执行时整体借给工作线程,完成后送回(故有 Working 态)。
enum MaintState {
    Off,
    Entering { started: Instant },
    Held(Box<Maintenance>),
    /// 借给工作线程(预览或执行)中。
    Working,
    Leaving { started: Instant },
}

impl MaintState {
    /// 是否处于"锁在手或即将在手"的模式内(用于 UI 横幅与语义判定)。
    fn engaged(&self) -> bool {
        !matches!(self, MaintState::Off)
    }
}

/// 信息覆盖层(与操作生命周期独立的只读弹层)。
#[derive(Clone, Copy, PartialEq)]
enum InfoLayer {
    None,
    Help,
}

/// 三栏焦点。`h/l`(或 ←/→)在栏间移动,`j/k` 在焦点栏内移动。
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Workspaces,
    Chats,
    Detail,
}

/// 左栏(工作区)的一行。
struct WsRow {
    /// None = 全部(清除过滤);Some("") = 无归属;Some(id) = 具体工作区。
    id: Option<String>,
    label: String,
    sessions: u64,
    bytes: Option<u64>,
}

/// 右栏子代理树的一行(先序展开,depth 控制缩进)。
struct SubRow {
    composer_id: String,
    name: Option<String>,
    depth: usize,
    bytes: Option<u64>,
    is_dangling: bool,
}

/// 后台全量扫描状态。
enum ScanState {
    Running { started: Instant },
    Ready,
}

struct App {
    db_path: PathBuf,
    sessions: Vec<headers::SessionHeader>,
    /// 会话父子归属(sessions 变化时随 `refresh_model` 重建)。
    lineage: Lineage,
    entries: Vec<Entry>,
    /// 过滤/视图后的可见行(entries 下标)。
    visible: Vec<usize>,
    scan: scan::ScanResult,
    scan_state: ScanState,
    sort: SortKey,
    reverse: bool,
    view: View,
    filter: String,
    /// 工作区过滤: None = 不过滤;Some("") = 只看无归属;Some(id) = 只看该工作区。
    ws_filter: Option<String>,
    /// workspaceId → 标签/路径(由 headers 解析,会话列表刷新时重算)。
    workspaces: rustc_hash::FxHashMap<String, crate::workspace::WorkspaceInfo>,
    /// 三栏焦点。
    focus: Pane,
    /// 左栏行(entries 重建时重算)。
    ws_rows: Vec<WsRow>,
    ws_table: TableState,
    /// 右栏子代理树(跟随中栏光标,`sync_sub_rows` 重算)。
    sub_rows: Vec<SubRow>,
    sub_table: TableState,
    /// 右栏当前展示的会话 id(变化检测,避免每帧重建树)。
    sub_of: Option<String>,
    input: InputMode,
    table: TableState,
    selected_ids: FxHashSet<String>,
    op: OpState,
    info: InfoLayer,
    maint: MaintState,
    /// 已请求退出: 等手头的不可中断任务收尾后自动退出(见 `busy_reason`)。
    quit_requested: bool,
    status: Option<String>,
    /// 当前扫描任务的代号,用于丢弃被取代的旧任务结果。
    scan_gen: ScanGen,
    /// 当前扫描任务的进度。**每个任务独占一个实例**:
    /// 被取代的旧线程继续写它自己那份,不会污染界面。
    scan_progress: std::sync::Arc<crate::progress::Progress>,
    act_progress: std::sync::Arc<crate::progress::Progress>,
    tx: mpsc::Sender<TaskMsg>,
    rx: mpsc::Receiver<TaskMsg>,
}

pub fn run(path: PathBuf) -> Result<()> {
    let sessions = db::with_analysis(&path, headers::load_union)?;
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        db_path: path,
        lineage: Lineage::build(&sessions),
        sessions,
        entries: Vec::new(),
        visible: Vec::new(),
        scan: scan::ScanResult::default(),
        scan_state: ScanState::Running { started: Instant::now() },
        sort: SortKey::Recency, // 扫描没到位之前先按时间排
        reverse: false,
        view: View::All,
        filter: String::new(),
        ws_filter: None,
        workspaces: rustc_hash::FxHashMap::default(),
        focus: Pane::Chats,
        ws_rows: Vec::new(),
        ws_table: TableState::default().with_selected(0),
        sub_rows: Vec::new(),
        sub_table: TableState::default(),
        sub_of: None,
        input: InputMode::Normal,
        table: TableState::default().with_selected(0),
        selected_ids: FxHashSet::default(),
        op: OpState::Idle,
        info: InfoLayer::None,
        maint: MaintState::Off,
        quit_requested: false,
        status: None,
        scan_gen: 0,
        scan_progress: std::sync::Arc::new(crate::progress::Progress::new()),
        act_progress: std::sync::Arc::new(crate::progress::Progress::new()),
        tx,
        rx,
    };
    app.refresh_model();
    app.rebuild_entries();
    app.spawn_deep_scan();

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();

    // 退出前必须让维护会话正常收尾: 直接 drop 只放锁不 checkpoint,
    // 残留的 -wal 正是 Cursor 静默回滚的诱因。
    if let MaintState::Held(maint) = std::mem::replace(&mut app.maint, MaintState::Off) {
        eprintln!("退出维护模式,收尾中(checkpoint + 物理收缩)…");
        match maint.release(&app.act_progress) {
            Ok(freed) if freed > 0 => eprintln!("完成,物理收缩 {}。", fmt_bytes(freed)),
            Ok(_) => eprintln!("完成。"),
            Err(e) => eprintln!("警告: 维护模式收尾失败: {}", e.render()),
        }
    }
    result
}

impl App {
    fn scan_ready(&self) -> bool {
        matches!(self.scan_state, ScanState::Ready)
    }

    /// 行数是否已到位(快扫一落地即真;体积另看 `scan.sized`)。
    /// 空库时恒假,但那时也没有可显示的行,无影响。
    fn rows_ready(&self) -> bool {
        self.scan.total_rows > 0 || self.scan_ready()
    }

    /// 当前是否有不能立即中断的后台任务;返回它的名字用于提示。
    ///
    /// 写操作与收尾不可中断(中途放弃会留下与计划不符的状态);
    /// 只读的扫描/预览可以取消,故不算 busy。
    fn busy_reason(&self) -> Option<&'static str> {
        match (&self.op, &self.maint) {
            // 执行中一律算忙——即便可中断也要等线程归还连接/会话再退出,
            // 否则连接在半途被进程回收,checkpoint 收尾就丢了
            (OpState::Applying { .. }, _) => Some("执行"),
            (_, MaintState::Working) => Some("维护会话操作"),
            (_, MaintState::Leaving { .. }) => Some("维护模式收尾"),
            _ => None,
        }
    }

    /// 请求退出。可取消的任务就地取消,不可中断的等它收尾后自动退出。
    fn request_quit(&mut self) {
        self.quit_requested = true;
        // 只读长任务(进入维护模式的首次扫描、后台深扫)直接叫停
        if matches!(self.maint, MaintState::Entering { .. }) || !self.scan_ready() {
            self.scan_progress.cancel();
        }
        // 预览与可中断的执行(收缩)一并叫停
        let cancellable_op = match &self.op {
            OpState::Previewing { .. } => true,
            OpState::Applying { action, .. } => action.interruptible(),
            _ => false,
        };
        if cancellable_op {
            self.act_progress.cancel();
        }
        if let Some(what) = self.busy_reason() {
            self.status = Some(format!("{what}完成后将自动退出…"));
        }
    }

    /// 可以安全退出了吗(没有不可中断的任务在跑)。
    fn ready_to_quit(&self) -> bool {
        self.quit_requested && self.busy_reason().is_none()
    }

    /// 会话列表变化后重算派生模型: workspaceId → 标签映射
    /// (读若干 workspace.json,毫秒级)与父子归属。
    fn refresh_model(&mut self) {
        self.workspaces = crate::workspace::resolve(&self.sessions, &self.db_path);
        self.lineage = Lineage::build(&self.sessions);
    }

    /// 工作区显示标签。`""` 是"无归属"哨兵(见 `ws_filter`)。
    fn ws_label_of(&self, wid: &str) -> String {
        if wid.is_empty() {
            return "(无归属)".into();
        }
        self.workspaces.get(wid).map_or_else(
            || wid.chars().take(8).collect(),
            |info| info.label.clone(),
        )
    }

    fn rebuild_entries(&mut self) {
        // 在旧的 visible/entries 一致状态上取光标 id,再整体替换——
        // 顺序颠倒会拿旧下标查新(可能变小的)entries,直接越界。
        let keep = self.current_entry().map(|e| e.composer_id.clone());
        let header_ids: FxHashSet<&str> =
            self.sessions.iter().map(|s| s.composer_id.as_str()).collect();
        let sized = self.scan.sized;
        let mut entries: Vec<Entry> = self
            .sessions
            .iter()
            .map(|s| {
                let stat = self.scan.per_composer.get(&s.composer_id);
                let ws_label = s
                    .workspace_id
                    .as_deref()
                    .map(|wid| self.ws_label_of(wid));
                Entry {
                    composer_id: s.composer_id.clone(),
                    name: s.name.clone(),
                    recency: s.recency,
                    is_archived: s.is_archived,
                    attach: Some(self.lineage.attach(&s.composer_id)),
                    rows: stat.map_or(0, |c| c.rows),
                    bytes: sized.then(|| stat.map_or(0, |c| c.bytes)),
                    descendants: self.lineage.descendants_of(&s.composer_id).len(),
                    workspace_id: s.workspace_id.clone(),
                    ws_label,
                }
            })
            .collect();
        for (cid, stat) in &self.scan.per_composer {
            if !header_ids.contains(cid.as_str()) {
                entries.push(Entry {
                    composer_id: cid.clone(),
                    name: None,
                    recency: 0,
                    is_archived: false,
                    attach: None,
                    rows: stat.rows,
                    bytes: sized.then_some(stat.bytes),
                    descendants: 0,
                    workspace_id: None,
                    ws_label: None,
                });
            }
        }
        self.entries = entries;
        self.rebuild_ws_rows();
        self.sub_of = None; // 强制右栏重建(树结构可能已变)
        self.refilter(keep);
    }

    /// 会话(含全部后代)的合计字节数。深扫未到位时返回 None。
    fn subtree_bytes(&self, id: &str) -> Option<u64> {
        if !self.scan.sized {
            return None;
        }
        let own = self.scan.per_composer.get(id).map_or(0, |c| c.bytes);
        let subs: u64 = self
            .lineage
            .descendants_of(id)
            .iter()
            .map(|d| self.scan.per_composer.get(d).map_or(0, |c| c.bytes))
            .sum();
        Some(own + subs)
    }

    /// 重建左栏: 按工作区聚合中栏会话数与子树体积,首行"全部"清除过滤。
    fn rebuild_ws_rows(&mut self) {
        let deep = self.scan.sized;
        // wid("" = 无归属) → (会话数, 字节数)。挂靠子代理不单独计数,
        // 其体积并入所属根会话(删除该根即释放的量)。
        let mut agg: rustc_hash::FxHashMap<String, (u64, u64)> =
            rustc_hash::FxHashMap::default();
        for s in &self.sessions {
            if self.lineage.attach(&s.composer_id) == Attach::Attached {
                continue;
            }
            let wid = s.workspace_id.clone().unwrap_or_default();
            let bytes = self.subtree_bytes(&s.composer_id).unwrap_or(0);
            let e = agg.entry(wid).or_default();
            e.0 += 1;
            e.1 += bytes;
        }
        let mut rows: Vec<WsRow> = agg
            .into_iter()
            .map(|(wid, (sessions, bytes))| WsRow {
                label: self.ws_label_of(&wid),
                id: Some(wid),
                sessions,
                bytes: deep.then_some(bytes),
            })
            .collect();
        rows.sort_by(|a, b| {
            b.bytes
                .unwrap_or(0)
                .cmp(&a.bytes.unwrap_or(0))
                .then(b.sessions.cmp(&a.sessions))
                .then(a.label.cmp(&b.label))
        });
        let total_sessions = rows.iter().map(|r| r.sessions).sum();
        let total_bytes = deep.then(|| rows.iter().map(|r| r.bytes.unwrap_or(0)).sum());
        rows.insert(
            0,
            WsRow { id: None, label: "全部".into(), sessions: total_sessions, bytes: total_bytes },
        );
        // 光标落在当前过滤项上(过滤项消失则回"全部")
        let cursor = self
            .ws_filter
            .as_ref()
            .and_then(|w| rows.iter().position(|r| r.id.as_deref() == Some(w)))
            .unwrap_or(0);
        self.ws_rows = rows;
        self.ws_table.select(Some(cursor));
    }

    /// 右栏子代理树跟随中栏光标(仅在目标变化时重建)。
    fn sync_sub_rows(&mut self) {
        let current = self
            .current_entry()
            .filter(|e| e.has_header())
            .map(|e| e.composer_id.clone());
        if current == self.sub_of {
            return;
        }
        self.sub_of = current.clone();
        self.sub_rows.clear();
        self.sub_table.select(None);
        let Some(root) = current else { return };
        let by_id: rustc_hash::FxHashMap<&str, &headers::SessionHeader> =
            self.sessions.iter().map(|s| (s.composer_id.as_str(), s)).collect();
        // 先序 DFS 展开(children 已排序,防环由 seen 保证)
        let mut seen: FxHashSet<String> = FxHashSet::default();
        seen.insert(root.clone());
        let mut stack: Vec<(String, usize)> = self
            .lineage
            .children_of(&root)
            .iter()
            .rev()
            .map(|c| (c.clone(), 1))
            .collect();
        while let Some((id, depth)) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let header = by_id.get(id.as_str());
            self.sub_rows.push(SubRow {
                name: header.and_then(|h| h.name.clone()),
                bytes: self
                    .scan
                    .sized
                    .then(|| self.scan.per_composer.get(&id).map_or(0, |c| c.bytes)),
                is_dangling: self.lineage.is_dangling(&id),
                depth,
                composer_id: id.clone(),
            });
            for c in self.lineage.children_of(&id).iter().rev() {
                stack.push((c.clone(), depth + 1));
            }
        }
        if !self.sub_rows.is_empty() {
            self.sub_table.select(Some(0));
        }
    }

    fn sort_and_refilter(&mut self) {
        let keep = self.current_entry().map(|e| e.composer_id.clone());
        self.refilter(keep);
    }

    fn refilter(&mut self, keep: Option<String>) {
        let key = self.sort;
        self.entries.sort_by(|a, b| {
            let ord = match key {
                SortKey::Bytes => {
                    b.bytes.unwrap_or(0).cmp(&a.bytes.unwrap_or(0)).then(b.rows.cmp(&a.rows))
                }
                SortKey::Rows => b.rows.cmp(&a.rows),
                SortKey::Recency => b.recency.cmp(&a.recency),
                SortKey::Name => a
                    .name
                    .as_deref()
                    .unwrap_or("\u{10FFFF}")
                    .cmp(b.name.as_deref().unwrap_or("\u{10FFFF}")),
                // 同工作区相邻(形成分组视觉)。label 已消歧,再按 wid
                // 收尾是防御: 万一撞名也不至于两组交错;组内按时间降序
                SortKey::Workspace => a
                    .ws_label
                    .as_deref()
                    .unwrap_or("\u{10FFFF}")
                    .cmp(b.ws_label.as_deref().unwrap_or("\u{10FFFF}"))
                    .then_with(|| a.workspace_id.cmp(&b.workspace_id))
                    .then(b.recency.cmp(&a.recency)),
            };
            if self.reverse { ord.reverse() } else { ord }
        });

        let query = self.filter.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            // 挂靠的子代理在右栏树里,不占中栏行
            .filter(|(_, e)| e.in_chat_list())
            .filter(|(_, e)| match self.view {
                View::All => true,
                View::Orphans => !e.has_header() || e.is_dangling(),
                View::Archived => e.is_archived,
            })
            .filter(|(_, e)| match &self.ws_filter {
                None => true,
                Some(w) if w.is_empty() => e.workspace_id.is_none(),
                Some(w) => e.workspace_id.as_deref() == Some(w),
            })
            .filter(|(_, e)| {
                query.is_empty()
                    || e.composer_id.starts_with(&query)
                    || e.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&query))
                    || e.ws_label.as_deref().is_some_and(|w| w.to_lowercase().contains(&query))
            })
            .map(|(i, _)| i)
            .collect();

        let idx = keep
            .and_then(|id| {
                self.visible.iter().position(|&i| self.entries[i].composer_id == id)
            })
            .unwrap_or(0);
        self.table.select(Some(idx.min(self.visible.len().saturating_sub(1))));
    }

    fn current_entry(&self) -> Option<&Entry> {
        let cursor = self.table.selected().unwrap_or(0);
        self.visible.get(cursor).and_then(|&i| self.entries.get(i))
    }

    /// 右栏光标指向的子代理。
    fn current_sub(&self) -> Option<&SubRow> {
        self.sub_table.selected().and_then(|i| self.sub_rows.get(i))
    }

    /// 焦点栏内移动光标。左栏移动即时应用工作区过滤。
    fn move_selection(&mut self, delta: i64) {
        fn shift(table: &mut TableState, len: usize, delta: i64) {
            if len == 0 {
                return;
            }
            let cur = table.selected().unwrap_or(0) as i64;
            table.select(Some((cur + delta).clamp(0, len as i64 - 1) as usize));
        }
        match self.focus {
            Pane::Chats => shift(&mut self.table, self.visible.len(), delta),
            Pane::Workspaces => {
                shift(&mut self.ws_table, self.ws_rows.len(), delta);
                self.apply_ws_cursor();
            }
            Pane::Detail => shift(&mut self.sub_table, self.sub_rows.len(), delta),
        }
    }

    /// 焦点栏内跳到首/尾。
    fn jump_selection(&mut self, to_end: bool) {
        fn jump(table: &mut TableState, len: usize, to_end: bool) {
            table.select(Some(if to_end { len.saturating_sub(1) } else { 0 }));
        }
        match self.focus {
            Pane::Chats => jump(&mut self.table, self.visible.len(), to_end),
            Pane::Workspaces => {
                jump(&mut self.ws_table, self.ws_rows.len(), to_end);
                self.apply_ws_cursor();
            }
            Pane::Detail => jump(&mut self.sub_table, self.sub_rows.len(), to_end),
        }
    }

    /// 左右移动焦点(Workspaces ↔ Chats ↔ Detail)。
    fn move_focus(&mut self, delta: i64) {
        let order = [Pane::Workspaces, Pane::Chats, Pane::Detail];
        let cur = order.iter().position(|p| *p == self.focus).unwrap_or(1) as i64;
        let next = (cur + delta).clamp(0, order.len() as i64 - 1) as usize;
        self.focus = order[next];
        // 进入右栏时保证有光标可操作
        if self.focus == Pane::Detail
            && self.sub_table.selected().is_none()
            && !self.sub_rows.is_empty()
        {
            self.sub_table.select(Some(0));
        }
    }

    /// 把左栏光标位置应用为工作区过滤。
    fn apply_ws_cursor(&mut self) {
        let cursor = self.ws_table.selected().unwrap_or(0);
        let new_filter = self.ws_rows.get(cursor).and_then(|r| r.id.clone());
        if new_filter != self.ws_filter {
            self.ws_filter = new_filter;
            self.sort_and_refilter();
        }
    }

    /// 开启一轮扫描任务: 叫停上一轮、换用全新的进度对象、递增代号。
    ///
    /// 换实例而不是复位,是因为旧线程要到下一个检查点才会退出,
    /// 期间它仍在写自己那份进度——共用一个实例就会让进度条来回跳。
    fn begin_scan_task(&mut self) -> (ScanGen, std::sync::Arc<crate::progress::Progress>) {
        self.scan_progress.cancel();
        self.scan_gen += 1;
        self.scan_progress = std::sync::Arc::new(crate::progress::Progress::new());
        self.scan_state = ScanState::Running { started: Instant::now() };
        (self.scan_gen, self.scan_progress.clone())
    }

    fn spawn_deep_scan(&mut self) {
        let live = match headers::live_set(&self.sessions) {
            Ok(l) => l,
            Err(e) => {
                self.status = Some(format!("存活集合构建失败: {}", e.render()));
                return;
            }
        };
        let tx = self.tx.clone();
        let path = self.db_path.clone();
        // 维护模式下顺带收集待清扫 key 清单,后续清扫可零重扫直接执行
        let collect = self.maint.engaged();
        let (generation, p) = self.begin_scan_task();
        std::thread::spawn(move || {
            // 第一段: covering-index 快扫(不触数据页,秒级),
            // 行数/孤儿/待删清单先到位,列表立即可排序可操作。
            let fast = db::with_analysis(&path, |conn| {
                let _guard = db::CancelGuard::install(conn, p.clone());
                scan::scan_keys(conn, &live, false, collect, &p)
            });
            let fast_failed = fast.is_err();
            let _ = tx.send(TaskMsg::Scan(generation, fast));
            if fast_failed {
                return;
            }
            // 第二段: 深扫读数据页补体积(成本与库体积成正比)。
            let deep = db::with_analysis(&path, |conn| {
                // 连接级取消: COUNT 与行迭代都能被即时打断
                let _guard = db::CancelGuard::install(conn, p.clone());
                scan::scan_keys(conn, &live, true, collect, &p)
            });
            let _ = tx.send(TaskMsg::Scan(generation, deep));
        });
    }

    /// 进入维护模式: 取锁 + 锁内做唯一一次全量扫描(含待清扫清单)。
    ///
    /// 启动时那轮普通扫描会被 `begin_scan_task` 一并叫停——
    /// 否则两轮扫描并行,既浪费 I/O 又让进度显示打架。
    fn enter_maintenance(&mut self) {
        self.maint = MaintState::Entering { started: Instant::now() };
        let tx = self.tx.clone();
        let path = self.db_path.clone();
        let (generation, p) = self.begin_scan_task();
        std::thread::spawn(move || {
            let r = (|| -> Result<_> {
                let maint = Maintenance::acquire(&path)?;
                let scan = {
                    // 锁内首扫同样可即时取消;guard 在作用域结束卸载,
                    // 不影响会话后续的写操作
                    let _guard = db::CancelGuard::install(maint.conn(), p.clone());
                    let sessions = headers::load_union(maint.conn())?;
                    let live = headers::live_set(&sessions)?;
                    (sessions, scan::scan_keys(maint.conn(), &live, true, true, &p)?)
                };
                Ok((Box::new(maint), scan.0, Box::new(scan.1)))
            })();
            p.finish();
            let _ = tx.send(TaskMsg::Entered(generation, r));
        });
    }

    /// 退出维护模式: 一次性 checkpoint + 物理收缩,放锁。
    fn leave_maintenance(&mut self) {
        let MaintState::Held(maint) = std::mem::replace(&mut self.maint, MaintState::Off) else {
            return;
        };
        self.maint = MaintState::Leaving { started: Instant::now() };
        let writes = maint.writes();
        self.status = Some(format!("退出维护模式(会话内执行了 {writes} 次操作),收尾中…"));
        let tx = self.tx.clone();
        let p = self.act_progress.clone();
        std::thread::spawn(move || {
            let r = maint.release(&p);
            p.finish();
            let _ = tx.send(TaskMsg::Released(r));
        });
    }

    fn start_preview(&mut self, action: Action) {
        if !matches!(self.op, OpState::Idle) {
            self.status = Some("已有操作进行中。".into());
            return;
        }
        if matches!(self.maint, MaintState::Entering { .. } | MaintState::Leaving { .. }) {
            self.status = Some("维护模式切换中,请稍候。".into());
            return;
        }
        // 清扫的数据深度扫描已经算好,直接从内存出预览,零扫描秒开。
        // 非维护模式下执行时仍会重算(快照可能过期);维护模式下锁保证精确。
        if matches!(action, Action::Sweep) && self.scan_ready() {
            let summary = sweep_summary_from_scan(
                &self.scan,
                self.dangling_ids().len(),
                self.maint.engaged(),
            );
            self.op = OpState::Confirming { action, summary, gc_plan: None };
            return;
        }

        self.op = OpState::Previewing { action: action.clone(), started: Instant::now() };
        let tx = self.tx.clone();
        let path = self.db_path.clone();
        let p = self.act_progress.clone();

        // 维护模式: 把会话借给工作线程,预览在锁内的连接上跑;
        // gc 的计划一并带回,确认执行时直接复用,省掉第二次 mark。
        if let MaintState::Held(maint) = std::mem::replace(&mut self.maint, MaintState::Working) {
            std::thread::spawn(move || {
                let r = preview_in_session(&maint, &action, &p);
                p.finish();
                let _ = tx.send(TaskMsg::Preview(action, r, Some(maint)));
            });
            return;
        }

        std::thread::spawn(move || {
            let r = preview(&path, &action, &p).map(|summary| Preview { summary, gc_plan: None });
            p.finish();
            let _ = tx.send(TaskMsg::Preview(action, r, None));
        });
    }

    fn start_apply(&mut self, action: Action, backup: bool, gc_plan: Option<Box<gc::GcPlan>>) {
        self.op = OpState::Applying { action: action.clone(), started: Instant::now() };
        let tx = self.tx.clone();
        let path = self.db_path.clone();
        let p = self.act_progress.clone();

        // 维护模式: 在锁内的连接上执行,跳过收尾(退出模式时统一做),
        // 清扫直接用扫描时收集的 key 清单(孤儿子代理清单同源精确),
        // gc 直接用预览算好的计划。
        if let MaintState::Held(mut maint) =
            std::mem::replace(&mut self.maint, MaintState::Working)
        {
            let (condemned, dangling) = if matches!(action, Action::Sweep) {
                (self.scan.condemned_keys.clone(), self.dangling_ids())
            } else {
                (Vec::new(), Vec::new())
            };
            std::thread::spawn(move || {
                let r = apply_in_session(
                    &mut maint, &action, backup, condemned, dangling, gc_plan, &p,
                );
                p.finish();
                let _ = tx.send(TaskMsg::Applied(r, Some(maint)));
            });
            return;
        }

        std::thread::spawn(move || {
            let r = apply(&path, &action, backup, &p);
            p.finish();
            let _ = tx.send(TaskMsg::Applied(r, None));
        });
    }

    fn poll_background(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                // 被取代的旧扫描: 结果与错误一律丢弃,状态由新任务负责
                TaskMsg::Scan(g, _) | TaskMsg::Entered(g, _) if g != self.scan_gen => {}
                TaskMsg::Scan(_, Ok(scan)) => {
                    let sized = scan.sized;
                    self.scan = scan;
                    // 快扫结果先上屏(行数/孤儿),扫描态保持到深扫补完体积
                    if sized {
                        self.scan_state = ScanState::Ready;
                        if self.sort == SortKey::Recency {
                            self.sort = SortKey::Bytes;
                        }
                    }
                    self.rebuild_entries();
                }
                TaskMsg::Scan(_, Err(e)) => {
                    self.status = Some(if e.is_cancelled() {
                        "扫描已取消。".into()
                    } else {
                        format!("扫描失败: {}", e.render())
                    });
                    self.scan_state = ScanState::Ready;
                }
                TaskMsg::Preview(action, result, returned) => {
                    self.return_session(returned);
                    match result {
                        Ok(pv) => {
                            self.op = OpState::Confirming {
                                action,
                                summary: pv.summary,
                                gc_plan: pv.gc_plan,
                            };
                        }
                        Err(e) => {
                            self.op = OpState::Idle;
                            self.status = Some(format!("预览失败: {}", e.render()));
                        }
                    }
                }
                TaskMsg::Applied(result, returned) => {
                    self.op = OpState::Idle;
                    self.return_session(returned);
                    match result {
                        Ok((summary, patch)) => {
                            self.status = Some(summary);
                            self.selected_ids.clear();
                            // 维护模式下会话独占连接,不能再另开只读连接读 headers
                            let reloaded = match &self.maint {
                                MaintState::Held(m) => headers::load_union(m.conn()),
                                _ => db::with_analysis(&self.db_path, headers::load_union),
                            };
                            match reloaded {
                                Ok(sessions) => {
                                    self.sessions = sessions;
                                    self.refresh_model();
                                }
                                Err(e) => self.status = Some(format!("刷新失败: {}", e.render())),
                            }
                            // 增量修正统计,免去整库重扫(r 键可全量校准)
                            self.apply_patch(patch);
                        }
                        Err(e) => self.status = Some(format!("执行失败: {}", e.render())),
                    }
                }
                TaskMsg::Entered(_, Ok((maint, sessions, scan))) => {
                    self.maint = MaintState::Held(maint);
                    self.sessions = sessions;
                    self.refresh_model();
                    self.scan = *scan;
                    self.scan_state = ScanState::Ready;
                    if self.sort == SortKey::Recency {
                        self.sort = SortKey::Bytes;
                    }
                    self.rebuild_entries();
                    self.status = Some("已进入维护模式: 独占数据库,后续操作零重扫。".into());
                }
                TaskMsg::Entered(_, Err(e)) => {
                    self.maint = MaintState::Off;
                    self.scan_state = ScanState::Ready;
                    if e.is_cancelled() {
                        self.status = Some("已中止维护模式启动,数据库锁已释放。".into());
                    } else {
                        self.status = Some(format!("进入维护模式失败: {}", e.render()));
                        // 启动普通扫描顶回来: 进入维护时把原扫描取代掉了,
                        // 失败后不能让界面停在"无数据"状态
                        if !self.quit_requested {
                            self.spawn_deep_scan();
                        }
                    }
                }
                TaskMsg::Released(result) => {
                    self.maint = MaintState::Off;
                    self.status = Some(match result {
                        Ok(pending) if pending > 0 => format!(
                            "已退出维护模式。仍有 {} 空闲空间未回收,可按 v(增量,可中断)或 V(全量)。",
                            fmt_bytes(pending)
                        ),
                        Ok(_) => "已退出维护模式。".into(),
                        Err(e) => format!("退出维护模式时出错: {}", e.render()),
                    });
                }
            }
        }
    }

    /// 工作线程归还维护会话。借出期间状态为 Working,归还后回到 Held;
    /// 非维护模式(returned=None)则保持原状。
    fn return_session(&mut self, returned: Option<Box<Maintenance>>) {
        if let Some(maint) = returned {
            self.maint = MaintState::Held(maint);
        } else if matches!(self.maint, MaintState::Working) {
            // 线程未归还(内部 panic 等): 锁已随连接释放,退回普通模式
            self.maint = MaintState::Off;
            self.status = Some("维护会话已丢失,已退回普通模式。".into());
        }
    }

    /// 把一次操作的已知效果并入内存统计(saturating,偏差由 r 键全量校准)。
    fn apply_patch(&mut self, patch: ScanPatch) {
        match patch {
            ScanPatch::RemoveComposers(ids) => {
                for id in &ids {
                    self.remove_composer_stats(id);
                }
            }
            ScanPatch::TrimPrefixes(ids) => {
                const TRIMMED: &[&str] =
                    &["checkpointId", "ofsContent", "composerVirtualRowHeights"];
                for id in &ids {
                    let Some(c) = self.scan.per_composer.get_mut(id) else { continue };
                    for prefix in TRIMMED {
                        let Some(ps) = c.per_prefix.remove(prefix) else { continue };
                        c.rows = c.rows.saturating_sub(ps.rows);
                        c.bytes = c.bytes.saturating_sub(ps.bytes);
                        self.scan.total_rows = self.scan.total_rows.saturating_sub(ps.rows);
                        if let Some(g) = self.scan.per_prefix.get_mut(prefix) {
                            g.rows = g.rows.saturating_sub(ps.rows);
                            g.bytes = g.bytes.saturating_sub(ps.bytes);
                        }
                    }
                }
            }
            ScanPatch::SweepOrphans => {
                let orphans: Vec<String> = std::mem::take(&mut self.scan.orphan_composers)
                    .into_iter()
                    .collect();
                for id in &orphans {
                    self.remove_composer_stats(id);
                }
                for g in self.scan.per_prefix.values_mut() {
                    g.orphan_rows = 0;
                    g.orphan_bytes = 0;
                }
                // 墓碑行随清扫一并删除;待删清单已兑现,防止二次清扫重放旧清单
                self.scan.live_tombstone_rows = 0;
                self.scan.condemned_keys.clear();
            }
            ScanPatch::Gc { blob_rows, blob_bytes, content_rows, content_bytes } => {
                self.scan.blob.rows = self.scan.blob.rows.saturating_sub(blob_rows);
                self.scan.blob.bytes = self.scan.blob.bytes.saturating_sub(blob_bytes);
                self.scan.content.rows = self.scan.content.rows.saturating_sub(content_rows);
                self.scan.content.bytes = self.scan.content.bytes.saturating_sub(content_bytes);
                self.scan.total_rows =
                    self.scan.total_rows.saturating_sub(blob_rows + content_rows);
            }
            // 收缩只动物理布局,不改任何逻辑行
            ScanPatch::None => {}
        }
        self.rebuild_entries();
    }

    fn remove_composer_stats(&mut self, id: &str) {
        let Some(c) = self.scan.per_composer.remove(id) else { return };
        self.scan.total_rows = self.scan.total_rows.saturating_sub(c.rows);
        for (prefix, ps) in &c.per_prefix {
            if let Some(g) = self.scan.per_prefix.get_mut(prefix) {
                g.rows = g.rows.saturating_sub(ps.rows);
                g.bytes = g.bytes.saturating_sub(ps.bytes);
            }
        }
        self.scan.orphan_composers.remove(id);
        self.selected_ids.remove(id);
    }

    /// 按成本模型把"回收空间"具体化为增量或全量动作。
    /// 在预览前就定下方式,"执行中可否 Esc 中断"的语义才是准确的。
    fn choose_shrink_action(&mut self) -> Option<Action> {
        let chosen = match &self.maint {
            MaintState::Held(m) => crate::vacuum::choose(m.conn()),
            _ => db::with_analysis(&self.db_path, crate::vacuum::choose),
        };
        match chosen {
            Ok((crate::vacuum::Strategy::Incremental, _)) => Some(Action::Shrink),
            Ok((crate::vacuum::Strategy::Full, _)) => Some(Action::VacuumFull),
            Err(e) => {
                self.status = Some(format!("无法评估空闲页: {}", e.render()));
                None
            }
        }
    }

    /// 动作目标。右栏聚焦时是光标下的子代理(单删,含其后代);
    /// 否则取多选,再退光标所在的有 header 会话。
    fn action_targets(&self) -> Vec<String> {
        if self.focus == Pane::Detail {
            return self.current_sub().map(|s| vec![s.composer_id.clone()]).unwrap_or_default();
        }
        if !self.selected_ids.is_empty() {
            return self.selected_ids.iter().cloned().collect();
        }
        self.current_entry()
            .filter(|e| e.has_header())
            .map(|e| vec![e.composer_id.clone()])
            .unwrap_or_default()
    }

    /// 当前会话集合里的孤儿子代理 id(清扫时连 header 一起删)。
    fn dangling_ids(&self) -> Vec<String> {
        self.lineage.dangling_ids()
    }
}

/// 从深度扫描的内存快照构造清扫预览(免全表扫描)。
fn sweep_summary_from_scan(scan: &scan::ScanResult, dangling: usize, locked: bool) -> String {
    let rows: u64 = scan.per_prefix.values().map(|s| s.orphan_rows).sum::<u64>()
        + scan.live_tombstone_rows;
    let bytes: u64 = scan.per_prefix.values().map(|s| s.orphan_bytes).sum();
    let note = if locked {
        "(维护模式: 排他锁保证该清单精确,执行时直接删除,无需重扫)"
    } else {
        "(数字来自当前扫描快照,执行时在写连接上重新精确计算;r 键可先重扫)"
    };
    format!(
        "将清扫孤儿数据 {} 行 / {}(涉及 {} 个孤儿 composer,存活会话墓碑行 {})。\n\
         {}只删界面上不可见的死数据,自动生成可回滚备份。\n\
         {note}",
        rows,
        ByteSize::b(bytes),
        scan.orphan_composers.len(),
        scan.live_tombstone_rows,
        dangling_note(dangling),
    )
}

/// 孤儿子代理的预览提示行(0 个时为空)。
fn dangling_note(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("含 {n} 个孤儿子代理(父会话已删除),将连 header 一起清除。\n")
    }
}

/// 维护会话内的预览: 复用锁内连接,gc 顺带把计划带回供执行复用。
fn preview_in_session(
    maint: &Maintenance,
    action: &Action,
    p: &std::sync::Arc<crate::progress::Progress>,
) -> Result<Preview> {
    let conn = maint.conn();
    let _guard = db::CancelGuard::install(conn, p.clone());
    let p: &crate::progress::Progress = p;
    let sessions = headers::load_union(conn)?;
    let live = headers::live_set(&sessions)?;
    match action {
        Action::Gc => {
            let plan = gc::plan(conn, &live, p)?;
            let summary = gc_summary(&plan);
            Ok(Preview { summary, gc_plan: Some(Box::new(plan)) })
        }
        Action::Delete(ids) => {
            let targets = delete::resolve_targets_cascading(&sessions, ids)?;
            let plan = delete::plan(conn, targets, p)?;
            Ok(Preview { summary: delete_summary(&plan), gc_plan: None })
        }
        Action::Trim(ids) => {
            let targets = delete::resolve_targets(&sessions, ids)?;
            let plan = trim::plan(conn, targets, false, p)?;
            Ok(Preview { summary: trim_summary(&plan), gc_plan: None })
        }
        Action::Sweep => {
            let dangling = Lineage::build(&sessions).dangling_ids();
            let plan = sweep::plan(conn, &live, dangling, true, p)?;
            Ok(Preview { summary: sweep_summary(&plan), gc_plan: None })
        }
        Action::Shrink | Action::VacuumFull => {
            Ok(Preview { summary: shrink_summary(conn, action)?, gc_plan: None })
        }
    }
}

/// 收缩类操作的预览: 空闲页概况 + 两种方式的取舍说明。
fn shrink_summary(conn: &rusqlite::Connection, action: &Action) -> Result<String> {
    let (pages, bytes) = crate::vacuum::freelist(conn)?;
    if pages == 0 {
        return Ok("没有空闲页可回收,文件已是紧凑状态。".into());
    }
    Ok(match action {
        Action::VacuumFull => format!(
            "全量 VACUUM: 顺序重建整库,回收空闲 {pages} 页 / {}。\n\
             空闲占比高时比增量快一个数量级,但需要约等于有效数据量的临时磁盘空间,\n\
             且单事务执行,**中途不可取消**。",
            ByteSize::b(bytes)
        ),
        _ => format!(
            "增量收缩: 回收空闲 {pages} 页 / {}。\n\
             逐批进行,**随时可按 Esc 取消**,已回收部分保留、下次可续做。\n\
             代价是随机 I/O——空闲页很多或文件系统很慢时会以小时计,\n\
             那种情况下改用全量 VACUUM(帮助里的 V 键)更划算。",
            ByteSize::b(bytes)
        ),
    })
}

/// 维护会话内的执行: 跳过收尾(退出模式时统一做),
/// 清扫用扫描时收集的 key 清单,gc 用预览算好的计划。
fn apply_in_session(
    maint: &mut Maintenance,
    action: &Action,
    backup: bool,
    condemned: Vec<String>,
    dangling: Vec<String>,
    gc_plan: Option<Box<gc::GcPlan>>,
    p: &crate::progress::Progress,
) -> Result<(String, ScanPatch)> {
    let db_path = maint.db_path().to_owned();
    let out = match action {
        Action::Delete(ids) => {
            let o = delete::apply_on(maint.conn_mut(), &db_path, ids, backup, false, p)?;
            (delete_outcome_summary(&o), ScanPatch::RemoveComposers(target_ids(&o.plan.targets)))
        }
        Action::Trim(ids) => {
            let o = trim::apply_on(maint.conn_mut(), &db_path, ids, false, backup, false, p)?;
            (trim_outcome_summary(&o), ScanPatch::TrimPrefixes(target_ids(&o.plan.targets)))
        }
        Action::Sweep => {
            let (rows, purged, bk) = sweep::apply_keys(
                maint.conn_mut(),
                &db_path,
                &condemned,
                &dangling,
                backup,
                p,
            )?;
            (sweep_outcome_summary(rows, purged, &bk), ScanPatch::SweepOrphans)
        }
        Action::Gc => {
            let plan = match gc_plan {
                Some(plan) => *plan,
                // 未带回计划(理论上不会发生): 锁内重算一次
                None => {
                    let sessions = headers::load_union(maint.conn())?;
                    let live = headers::live_set(&sessions)?;
                    gc::plan(maint.conn(), &live, p)?
                }
            };
            let o = gc::apply_plan(maint.conn_mut(), &db_path, plan, backup, false, p)?;
            (gc_outcome_summary(&o), gc_patch(&o))
        }
        Action::Shrink => {
            let r = crate::vacuum::shrink_incremental(maint.conn(), p)?;
            (format!("{}。", r.describe()), ScanPatch::None)
        }
        Action::VacuumFull => {
            // VACUUM 不能在事务里跑;维护会话没有开启长事务,直接执行即可
            let freed = crate::vacuum::vacuum_full(maint.conn(), p)?;
            (format!("VACUUM 完成,文件缩小 {}。", ByteSize::b(freed)), ScanPatch::None)
        }
    };
    maint.bump_writes();
    Ok(out)
}

fn target_ids(targets: &[delete::Target]) -> Vec<String> {
    targets.iter().map(|t| t.composer_id.as_str().to_owned()).collect()
}

fn backup_label(path: &Option<PathBuf>) -> String {
    path.as_ref().map_or("无".into(), |p| p.display().to_string())
}

/// 摘要生成: 预览与执行、普通与维护路径共用同一套文案。
fn delete_summary(plan: &delete::DeletePlan) -> String {
    let cascaded = plan.targets.iter().filter(|t| t.cascaded).count();
    let mut s = format!(
        "将删除 {} 个会话{},共 {} 行 / {}:\n",
        plan.targets.len(),
        if cascaded > 0 { format!("(含连带子代理 {cascaded} 个)") } else { String::new() },
        plan.keys.len(),
        ByteSize::b(plan.total_bytes)
    );
    for t in &plan.targets {
        s.push_str(&format!(
            "  {}{}  {}\n",
            if t.cascaded { "└ 连带 " } else { "" },
            t.composer_id.short(),
            t.name.as_deref().unwrap_or("(未命名)")
        ));
    }
    s.push_str("同步四处存储,自动生成可回滚备份。");
    s
}

fn trim_summary(plan: &trim::TrimPlan) -> String {
    let mut s = format!(
        "将修剪 {} 个会话的快照数据,共 {} 行 / {}(正文保留):\n",
        plan.targets.len(),
        plan.keys.len(),
        ByteSize::b(plan.total_bytes)
    );
    for (prefix, stat) in &plan.per_prefix {
        s.push_str(&format!(
            "  {prefix:<32} {:>7} 行 {:>10}\n",
            stat.rows,
            ByteSize::b(stat.bytes).to_string()
        ));
    }
    s.push_str("影响仅限\"恢复到检查点\",自动生成可回滚备份。");
    s
}

fn sweep_summary(plan: &sweep::SweepPlan) -> String {
    format!(
        "将清扫孤儿数据 {} 行 / {}(涉及 {} 个孤儿 composer,墓碑行 {})。\n\
         {}只删界面上不可见的死数据,自动生成可回滚备份。",
        plan.keys.len(),
        ByteSize::b(plan.total_bytes),
        plan.orphan_composers,
        plan.tombstone_rows,
        dangling_note(plan.dangling_sessions.len()),
    )
}

fn sweep_outcome_summary(rows: u64, purged_headers: u64, bk: &Option<PathBuf>) -> String {
    format!(
        "已清扫 {rows} 行孤儿数据{}。备份: {}",
        if purged_headers > 0 {
            format!(",连带清除 {purged_headers} 个孤儿子代理 header")
        } else {
            String::new()
        },
        backup_label(bk),
    )
}

fn gc_summary(plan: &gc::GcPlan) -> String {
    format!(
        "blob: {} 个 / {},可回收 {} 个 / {}\n\
         composer.content: {} 个 / {},可回收 {} 个 / {}\n\
         根解码失败率 {:.2}%(阈值 2%)。自动生成可回滚备份。",
        plan.total_blobs,
        ByteSize::b(plan.total_bytes),
        plan.orphans.len(),
        ByteSize::b(plan.orphan_bytes),
        plan.content_total,
        ByteSize::b(plan.content_bytes),
        plan.content_orphans.len(),
        ByteSize::b(plan.content_orphan_bytes),
        plan.root_error_rate * 100.0,
    )
}

fn delete_outcome_summary(o: &delete::DeleteOutcome) -> String {
    format!(
        "已删除 {} 个会话({} 行,workspace 库改写 {} 个)。备份: {}",
        o.plan.targets.len(),
        o.deleted_rows,
        o.workspaces_edited.len(),
        backup_label(&o.backup_path),
    )
}

fn trim_outcome_summary(o: &trim::TrimOutcome) -> String {
    format!(
        "已修剪 {} 行 / {}。备份: {}",
        o.deleted_rows,
        ByteSize::b(o.plan.total_bytes),
        backup_label(&o.backup_path),
    )
}

fn gc_outcome_summary(o: &gc::GcOutcome) -> String {
    format!(
        "已回收 {} 个孤儿 blob + {} 个 content 行,共 {}。备份: {}",
        o.plan.orphans.len(),
        o.plan.content_orphans.len(),
        ByteSize::b(o.plan.orphan_bytes + o.plan.content_orphan_bytes),
        backup_label(&o.backup_path),
    )
}

fn gc_patch(o: &gc::GcOutcome) -> ScanPatch {
    ScanPatch::Gc {
        blob_rows: o.plan.orphans.len() as u64,
        blob_bytes: o.plan.orphan_bytes,
        content_rows: o.plan.content_orphans.len() as u64,
        content_bytes: o.plan.content_orphan_bytes,
    }
}

/// 后台线程: 普通模式的 dry-run 预览(独立只读连接)。
fn preview(
    path: &std::path::Path,
    action: &Action,
    p: &std::sync::Arc<crate::progress::Progress>,
) -> Result<String> {
    db::with_analysis(path, |conn| {
        let _guard = db::CancelGuard::install(conn, p.clone());
        let p: &crate::progress::Progress = p;
        let sessions = headers::load_union(conn)?;
        let live = headers::live_set(&sessions)?;
        match action {
            Action::Delete(ids) => {
                let targets = delete::resolve_targets_cascading(&sessions, ids)?;
                Ok(delete_summary(&delete::plan(conn, targets, p)?))
            }
            Action::Trim(ids) => {
                let targets = delete::resolve_targets(&sessions, ids)?;
                Ok(trim_summary(&trim::plan(conn, targets, false, p)?))
            }
            Action::Sweep => {
                let dangling = Lineage::build(&sessions).dangling_ids();
                Ok(sweep_summary(&sweep::plan(conn, &live, dangling, true, p)?))
            }
            Action::Gc => Ok(gc_summary(&gc::plan(conn, &live, p)?)),
            Action::Shrink | Action::VacuumFull => shrink_summary(conn, action),
        }
    })
}

/// 后台线程: 普通模式的执行(内部过写安全门,自动备份并收尾)。
fn apply(
    path: &std::path::Path,
    action: &Action,
    backup: bool,
    p: &crate::progress::Progress,
) -> Result<(String, ScanPatch)> {
    match action {
        Action::Delete(ids) => {
            let o = delete::apply(path, ids, backup, p)?;
            Ok((
                delete_outcome_summary(&o),
                ScanPatch::RemoveComposers(target_ids(&o.plan.targets)),
            ))
        }
        Action::Trim(ids) => {
            let o = trim::apply(path, ids, false, backup, p)?;
            Ok((trim_outcome_summary(&o), ScanPatch::TrimPrefixes(target_ids(&o.plan.targets))))
        }
        Action::Sweep => {
            let o = sweep::apply(path, backup, p)?;
            Ok((
                sweep_outcome_summary(o.deleted_rows, o.purged_header_rows, &o.backup_path),
                ScanPatch::SweepOrphans,
            ))
        }
        Action::Gc => {
            let o = gc::apply(path, backup, p)?;
            Ok((gc_outcome_summary(&o), gc_patch(&o)))
        }
        Action::Shrink => {
            let conn = crate::safety::open_write_gated(path)?;
            crate::safety::checkpoint_truncate(&conn)?;
            let r = crate::vacuum::shrink_incremental(&conn, p)?;
            crate::safety::checkpoint_truncate(&conn)?;
            Ok((format!("{}。", r.describe()), ScanPatch::None))
        }
        Action::VacuumFull => {
            let conn = crate::safety::open_write_gated(path)?;
            crate::safety::checkpoint_truncate(&conn)?;
            let freed = crate::vacuum::vacuum_full(&conn, p)?;
            crate::safety::checkpoint_truncate(&conn)?;
            Ok((
                format!("VACUUM 完成,文件缩小 {}。", ByteSize::b(freed)),
                ScanPatch::None,
            ))
        }
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_background();
        // 退出请求在这里兑现: 被取消/收尾的任务回流后即可安全离开
        if app.ready_to_quit() {
            return Ok(());
        }
        app.sync_sub_rows(); // 右栏树跟随中栏光标(目标未变时零成本)
        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // 过滤输入模式独占键盘
        if app.input == InputMode::Filter {
            match key.code {
                KeyCode::Esc => {
                    app.filter.clear();
                    app.input = InputMode::Normal;
                    app.sort_and_refilter();
                }
                KeyCode::Enter => app.input = InputMode::Normal,
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.sort_and_refilter();
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.sort_and_refilter();
                }
                _ => {}
            }
            continue;
        }

        // 可中断的执行(收缩)按 Esc 就地叫停: 已回收部分保留
        if let OpState::Applying { action, .. } = &app.op
            && action.interruptible()
            && key.code == KeyCode::Esc
        {
            app.act_progress.cancel();
            app.status = Some("正在停止收缩…".into());
            continue;
        }

        // 确认态独占键盘: y/Y 进入执行,其它键取消回 Idle
        if matches!(app.op, OpState::Confirming { .. }) {
            let OpState::Confirming { action, gc_plan, .. } =
                std::mem::replace(&mut app.op, OpState::Idle)
            else {
                unreachable!("guarded by matches! above");
            };
            match key.code {
                KeyCode::Char('y') => app.start_apply(action, true, gc_plan),
                KeyCode::Char('Y') => app.start_apply(action, false, gc_plan),
                _ => app.status = Some("已取消。".into()),
            }
            continue;
        }

        // 信息层: 任意键关闭
        if app.info != InfoLayer::None {
            app.info = InfoLayer::None;
            continue;
        }

        match key.code {
            // Esc 优先用于就地取消(清过滤 / 中止启动),q 一律是退出意图
            KeyCode::Esc if !app.filter.is_empty() => {
                app.filter.clear();
                app.sort_and_refilter();
            }
            KeyCode::Esc if app.ws_filter.is_some() => {
                app.ws_filter = None;
                app.ws_table.select(Some(0));
                app.sort_and_refilter();
            }
            KeyCode::Esc if matches!(app.maint, MaintState::Entering { .. }) => {
                app.scan_progress.cancel();
                app.status = Some("正在中止维护模式启动…".into());
            }
            KeyCode::Esc if app.focus != Pane::Chats => app.focus = Pane::Chats,
            KeyCode::Char('q') | KeyCode::Esc => {
                app.request_quit();
                if app.ready_to_quit() {
                    return Ok(());
                }
            }
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::PageDown => app.move_selection(20),
            KeyCode::PageUp => app.move_selection(-20),
            KeyCode::Left | KeyCode::Char('h') => app.move_focus(-1),
            KeyCode::Right | KeyCode::Char('l') => app.move_focus(1),
            KeyCode::Home | KeyCode::Char('g') => app.jump_selection(false),
            KeyCode::End | KeyCode::Char('G') => app.jump_selection(true),
            KeyCode::Char('s') => {
                app.sort = app.sort.next();
                app.sort_and_refilter();
            }
            KeyCode::Char('S') => {
                app.reverse = !app.reverse;
                app.sort_and_refilter();
            }
            KeyCode::Tab => {
                app.view = app.view.next();
                app.sort_and_refilter();
            }
            KeyCode::Char('/') => app.input = InputMode::Filter,
            // Enter: 左栏确认过滤并回中栏;中栏进右栏看子代理/详情
            KeyCode::Enter => match app.focus {
                Pane::Workspaces => app.focus = Pane::Chats,
                Pane::Chats => {
                    if app.current_entry().is_some() {
                        app.focus = Pane::Detail;
                        app.sync_sub_rows();
                        if app.sub_table.selected().is_none() && !app.sub_rows.is_empty() {
                            app.sub_table.select(Some(0));
                        }
                    }
                }
                Pane::Detail => {}
            },
            KeyCode::Char('?') => app.info = InfoLayer::Help,
            KeyCode::Char(' ') if app.focus == Pane::Chats => {
                if let Some(e) = app.current_entry() {
                    if !e.has_header() {
                        app.status = Some("孤儿数据不按会话删,用 x(清扫)处理。".into());
                    } else {
                        let id = e.composer_id.clone();
                        if !app.selected_ids.remove(&id) {
                            app.selected_ids.insert(id);
                        }
                        app.move_selection(1);
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                let targets = app.action_targets();
                if targets.is_empty() {
                    app.status = Some("先用空格选中或将光标移到要删除的会话。".into());
                } else {
                    app.start_preview(Action::Delete(targets));
                }
            }
            KeyCode::Char('t') => {
                let targets = app.action_targets();
                if targets.is_empty() {
                    app.status = Some("先用空格选中或将光标移到要修剪的会话。".into());
                } else {
                    app.start_preview(Action::Trim(targets));
                }
            }
            KeyCode::Char('r') => {
                // 维护模式禁止重扫: spawn_deep_scan 走 immutable 只读连接,
                // 它无视锁与 WAL,读到的是会话开始前的陈旧快照,
                // 会把排他锁保证精确的内存模型(含待清扫清单)覆盖掉。
                if app.maint.engaged() {
                    app.status =
                        Some("维护模式下模型由排他锁保证精确,无需重扫(统计已增量修正)。".into());
                } else if app.scan_ready() {
                    app.status = Some("重新扫描…".into());
                    app.spawn_deep_scan();
                }
            }
            KeyCode::Char('v') => {
                if let Some(action) = app.choose_shrink_action() {
                    app.start_preview(action);
                }
            }
            KeyCode::Char('V') => app.start_preview(Action::VacuumFull),
            KeyCode::Char('M') => match &app.maint {
                MaintState::Off => {
                    if matches!(app.op, OpState::Idle) {
                        app.status = Some("正在取得数据库独占锁…".into());
                        app.enter_maintenance();
                    } else {
                        app.status = Some("有操作进行中,无法切换维护模式。".into());
                    }
                }
                MaintState::Held(_) => {
                    app.status = Some("正在收尾并释放锁…".into());
                    app.leave_maintenance();
                }
                MaintState::Entering { .. } => {
                    app.scan_progress.cancel();
                    app.status = Some("正在中止维护模式启动…".into());
                }
                MaintState::Leaving { .. } | MaintState::Working => {
                    app.status = Some("维护会话忙,请稍候。".into());
                }
            },
            KeyCode::Char('w') => app.focus = Pane::Workspaces,
            KeyCode::Char('x') => app.start_preview(Action::Sweep),
            KeyCode::Char('c') => app.start_preview(Action::Gc),
            _ => {}
        }
    }
}

fn fmt_bytes(b: u64) -> String {
    ByteSize::b(b).to_string()
}

fn fmt_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "-".into())
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, mid, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Fill(1),
    ])
    .areas(mid);
    mid
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    // 维护模式占一行常驻横幅: 持锁期间启动 Cursor 会触发它的静默回滚
    let banner_height = u16::from(app.maint.engaged());
    let [title_area, banner_area, panes_area, summary_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner_height),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    // 三栏: 工作区 | 会话 | 详情/子代理
    let [ws_area, table_area, detail_area] = Layout::horizontal([
        Constraint::Length(24),
        Constraint::Fill(1),
        Constraint::Percentage(30),
    ])
    .areas(panes_area);

    if banner_height > 0 {
        let text = match &app.maint {
            MaintState::Off => String::new(),
            MaintState::Entering { started } => format!(
                " 维护模式启动中 {:.0}s | {} | [Esc/M] 取消 ",
                started.elapsed().as_secs_f64(),
                app.scan_progress.render().unwrap_or_else(|| "取锁中…".into()),
            ),
            MaintState::Leaving { started } => format!(
                " 退出维护模式 {:.0}s | {} ",
                started.elapsed().as_secs_f64(),
                app.act_progress.render().unwrap_or_else(|| "收尾中…".into()),
            ),
            MaintState::Held(_) | MaintState::Working => {
                " ⚠ 维护模式: 已独占数据库(零重扫)。此期间切勿启动 Cursor——它会把主库判为损坏并回滚。[M] 退出 "
                    .to_string()
            }
        };
        f.render_widget(
            Line::from(text).style(Style::new().fg(Color::Black).bg(Color::Red).bold()),
            banner_area,
        );
    }

    // 标题行
    let mut title_parts = vec![
        " cursor-chat-cleanup ".bold(),
        format!("视图:{} ", app.view.label()).into(),
        format!("排序:{}{} ", app.sort.label(), if app.reverse { "↑" } else { "↓" }).into(),
    ];
    if let Some(w) = &app.ws_filter {
        title_parts.push(format!("工作区:{} ", app.ws_label_of(w)).fg(Color::Green));
    }
    if !app.filter.is_empty() || app.input == InputMode::Filter {
        let editing = app.input == InputMode::Filter;
        title_parts.push(
            format!("过滤:{}{} ", app.filter, if editing { "▏" } else { "" }).fg(Color::Yellow),
        );
    }
    title_parts.push("[?]帮助".dim());
    f.render_widget(Line::from(title_parts), title_area);

    draw_ws_pane(f, app, ws_area);
    draw_chat_pane(f, app, table_area);
    draw_detail_pane(f, app, detail_area);

    // 状态栏
    let op_line = match &app.op {
        OpState::Previewing { action, started } => {
            Some((format!("{} 预览", action.verb()), *started))
        }
        OpState::Applying { action, started } => {
            Some((format!("{} 执行", action.verb()), *started))
        }
        OpState::Idle | OpState::Confirming { .. } => None,
    };
    let summary = if let Some((verb, started)) = op_line {
        let detail = app.act_progress.render().map_or(String::new(), |d| format!(" | {d}"));
        Line::from(
            format!(
                " {verb}中… {:.0}s{detail}(执行期间请勿启动 Cursor) ",
                started.elapsed().as_secs_f64()
            )
            .fg(Color::Black)
            .bg(Color::Yellow),
        )
    } else if !app.scan_ready() && !app.maint.engaged() {
        // 扫描进行中: 进度优先于状态消息(否则一条旧提示会把进度挡住);
        // 维护模式下横幅已在显示进度,这里让位给状态消息
        let started = match app.scan_state {
            ScanState::Running { started } => started,
            ScanState::Ready => unreachable!("guarded by scan_ready above"),
        };
        let tail = app.status.as_deref().map_or(String::new(), |s| format!(" | {s}"));
        Line::from(
            format!(
                " 扫描中 {:.0}s | {}{tail} ",
                started.elapsed().as_secs_f64(),
                app.scan_progress.render().unwrap_or_else(|| "准备中…".into()),
            )
            .fg(Color::Yellow),
        )
    } else if let Some(status) = &app.status {
        Line::from(format!(" {status} ").fg(Color::Yellow))
    } else if app.scan_ready() {
        let owned: u64 = app.scan.per_prefix.values().map(|s| s.bytes).sum();
        let orphan: u64 = app.scan.per_prefix.values().map(|s| s.orphan_bytes).sum();
        Line::from(vec![
            format!(
                " 共 {} 行 | 会话自有 {} | blob {} | content {} ",
                app.scan.total_rows,
                fmt_bytes(owned),
                fmt_bytes(app.scan.blob.bytes),
                fmt_bytes(app.scan.content.bytes),
            )
            .into(),
            format!("| 孤儿可回收 {} ", fmt_bytes(orphan)).fg(Color::Yellow),
            "[h/l]栏 [空格]选 [d]删 [t]修剪 [x]清扫 [c]GC [v]收缩 [r]重扫 [M]维护".dim(),
        ])
    } else {
        // 仅在"维护模式 + 扫描中 + 无状态消息"时到达
        let started = match app.scan_state {
            ScanState::Running { started } => started,
            ScanState::Ready => unreachable!("scan_ready branch above already returned"),
        };
        Line::from(
            format!(
                " 扫描中… {:.0}s{},行数与体积稍后填充 ",
                started.elapsed().as_secs_f64(),
                app.scan_progress.render().map_or(String::new(), |d| format!(" | {d}")),
            )
            .fg(Color::Yellow),
        )
    };
    f.render_widget(summary.style(Style::new().bg(Color::DarkGray)), summary_area);

    // 确认层(操作生命周期的 Confirming 态)
    if let OpState::Confirming { action, summary, .. } = &app.op {
        let area = centered_rect(70, 50, f.area());
        f.render_widget(Clear, area);
        let mut text = Text::from(summary.as_str());
        text.push_line(Line::from(""));
        text.push_line(
            Line::from("[y] 执行(自动备份)   [Y] 无备份执行(不可回滚)   [其它键] 取消")
                .bold()
                .fg(Color::Yellow),
        );
        f.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(format!(" 确认: {} ", action.verb()))
                    .border_style(Style::new().fg(Color::Yellow)),
            ),
            area,
        );
    }

    // 信息层
    if app.info == InfoLayer::Help {
        let area = centered_rect(60, 60, f.area());
        f.render_widget(Clear, area);
        let help = "\
导航     j/k ↑/↓ PgUp/PgDn g/G 栏内移动
栏切换   h/l ←/→ 在 工作区|会话|详情 三栏间移动焦点
         w 跳到工作区栏  Enter 进下一栏  Esc 回会话栏
工作区   左栏移动光标即过滤(重名已用路径消歧),Esc 清除
排序     s 切换字段   S 反向
视图     Tab 全部/孤儿/归档(孤儿含父会话已删的子代理)
过滤     / 输入,Enter 确定,Esc 清除(也匹配工作区名)
选择     空格(中栏多选)
清理     d 删除会话(必须连带其全部子代理;右栏聚焦时单删该子代理)
         t 修剪快照(保留正文)
         x 清扫孤儿(孤儿子代理连 header 一起删)   c blob+content GC
         全部两段式: 预览 → 确认执行
         y 带备份执行(restore 可回滚)
         Y 无备份彻底删除(不可回滚)
刷新     r 全量重扫(操作后统计为增量修正;维护模式下无需也不可用)
回收空间 v 自动按空闲量选择: 量小走增量(可 Esc 中断),
           量大走全量 VACUUM(顺序重建,快得多但不可中断)
         V 强制全量 VACUUM
         删除只是逻辑释放,文件变小要靠这一步
维护模式 M 切换。取整库排他锁,一次扫描后所有操作零重扫。
         退出只做 checkpoint,不会被收缩卡住。
         代价: 持锁期间绝不能启动 Cursor(它会回滚数据库)。
退出     q(忙时先收尾再自动退出)";
        f.render_widget(
            Paragraph::new(help).block(Block::bordered().title(" 帮助 ")),
            area,
        );
    }
}

/// 焦点栏的标题样式(视觉上指示 h/l 焦点位置)。
fn pane_title_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Green).bold()
    } else {
        Style::new().fg(Color::Cyan)
    }
}

/// 左栏: 工作区列表(移动光标即过滤)。
fn draw_ws_pane(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Workspaces;
    let active = app.ws_filter.as_deref();
    let rows = app.ws_rows.iter().map(|r| {
        let current = match (&r.id, active) {
            (None, None) => true,
            (Some(id), Some(w)) => id == w,
            _ => false,
        };
        Row::new([
            if current { "●" } else { " " }.to_string(),
            r.label.clone(),
            r.sessions.to_string(),
        ])
    });
    let table = Table::new(
        rows,
        [Constraint::Length(1), Constraint::Fill(1), Constraint::Length(4)],
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(
        Block::new()
            .borders(Borders::TOP | Borders::RIGHT)
            .title(format!("工作区 {}", app.ws_rows.len().saturating_sub(1)))
            .title_style(pane_title_style(focused)),
    );
    let mut state = std::mem::take(&mut app.ws_table);
    f.render_stateful_widget(table, area, &mut state);
    app.ws_table = state;
}

/// 中栏: 会话列表(只列主代理/无归属子代理/孤儿;挂靠子代理在右栏)。
fn draw_chat_pane(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Chats;
    // 选定具体工作区后该列全同,隐藏省宽度
    let show_ws = app.ws_filter.is_none();
    let mut header_cells =
        vec!["", "体积", "行数", "最后更新", "标记", "工作区", "名称", "id"];
    if !show_ws {
        header_cells.remove(5);
    }
    let header = Row::new(header_cells)
        .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    let rows = app.visible.iter().map(|&i| {
        let e = &app.entries[i];
        let sel = if app.selected_ids.contains(&e.composer_id) { "✓" } else { "" };
        let bytes = e.bytes.map_or("…".into(), fmt_bytes);
        let rows = if app.rows_ready() { e.rows.to_string() } else { "…".into() };
        let time = if e.recency > 0 { fmt_time(e.recency) } else { "-".into() };
        let mut marks = String::new();
        if !e.has_header() || e.is_dangling() {
            marks.push('孤');
        }
        if e.is_dangling() {
            marks.push('子');
        }
        if e.attach == Some(Attach::Unattributable) {
            marks.push('悬');
        }
        if e.is_archived {
            marks.push('档');
        }
        let mut name = e
            .name
            .as_deref()
            .unwrap_or(if e.has_header() { "(未命名)" } else { "(孤儿数据)" })
            .to_owned();
        if e.descendants > 0 {
            name.push_str(&format!(" (+{}子)", e.descendants));
        }
        let style = if app.selected_ids.contains(&e.composer_id) {
            Style::new().fg(Color::Yellow)
        } else if !e.has_header() || e.is_dangling() {
            Style::new().fg(Color::Red)
        } else if e.is_archived {
            Style::new().dim()
        } else {
            Style::new()
        };
        let mut cells = vec![
            sel.to_string(),
            bytes,
            rows,
            time,
            marks,
            e.ws_label.clone().unwrap_or_else(|| "-".into()),
            name,
            e.composer_id.chars().take(8).collect(),
        ];
        if !show_ws {
            cells.remove(5);
        }
        Row::new(cells).style(style)
    });
    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(16),
        Constraint::Length(4),
        Constraint::Length(16),
        Constraint::Fill(1),
        Constraint::Length(8),
    ];
    if !show_ws {
        widths.remove(5);
    }
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .block(
            Block::new()
                .borders(Borders::TOP)
                .title(format!(
                    "会话 {}/{} (孤儿 composer {},已选 {})",
                    app.visible.len(),
                    app.entries.len(),
                    app.scan.orphan_composers.len(),
                    app.selected_ids.len(),
                ))
                .title_style(pane_title_style(focused)),
        );
    // 就地渲染: 让 ratatui 把选中行钳制与滚动偏移写回状态。
    // clone 后丢弃会导致陈旧的选中下标每帧被钳到最后一行(实测 bug)。
    let mut state = std::mem::take(&mut app.table);
    f.render_stateful_widget(table, area, &mut state);
    app.table = state;
}

/// 右栏: 光标会话的详情 + 子代理树。
fn draw_detail_pane(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Detail;
    let block = Block::new()
        .borders(Borders::TOP | Borders::LEFT)
        .title(format!("详情/子代理 {}", app.sub_rows.len()))
        .title_style(pane_title_style(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(e) = app.current_entry().cloned() else { return };

    // 子代理树占下半区(有子代理时),详情占其余
    let sub_h = if app.sub_rows.is_empty() {
        0
    } else {
        (app.sub_rows.len() as u16 + 2).min(inner.height / 2)
    };
    let [detail_area, subs_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(sub_h)]).areas(inner);

    let header = app.sessions.iter().find(|s| s.composer_id == e.composer_id);
    let stat = app.scan.per_composer.get(&e.composer_id);
    let mut lines: Vec<Line> = vec![
        Line::from(vec!["composerId  ".dim(), e.composer_id.clone().into()]),
        Line::from(vec![
            "名称        ".dim(),
            e.name.clone().unwrap_or_else(|| "(未命名)".into()).into(),
        ]),
    ];
    if let Some(h) = header {
        lines.push(Line::from(vec![
            "创建        ".dim(),
            h.created_at.map_or("-".into(), fmt_time).into(),
        ]));
        lines.push(Line::from(vec![
            "最后更新    ".dim(),
            h.last_updated_at.map_or("-".into(), fmt_time).into(),
        ]));
        match &h.workspace_id {
            Some(wid) => {
                lines.push(Line::from(vec![
                    "工作区      ".dim(),
                    app.ws_label_of(wid).into(),
                ]));
                if let Some(folder) = app.workspaces.get(wid).and_then(|i| i.folder.clone()) {
                    lines.push(Line::from(vec!["            ".dim(), folder.dim()]));
                }
            }
            None => {
                lines.push(Line::from(vec!["工作区      ".dim(), "-".into()]));
            }
        }
        match e.attach {
            Some(Attach::Dangling) => {
                let parent = h.parent_composer_id.as_deref().unwrap_or("?");
                lines.push(Line::from(
                    format!("孤儿子代理  父会话 {} 已删除,可被清扫", &parent[..8.min(parent.len())])
                        .fg(Color::Red),
                ));
            }
            Some(Attach::Unattributable) => {
                lines.push(Line::from("无归属子代理(父链接未知,保守保留)".fg(Color::Yellow)));
            }
            _ => {
                if let Some(parent) = &h.parent_composer_id {
                    lines.push(Line::from(vec!["父会话      ".dim(), parent.clone().into()]));
                }
            }
        }
        lines.push(Line::from(vec![
            "来源        ".dim(),
            match (h.in_header_table, h.in_legacy_blob) {
                (true, true) => "header 表 + 旧 blob",
                (true, false) => "header 表",
                (false, true) => "仅旧 blob",
                (false, false) => "-",
            }
            .into(),
        ]));
        let mut flags = Vec::new();
        if h.is_archived {
            flags.push("已归档");
        }
        if h.is_best_of_n {
            flags.push("Best-of-N 子会话");
        }
        if !flags.is_empty() {
            lines.push(Line::from(vec!["标记        ".dim(), flags.join(", ").into()]));
        }
    } else {
        lines.push(Line::from("状态        孤儿(不在任何 header 来源)".fg(Color::Red)));
    }
    lines.push(Line::from(""));
    match stat {
        Some(stat) if app.scan_ready() => {
            lines.push(Line::from(
                format!("存储占用: {} 行 / {}", stat.rows, fmt_bytes(stat.bytes)).bold(),
            ));
            if e.descendants > 0
                && let Some(total) = app.subtree_bytes(&e.composer_id)
            {
                lines.push(Line::from(format!(
                    "含 {} 个子代理合计: {}",
                    e.descendants,
                    fmt_bytes(total)
                )));
            }
            for (prefix, p) in &stat.per_prefix {
                lines.push(Line::from(format!(
                    "  {prefix:<30} {:>6} 行 {:>9}",
                    p.rows,
                    fmt_bytes(p.bytes)
                )));
            }
        }
        Some(stat) => {
            lines.push(Line::from(format!("存储占用: {} 行(体积扫描中…)", stat.rows)));
        }
        None => lines.push(Line::from("存储占用: 无 cursorDiskKV 行(幽灵 header)")),
    }
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        detail_area,
    );

    if sub_h > 0 {
        let rows = app.sub_rows.iter().map(|s| {
            let indent = "  ".repeat(s.depth.saturating_sub(1));
            let mut label = format!("{indent}└ {}", s.name.as_deref().unwrap_or("(未命名)"));
            if s.is_dangling {
                label.push_str(" [孤]");
            }
            let style =
                if s.is_dangling { Style::new().fg(Color::Red) } else { Style::new() };
            Row::new([
                label,
                s.bytes.map_or("…".into(), fmt_bytes),
                s.composer_id.chars().take(8).collect(),
            ])
            .style(style)
        });
        let table = Table::new(
            rows,
            [Constraint::Fill(1), Constraint::Length(9), Constraint::Length(8)],
        )
        .row_highlight_style(if focused {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().dim().add_modifier(Modifier::REVERSED)
        })
        .block(
            Block::new()
                .borders(Borders::TOP)
                .title(format!("子代理 {} (d 单删,含其后代)", app.sub_rows.len()))
                .title_style(pane_title_style(focused)),
        );
        let mut state = std::mem::take(&mut app.sub_table);
        f.render_stateful_widget(table, subs_area, &mut state);
        app.sub_table = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: Option<&str>, bytes: u64, live: bool, archived: bool) -> Entry {
        Entry {
            composer_id: id.into(),
            name: name.map(str::to_owned),
            recency: bytes as i64, // 测试里让时间与体积同序
            is_archived: archived,
            attach: live.then_some(Attach::Main),
            rows: bytes / 10,
            bytes: Some(bytes),
            descendants: 0,
            workspace_id: None,
            ws_label: None,
        }
    }

    fn mk_app(entries: Vec<Entry>) -> App {
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            db_path: PathBuf::new(),
            lineage: Lineage::build(&[]),
            sessions: Vec::new(),
            entries,
            visible: Vec::new(),
            scan: scan::ScanResult::default(),
            scan_state: ScanState::Ready,
            sort: SortKey::Bytes,
            reverse: false,
            view: View::All,
            filter: String::new(),
            ws_filter: None,
            workspaces: rustc_hash::FxHashMap::default(),
            focus: Pane::Chats,
            ws_rows: Vec::new(),
            ws_table: TableState::default().with_selected(0),
            sub_rows: Vec::new(),
            sub_table: TableState::default(),
            sub_of: None,
            input: InputMode::Normal,
            table: TableState::default().with_selected(0),
            selected_ids: FxHashSet::default(),
            op: OpState::Idle,
            info: InfoLayer::None,
            maint: MaintState::Off,
            quit_requested: false,
            status: None,
            scan_gen: 0,
            scan_progress: std::sync::Arc::new(crate::progress::Progress::new()),
            act_progress: std::sync::Arc::new(crate::progress::Progress::new()),
            tx,
            rx,
        };
        app.sort_and_refilter();
        app
    }

    fn visible_ids(app: &App) -> Vec<&str> {
        app.visible.iter().map(|&i| app.entries[i].composer_id.as_str()).collect()
    }

    #[test]
    fn filter_views_and_sort() {
        let mut app = mk_app(vec![
            entry("aaaa-1", Some("proto work"), 300, true, false),
            entry("bbbb-2", Some("other"), 200, true, true),
            entry("cccc-3", None, 100, false, false), // 孤儿
        ]);
        // 默认: 按体积降序,全部可见
        assert_eq!(visible_ids(&app), vec!["aaaa-1", "bbbb-2", "cccc-3"]);

        // 反向
        app.reverse = true;
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["cccc-3", "bbbb-2", "aaaa-1"]);
        app.reverse = false;

        // 名称过滤(大小写不敏感)
        app.filter = "PROTO".into();
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["aaaa-1"]);

        // id 前缀过滤
        app.filter = "cccc".into();
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["cccc-3"]);
        app.filter.clear();

        // 视图切换
        app.view = View::Orphans;
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["cccc-3"]);
        app.view = View::Archived;
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["bbbb-2"]);
    }

    /// 回归: entries 被替换成更小集合后,旧 visible 下标不得引发越界 panic。
    #[test]
    fn refilter_survives_entries_shrink() {
        let mut app = mk_app(vec![
            entry("aaaa-1", Some("a"), 300, true, false),
            entry("bbbb-2", Some("b"), 200, true, false),
            entry("cccc-3", Some("c"), 100, true, false),
        ]);
        app.table.select(Some(2)); // 光标在最后一行
        app.entries.truncate(1); // 模拟刷新后集合缩小
        app.sort_and_refilter(); // 不得 panic
        assert_eq!(visible_ids(&app), vec!["aaaa-1"]);
        assert!(app.current_entry().is_some());
    }

    #[test]
    fn sweep_preview_is_instant_when_scan_ready() {
        let mut app = mk_app(vec![entry("aaaa-1", Some("a"), 300, true, false)]);
        app.start_preview(Action::Sweep);
        assert!(
            matches!(app.op, OpState::Confirming { .. }),
            "扫描就绪时清扫预览应零扫描直达确认层"
        );
    }

    /// 回归: 被取代的旧扫描回流时必须整体丢弃。
    /// (实测症状: 启动扫描与维护模式扫描并存,进度条来回跳)
    #[test]
    fn stale_scan_result_is_discarded() {
        let mut app = mk_app(vec![entry("aaaa-1", Some("a"), 300, true, false)]);

        // 第一轮任务
        let (g1, p1) = app.begin_scan_task();
        assert_eq!(g1, 1);
        assert!(!app.scan_ready(), "开启任务后进入扫描中");

        // 第二轮取代它: 旧任务被叫停,且换了独立的进度对象
        let (g2, p2) = app.begin_scan_task();
        assert_eq!(g2, 2);
        assert!(p1.is_cancelled(), "旧任务必须被叫停");
        assert!(!p2.is_cancelled(), "新任务的进度对象是全新的");
        assert!(!std::sync::Arc::ptr_eq(&p1, &p2), "两轮任务不得共用进度对象");

        // 旧任务的结果回流: 必须被丢弃,不改变任何状态
        let stale = scan::ScanResult { total_rows: 999, ..Default::default() };
        app.tx.send(TaskMsg::Scan(g1, Ok(stale))).unwrap();
        app.poll_background();
        assert_eq!(app.scan.total_rows, 0, "过期结果不得写入");
        assert!(!app.scan_ready(), "过期结果不得结束扫描态");

        // 当前任务的快扫结果生效但不结束扫描态(体积还没到)
        let fast = scan::ScanResult { total_rows: 42, ..Default::default() };
        app.tx.send(TaskMsg::Scan(g2, Ok(fast))).unwrap();
        app.poll_background();
        assert_eq!(app.scan.total_rows, 42);
        assert!(app.rows_ready(), "快扫落地后行数可用");
        assert!(!app.scan_ready(), "体积未到,扫描态保持");

        // 深扫结果落地才结束扫描态
        let deep = scan::ScanResult { total_rows: 42, sized: true, ..Default::default() };
        app.tx.send(TaskMsg::Scan(g2, Ok(deep))).unwrap();
        app.poll_background();
        assert!(app.scan_ready());
    }

    #[test]
    fn workspace_filter_and_text_match() {
        let mut a = entry("aaaa-1", Some("a"), 300, true, false);
        a.workspace_id = Some("w1".into());
        a.ws_label = Some("proj-one".into());
        let mut b = entry("bbbb-2", Some("b"), 200, true, false);
        b.workspace_id = Some("w2".into());
        b.ws_label = Some("proj-two".into());
        let c = entry("cccc-3", None, 100, false, false); // 孤儿,无归属
        let mut app = mk_app(vec![a, b, c]);

        // 按 workspaceId 过滤
        app.ws_filter = Some("w1".into());
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["aaaa-1"]);

        // "" 哨兵 = 无归属
        app.ws_filter = Some(String::new());
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["cccc-3"]);

        // 文本过滤也匹配工作区标签
        app.ws_filter = None;
        app.filter = "proj-two".into();
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["bbbb-2"]);
    }

    fn session(cid: &str, wid: Option<&str>) -> headers::SessionHeader {
        headers::SessionHeader {
            composer_id: cid.into(),
            name: None,
            created_at: None,
            last_updated_at: None,
            recency: 0,
            is_archived: false,
            is_subagent: false,
            is_best_of_n: false,
            workspace_id: wid.map(str::to_owned),
            workspace_folder: None,
            parent_composer_id: None,
            sub_composer_ids: Vec::new(),
            in_header_table: true,
            in_legacy_blob: false,
        }
    }

    fn subagent(cid: &str, parent: &str) -> headers::SessionHeader {
        let mut s = session(cid, None);
        s.is_subagent = true;
        s.parent_composer_id = Some(parent.to_owned());
        s
    }

    #[test]
    fn ws_rows_aggregate_and_lead_with_all() {
        let mut app = mk_app(vec![]);
        app.sessions =
            vec![session("a", Some("w1")), session("b", Some("w1")), session("c", None)];
        app.lineage = Lineage::build(&app.sessions);
        app.rebuild_ws_rows();
        assert_eq!(app.ws_rows[0].label, "全部");
        assert_eq!(app.ws_rows[0].sessions, 3);
        let w1 = app.ws_rows.iter().find(|r| r.id.as_deref() == Some("w1")).unwrap();
        assert_eq!(w1.sessions, 2);
        let none = app.ws_rows.iter().find(|r| r.id.as_deref() == Some("")).unwrap();
        assert_eq!(none.label, "(无归属)");
        assert_eq!(none.sessions, 1);
    }

    /// 三栏联动: 挂靠子代理不占中栏行,随光标进右栏树;
    /// 孤儿子代理留在中栏并进"孤儿"视图;左栏聚合不数挂靠子代理。
    #[test]
    fn attached_subagents_move_to_detail_pane() {
        let mut app = mk_app(vec![]);
        app.sessions = vec![
            session("main-1", Some("w1")),
            subagent("sub1-1", "main-1"),
            subagent("sub2-1", "sub1-1"),
            subagent("dang-1", "ghost"),
        ];
        app.lineage = Lineage::build(&app.sessions);
        app.rebuild_entries();

        // 中栏: 主代理 + 孤儿子代理;挂靠的 sub1/sub2 不在
        let mut ids = visible_ids(&app);
        ids.sort();
        assert_eq!(ids, vec!["dang-1", "main-1"]);

        // 孤儿视图包含孤儿子代理
        app.view = View::Orphans;
        app.sort_and_refilter();
        assert_eq!(visible_ids(&app), vec!["dang-1"]);
        app.view = View::All;
        app.sort_and_refilter();

        // 光标移到 main-1,右栏树是两级子代理
        let pos = app
            .visible
            .iter()
            .position(|&i| app.entries[i].composer_id == "main-1")
            .unwrap();
        app.table.select(Some(pos));
        app.sync_sub_rows();
        let tree: Vec<(&str, usize)> =
            app.sub_rows.iter().map(|s| (s.composer_id.as_str(), s.depth)).collect();
        assert_eq!(tree, vec![("sub1-1", 1), ("sub2-1", 2)]);

        // 左栏: 挂靠子代理不计数(w1 只有 main-1;无归属只有 dang-1)
        let w1 = app.ws_rows.iter().find(|r| r.id.as_deref() == Some("w1")).unwrap();
        assert_eq!(w1.sessions, 1);
        let none = app.ws_rows.iter().find(|r| r.id.as_deref() == Some("")).unwrap();
        assert_eq!(none.sessions, 1);

        // 右栏聚焦时动作目标是光标下的子代理
        app.focus = Pane::Detail;
        app.sub_table.select(Some(1));
        assert_eq!(app.action_targets(), vec!["sub2-1".to_string()]);
    }

    #[test]
    fn action_targets_prefer_selection_over_cursor() {
        let mut app = mk_app(vec![
            entry("aaaa-1", Some("a"), 300, true, false),
            entry("cccc-3", None, 100, false, false),
        ]);
        // 光标在第一行(有 header)→ 目标是它
        assert_eq!(app.action_targets(), vec!["aaaa-1".to_string()]);
        // 多选优先
        app.selected_ids.insert("aaaa-1".into());
        assert_eq!(app.action_targets(), vec!["aaaa-1".to_string()]);
        // 光标在孤儿行且无多选 → 无目标
        app.selected_ids.clear();
        app.table.select(Some(1));
        assert!(app.action_targets().is_empty());
    }
}
