//! blob GC 的 mark 阶段: 计算 `agentKv:blob:` 命名空间的存活集合。
//!
//! 蓝本是官方 agentCompatService 的 `collectNestedBlobs`,
//! 而不是官方 GC 那个有确认误删缺陷的 mark 函数。要点:
//! - 对每个被标记的 blob,用 State/Turn/Step/Archive/UserMessage **五种类型
//!   全部尝试**解码并取提取结果的并集,直到收敛。过度标记只会少删,绝不误删。
//! - 补上官方 GC 的两个确认遗漏: `ConversationSummaryArchive` 内部的
//!   blobId、`cloudAgent:` 元数据引用的 state blob。
//! - 防御性字段全部纳入(schema 有定义但当前版本未见写方的 blob 引用):
//!   `conversation_state_blob_id`、`text_blob_id`/`rich_text_blob_id`、
//!   documents/videos/external_links、`subagent_state_refs`。
//! - 任何解码失败都计数;根解码失败率超阈值时拒绝清理(安全阀)。

use std::collections::VecDeque;

use base64::Engine as _;
use buffa::MessageView as _;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use rustc_hash::FxHashSet;
use serde_json::Value;

use crate::error::{Ctx as _, Result};
use crate::proto::agent::v1 as pb;
use crate::scan::{Attribution, attribute};
use crate::types::{BlobHex, hex_decode, hex_decode_into};

#[derive(Debug, Default)]
pub struct MarkStats {
    /// 扫过的 composerData/bubbleId 行数(仅存活会话)。
    pub root_rows: u64,
    /// 其中成功解出 conversationState 的。
    pub root_states: u64,
    /// conversationState 存在但解码失败的(安全阀分子)。
    pub root_decode_errors: u64,
    pub checkpoint_roots: u64,
    pub cloud_agent_roots: u64,
    /// 被引用但 `agentKv:blob:` 里不存在的(官方 GC 历史损伤的信号)。
    pub missing_blobs: u64,
    pub expanded_blobs: u64,
    /// 各阶段耗时(性能观测)。
    pub root_phase: std::time::Duration,
    pub pointer_phase: std::time::Duration,
    pub expand_phase: std::time::Duration,
}

pub struct MarkOutcome {
    /// 存活 blobId,含悬空引用。
    pub marked: FxHashSet<BlobHex>,
    /// 被引用的 `composer.content.<sha256>` 完整 key。
    ///
    /// 收集方式是对存活根行文本与存活 blob 内容做字面量扫描,不解析引用字段。
    /// 实测引用只以明文出现在 bubbleId JSON 和 blob 内容两处,该扫描完备。
    pub content_keys: FxHashSet<String>,
    pub stats: MarkStats,
}

impl MarkOutcome {
    /// 根解码失败率(安全阀依据)。
    pub fn root_error_rate(&self) -> f64 {
        let attempted = self.stats.root_states + self.stats.root_decode_errors;
        if attempted == 0 {
            return 0.0;
        }
        self.stats.root_decode_errors as f64 / attempted as f64
    }
}

/// 提取函数的输出端。Marker(单线程,直接去重入队)和
/// LocalSink(工作线程本地缓冲,汇总时再去重)都实现它。
trait IdSink {
    fn push_id(&mut self, id: &[u8]);
    fn push_content_key(&mut self, key: String);
}

/// 工作线程的本地收集器: 只做编码,不碰共享状态;
/// 汇总时值整体移交(move),不再有第二次分配。
#[derive(Default)]
struct LocalSink {
    ids: Vec<BlobHex>,
    content_keys: Vec<String>,
}

impl IdSink for LocalSink {
    fn push_id(&mut self, id: &[u8]) {
        if !id.is_empty() {
            self.ids.push(BlobHex::from_bytes(id));
        }
    }
    fn push_content_key(&mut self, key: String) {
        self.content_keys.push(key);
    }
}

