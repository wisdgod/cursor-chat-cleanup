//! 长操作的进度上报,CLI 行内刷新与 TUI 状态栏共用。
//!
//! 写侧(工作线程)在阶段切换时调用 [`Progress::stage`],处理中只增计数;
//! 读侧(ticker/TUI 渲染)取快照,无锁竞争(stage 锁只在切换与快照时短暂持有)。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, thiserror::Error)]
#[error("操作已取消")]
pub struct Cancelled;

#[derive(Default)]
pub struct Progress {
    stage: Mutex<String>,
    done: AtomicU64,
    total: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// 进入新阶段并清零计数。`total = 0` 表示总量未知。
    pub fn stage(&self, label: &str, total: u64) {
        let mut g = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        g.clear();
        g.push_str(label);
        drop(g);
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_done(&self, n: u64) {
        self.done.store(n, Ordering::Relaxed);
    }

    /// 操作结束,清空显示并复位取消标志(供下一次操作复用)。
    pub fn finish(&self) {
        self.stage.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.cancelled.store(false, Ordering::Relaxed);
    }

    /// 请求取消。长循环会在下一个检查点退出并返回 [`Cancelled`]。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 长循环的检查点: 已请求取消时返回 `Err(Cancelled)`。
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() { Err(Cancelled) } else { Ok(()) }
    }

    /// 渲染成单行文本;无活动阶段时返回 None。
    pub fn render(&self) -> Option<String> {
        let stage = {
            let g = self.stage.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_empty() {
                return None;
            }
            g.clone()
        };
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        Some(if total > 0 {
            const WIDTH: usize = 20;
            let done = done.min(total);
            let filled = (done as u128 * WIDTH as u128 / total as u128) as usize;
            format!(
                "{stage} [{}{}] {:>3.0}% {done}/{total}",
                "█".repeat(filled),
                "░".repeat(WIDTH - filled),
                done as f64 / total as f64 * 100.0,
            )
        } else if done > 0 {
            format!("{stage} {done}")
        } else {
            stage
        })
    }
}