struct Marker {
    marked: FxHashSet<BlobHex>,
    queue: VecDeque<BlobHex>,
    content_keys: FxHashSet<String>,
    stats: MarkStats,
}

impl IdSink for Marker {
    /// mark 热路径: 先在栈上编码做查重,只有**新** id 才落堆
    /// (每个 blob 平均被引用多次,重复命中是常态)。
    fn push_id(&mut self, id: &[u8]) {
        if id.is_empty() {
            return;
        }
        let mut buf = [0u8; 128];
        match BlobHex::encode_view(id, &mut buf) {
            Some(view) => {
                if !self.marked.contains(view) {
                    self.merge(BlobHex::from_bytes(id));
                }
            }
            // 超长 id(正常数据不出现): 走堆路径,merge 内部仍会去重
            None => self.merge(BlobHex::from_bytes(id)),
        }
    }
    fn push_content_key(&mut self, key: String) {
        self.content_keys.insert(key);
    }
}

/// 在任意字节流里扫 `composer.content.<64 位小写 hex>` 字面量。
/// 官方 key 由 `hex(sha256)` 构成,恒为 64 位小写 hex;
/// 后随更多 hex 字符时仍取前 64 位——误采只会多钉住一行,方向安全。
fn scan_content_keys<S: IdSink>(bytes: &[u8], sink: &mut S) {
    const PREFIX: &[u8] = b"composer.content.";
    const HASH_LEN: usize = 64;
    static FINDER: std::sync::LazyLock<memchr::memmem::Finder<'static>> =
        std::sync::LazyLock::new(|| memchr::memmem::Finder::new(PREFIX));
    for pos in FINDER.find_iter(bytes) {
        let start = pos + PREFIX.len();
        let Some(hash) = bytes.get(start..start + HASH_LEN) else { continue };
        if hash.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
            // SAFETY 等价性: 全部字节已验证为 ASCII hex
            let mut key = String::with_capacity(PREFIX.len() + HASH_LEN);
            key.push_str("composer.content.");
            key.push_str(std::str::from_utf8(hash).expect("all-ASCII checked above"));
            sink.push_content_key(key);
        }
    }
}

impl Marker {
    /// 唯一的入集合点。新 id 进队展开;克隆是 mark 的固有二次分配
    /// (集合与队列各持一份),集中在此处。
    fn merge(&mut self, id: BlobHex) {
        if !self.marked.contains(id.as_str()) {
            self.queue.push_back(id.clone());
            self.marked.insert(id);
        }
    }

    /// 外部来源的 hex 字符串(checkpoint 指针值、cloudAgent 元数据)。
    fn push_hex(&mut self, raw: &str) {
        if let Some(id) = BlobHex::parse(raw) {
            self.merge(id);
        }
    }
}

/// 计算存活 blob 集合。`live` 是存活 composerId 集合。
pub fn mark_live_blobs(
    conn: &Connection,
    live: &crate::types::LiveSet,
    p: &crate::progress::Progress,
) -> Result<MarkOutcome> {
    let mut m = Marker {
        marked: FxHashSet::default(),
        queue: VecDeque::new(),
        content_keys: FxHashSet::default(),
        stats: MarkStats::default(),
    };

    // 根 1/2: 存活会话的 composerData / bubbleId(JSON 里的 conversationState)
    //
    // 流水线并行: 主线程只做 SQLite 迭代 + 行值拷出(有界通道背压),
    // worker 池做 JSON 字段提取 + base64/hex + protobuf 解码 + id 提取。
    // 不做内容预筛——实测两种子串预筛都被这类库击穿(该键存在于几乎
    // 每一行;聊天正文还可能内嵌任意同款字面量),筛选力不可依赖。
    let phase_start = std::time::Instant::now();
    {
        use rayon::prelude::*;

        enum RootVerdict {
            State,
            NoState,
            Error,
        }
        struct RootDecode {
            verdict: RootVerdict,
            sink: LocalSink,
        }

        fn decode_root(json_bytes: &[u8]) -> RootDecode {
            let mut sink = LocalSink::default();
            // content 引用在 bubble JSON 里是明文,与 conversationState 是否为空无关
            scan_content_keys(json_bytes, &mut sink);
            let verdict = match decode_conversation_state(json_bytes) {
                Ok(Some(state_bytes)) => {
                    match pb::ConversationStateStructureView::decode_view(&state_bytes) {
                        Ok(st) => {
                            extract_state(&st, &mut sink, 0);
                            RootVerdict::State
                        }
                        Err(_) => RootVerdict::Error,
                    }
                }
                Ok(None) => RootVerdict::NoState,
                Err(_) => RootVerdict::Error,
            };
            RootDecode { verdict, sink }
        }

        let root_total: u64 = conn.query_row(
            "SELECT COUNT(*) FROM cursorDiskKV
             WHERE (key >= 'composerData:' AND key < 'composerData;')
                OR (key >= 'bubbleId:' AND key < 'bubbleId;')",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        p.stage("解码会话根", root_total);

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(512);
        let worker = std::thread::spawn(move || {
            rx.into_iter()
                .par_bridge()
                .map(|json| decode_root(&json))
                .collect::<Vec<RootDecode>>()
        });

        {
            let mut stmt = conn.prepare(
                "SELECT key, value FROM cursorDiskKV
                 WHERE (key >= 'composerData:' AND key < 'composerData;')
                    OR (key >= 'bubbleId:' AND key < 'bubbleId;')",
            )?;
            let mut rows = stmt.query([])?;
            let mut seen = 0u64;
            while let Some(row) = rows.next()? {
                seen += 1;
                if seen.is_multiple_of(1024) {
                    p.set_done(seen);
                    if p.is_cancelled() {
                        break; // 丢弃发送端,worker 自然收敛后统一报错
                    }
                }
                let ValueRef::Text(key_raw) = row.get_ref(0)? else { continue };
                let Ok(key) = std::str::from_utf8(key_raw) else { continue };
                let Attribution::Owned(_, cid) = attribute(key) else { continue };
                if !live.contains(cid) {
                    continue;
                }
                m.stats.root_rows += 1;
                let json_bytes: &[u8] = match row.get_ref(1)? {
                    ValueRef::Text(t) => t,
                    ValueRef::Blob(b) => b,
                    _ => continue, // NULL 墓碑
                };
                // worker 断开(panic)时这里会 Err;丢弃发送侧,join 处统一收敛
                if tx.send(json_bytes.to_vec()).is_err() {
                    break;
                }
            }
        }
        drop(tx);
        let results = match worker.join() {
            Ok(r) => r,
            Err(payload) => std::panic::resume_unwind(payload),
        };
        p.check()?;

        for r in results {
            match r.verdict {
                RootVerdict::State => m.stats.root_states += 1,
                RootVerdict::NoState => {}
                RootVerdict::Error => m.stats.root_decode_errors += 1,
            }
            for id in r.sink.ids {
                m.merge(id);
            }
            for key in r.sink.content_keys {
                m.content_keys.insert(key);
            }
        }
    }

    m.stats.root_phase = phase_start.elapsed();

    // 根 3: checkpoint 指针(value 就是 hex blobId,指向的 blob 是一棵 state)
    let phase_start = std::time::Instant::now();
    {
        let mut stmt = conn.prepare(
            "SELECT key, value FROM cursorDiskKV
             WHERE (key >= 'agentKv:checkpoint:' AND key < 'agentKv:checkpoint;')
                OR (key >= 'agentKv:bubbleCheckpoint:' AND key < 'agentKv:bubbleCheckpoint;')",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let ValueRef::Text(key_raw) = row.get_ref(0)? else { continue };
            let Ok(key) = std::str::from_utf8(key_raw) else { continue };
            let Attribution::Owned(_, cid) = attribute(key) else { continue };
            if !live.contains(cid) {
                continue;
            }
            let hex: &str = match row.get_ref(1)? {
                ValueRef::Text(t) => std::str::from_utf8(t).unwrap_or(""),
                ValueRef::Blob(b) => std::str::from_utf8(b).unwrap_or(""),
                _ => continue,
            };
            m.stats.checkpoint_roots += 1;
            m.push_hex(hex);
        }
    }

    // 根 4(官方 GC 的遗漏): cloudAgent: 元数据里的 cloudAgentStateBlobId
    {
        let mut stmt = conn.prepare(
            "SELECT value FROM cursorDiskKV
             WHERE key >= 'cloudAgent:' AND key < 'cloudAgent;'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let json_bytes: &[u8] = match row.get_ref(0)? {
                ValueRef::Text(t) => t,
                ValueRef::Blob(b) => b,
                _ => continue,
            };
            let Ok(v) = serde_json::from_slice::<Value>(json_bytes) else { continue };
            let mut found = Vec::new();
            collect_json_field(&v, "cloudAgentStateBlobId", &mut found);
            for hex in found {
                m.stats.cloud_agent_roots += 1;
                m.push_hex(&hex);
            }
        }
    }

    m.stats.pointer_phase = phase_start.elapsed();

    // 不动点展开: 每个被标记的 blob 取回来用五种类型全部尝试,直到收敛。
    // 实测过批量 IN + rayon 的版本: 每 blob 的分配与细粒度并行开销反而
    // 让该阶段慢了近 3 倍,故保持顺序 + 复用缓冲的形态。
    let phase_start = std::time::Instant::now();
    p.stage("展开 blob 引用", 0);
    {
        let mut get_blob =
            conn.prepare("SELECT value FROM cursorDiskKV WHERE key = 'agentKv:blob:' || ?1")?;
        // 复用缓冲: blob 内容拷进同一块 Vec,不在循环里反复分配
        let mut content: Vec<u8> = Vec::new();
        while let Some(hex) = m.queue.pop_front() {
            let found = get_blob.query_row([hex.as_str()], |row| {
                content.clear();
                Ok(match row.get_ref(0)? {
                    // 单字节 0x00 是官方的"空 blob"哨兵
                    ValueRef::Blob(b) if b.len() == 1 && b[0] == 0 => false,
                    ValueRef::Blob(b) => {
                        content.extend_from_slice(b);
                        true
                    }
                    ValueRef::Text(t) => std::str::from_utf8(t)
                        .is_ok_and(|s| hex_decode_into(s, &mut content)),
                    _ => false,
                })
            });
            match found {
                Ok(true) if !content.is_empty() => {
                    m.stats.expanded_blobs += 1;
                    if m.stats.expanded_blobs.is_multiple_of(512) {
                        p.set_done(m.stats.expanded_blobs);
                        p.check()?;
                    }
                    expand_blob(&content, &mut m);
                    // protobuf 里的字符串是裸 UTF-8,content 引用直接可见
                    scan_content_keys(&content, &mut m);
                }
                Ok(_) => {}
                Err(rusqlite::Error::QueryReturnedNoRows) => m.stats.missing_blobs += 1,
                Err(e) => return Err(e).ctx("读取 blob 失败"),
            }
        }
    }
    m.stats.expand_phase = phase_start.elapsed();
    p.finish();

    Ok(MarkOutcome { marked: m.marked, content_keys: m.content_keys, stats: m.stats })
}

/// 只反序列化我们需要的那一个字段。相比 `serde_json::Value` 全树构建,
/// 这让 serde 流式跳过其余内容,且无转义时字符串直接借用原缓冲。
#[derive(serde::Deserialize)]
struct ConversationStateField<'a> {
    #[serde(rename = "conversationState", borrow, default)]
    conversation_state: Option<std::borrow::Cow<'a, str>>,
}

/// 根解码失败的形态。调用方只按 Ok/Err 计数(安全阀分子),
/// 从不传播——因此不进 crate::Error,保留结构以便调试时可见。
#[derive(Debug)]
#[allow(dead_code)] // 字段仅用于 Debug 输出
enum RootDecodeError {
    Json(serde_json::Error),
    Base64(base64::DecodeError),
    Hex,
}

/// 官方解码语义: JSON 的 conversationState 字段,`~` 前缀 base64,否则 hex。
fn decode_conversation_state(json_bytes: &[u8]) -> Result<Option<Vec<u8>>, RootDecodeError> {
    let f: ConversationStateField<'_> =
        serde_json::from_slice(json_bytes).map_err(RootDecodeError::Json)?;
    let Some(s) = f.conversation_state else {
        return Ok(None);
    };
    let s: &str = &s;
    if s.is_empty() {
        return Ok(None);
    }
    let bytes = if let Some(b64) = s.strip_prefix('~') {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(RootDecodeError::Base64)?
    } else {
        hex_decode(s).ok_or(RootDecodeError::Hex)?
    };
    Ok((!bytes.is_empty()).then_some(bytes))
}

/// 在 JSON 树里递归收集指定字段的字符串值(cloudAgent 元数据结构未实测,做宽松匹配)。
fn collect_json_field(v: &Value, field: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == field && let Some(s) = val.as_str() {
                    out.push(s.to_owned());
                }
                collect_json_field(val, field, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_json_field(val, field, out);
            }
        }
        _ => {}
    }
}

const MAX_DEPTH: u32 = 64;

/// 五种类型全部尝试,提取结果取并集(collectNestedBlobs 的保守化版本)。
fn expand_blob<S: IdSink>(content: &[u8], sink: &mut S) {
    if let Ok(st) = pb::ConversationStateStructureView::decode_view(content) {
        extract_state(&st, sink, 0);
    }
    if let Ok(turn) = pb::ConversationTurnStructureView::decode_view(content) {
        extract_turn(&turn, sink, 0);
    }
    if let Ok(step) = pb::ConversationStepView::decode_view(content) {
        extract_step(&step, sink, 0);
    }
    if let Ok(archive) = pb::ConversationSummaryArchiveView::decode_view(content) {
        extract_archive(&archive, sink);
    }
    if let Ok(msg) = pb::UserMessageView::decode_view(content) {
        extract_user_message(&msg, sink);
    }
}

fn extract_state<S: IdSink>(st: &pb::ConversationStateStructureView<'_>, sink: &mut S, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    for id in st.turns.iter() {
        sink.push_id(id);
    }
    for id in st.root_prompt_messages_json.iter() {
        sink.push_id(id);
    }
    for id in st.todos.iter() {
        sink.push_id(id);
    }
    for id in st.summary_archives.iter() {
        sink.push_id(id);
    }
    if let Some(id) = st.summary {
        sink.push_id(id);
    }
    if let Some(id) = st.summary_archive {
        sink.push_id(id);
    }
    if let Some(id) = st.plan {
        sink.push_id(id);
    }
    for id in st.file_states.values() {
        sink.push_id(id);
    }
    for fs in st.file_states_v2.values() {
        if let Some(id) = fs.content {
            sink.push_id(id);
        }
        if let Some(id) = fs.initial_content {
            sink.push_id(id);
        }
    }
    // 防御性: 当前无读写方的死字段,一旦启用即高危
    for id in st.subagent_state_refs.values() {
        sink.push_id(id);
    }
    // 子 agent 的 state 是内联嵌套消息,就地递归
    for sub in st.subagent_states.values() {
        extract_state(&sub.conversation_state, sink, depth + 1);
    }
}

fn extract_turn<S: IdSink>(turn: &pb::ConversationTurnStructureView<'_>, sink: &mut S, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    match &turn.turn {
        Some(pb::conversation_turn_structure::TurnView::AgentConversationTurn(t)) => {
            sink.push_id(t.user_message);
            for id in t.steps.iter() {
                sink.push_id(id);
            }
        }
        Some(pb::conversation_turn_structure::TurnView::ShellConversationTurn(t)) => {
            sink.push_id(t.shell_command);
            sink.push_id(t.shell_output);
        }
        None => {}
    }
}

fn extract_step<S: IdSink>(step: &pb::ConversationStepView<'_>, sink: &mut S, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(pb::conversation_step::MessageView::ToolCall(tc)) = &step.message else {
        return;
    };
    match &tc.tool {
        Some(pb::tool_call::ToolView::ReadToolCall(read)) => {
            if let Some(pb::read_tool_result::ResultView::Success(s)) = &read.result.result {
                match &s.output {
                    Some(pb::read_tool_success::OutputView::DataBlobId(id)) => sink.push_id(id),
                    Some(pb::read_tool_success::OutputView::ContentBlobId(id)) => sink.push_id(id),
                    _ => {}
                }
            }
        }
        Some(pb::tool_call::ToolView::TruncatedToolCall(trunc)) => {
            sink.push_id(trunc.original_step_blob_id);
        }
        Some(pb::tool_call::ToolView::TaskToolCall(task)) => {
            if let Some(pb::task_result::ResultView::Success(s)) = &task.result.result {
                // 内联嵌套 step(不是 blobId),就地递归
                for inner in s.conversation_steps.iter() {
                    extract_step(inner, sink, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn extract_archive<S: IdSink>(archive: &pb::ConversationSummaryArchiveView<'_>, sink: &mut S) {
    // 官方 GC 的遗漏: 这两处是被压缩历史的唯一引用,漏标会导致误删
    for id in archive.summarized_messages.iter() {
        sink.push_id(id);
    }
    sink.push_id(archive.summary_message);
}

fn extract_user_message<S: IdSink>(msg: &pb::UserMessageView<'_>, sink: &mut S) {
    // 防御性三字段(schema 有定义但当前版本未见写方)
    sink.push_id(msg.conversation_state_blob_id);
    if let Some(id) = msg.text_blob_id {
        sink.push_id(id);
    }
    if let Some(id) = msg.rich_text_blob_id {
        sink.push_id(id);
    }

    let ctx = &msg.selected_context;
    for img in ctx.selected_images.iter() {
        match &img.data_or_blob_id {
            Some(pb::selected_image::DataOrBlobIdView::BlobId(id)) => sink.push_id(id),
            // 官方只在内联 data 为空时才算引用;我们无条件标记(保守方向)
            Some(pb::selected_image::DataOrBlobIdView::BlobIdWithData(b)) => sink.push_id(b.blob_id),
            _ => {}
        }
    }
    for doc in ctx.selected_documents.iter() {
        match &doc.data_or_blob_id {
            Some(pb::selected_document::DataOrBlobIdView::BlobId(id)) => sink.push_id(id),
            Some(pb::selected_document::DataOrBlobIdView::BlobIdWithData(b)) => sink.push_id(b.blob_id),
            _ => {}
        }
    }
    for video in ctx.selected_videos.iter() {
        match &video.data_or_blob_id {
            Some(pb::selected_video::DataOrBlobIdView::BlobId(id)) => sink.push_id(id),
            Some(pb::selected_video::DataOrBlobIdView::BlobIdWithData(b)) => sink.push_id(b.blob_id),
            _ => {}
        }
    }
    for entry in ctx.extra_context_entries.iter() {
        if let Some(pb::extra_context_entry::DataOrBlobIdView::BlobId(id)) = &entry.data_or_blob_id {
            sink.push_id(id);
        }
    }
    for pr in ctx.selected_pull_requests.iter() {
        if let Some(id) = pr.blob_id {
            sink.push_id(id);
        }
    }
    for sel in ctx.git_pr_diff_selections.iter() {
        if let Some(id) = sel.blob_id {
            sink.push_id(id);
        }
    }
    for link in ctx.external_links.iter() {
        if let Some(id) = link.blob_id {
            sink.push_id(id);
        }
    }
    if let Some(pb::invocation_context::DataView::BlobId(id)) = &ctx.invocation_context.data {
        sink.push_id(id);
    }
}
