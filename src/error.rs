//! crate 级结构化错误(AGENTS §5: 错误即数据)。
//!
//! 相对"每模块一个 enum"的取舍: 本 crate 是单一二进制,全部错误汇入同
//! 一个显示点(CLI stderr / TUI 状态栏),没有任何调用方按模块边界区分
//! 错误——逐模块 enum 只会产生一摞 transparent 包装。因此采用单一 enum,
//! 以**变体**为单位携带操作语义与结构化字段: 上层用字段与判别方法
//! (`is_cancelled` / `is_snapshot_torn`)做决策,不解析 Display 文本;
//! Display 只负责渲染给人看。

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ---- 基础设施 ----
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Cancelled(#[from] crate::progress::Cancelled),
    #[error(transparent)]
    Id(#[from] crate::types::IdError),
    /// 终端/通用 I/O(带路径的文件操作用专门变体)。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 静态操作上下文(替代 anyhow 的 `.context`;整链渲染见 [`Error::render`])。
    #[error("{op}")]
    Ctx { op: &'static str, source: Box<Error> },

    // ---- 定位/打开 ----
    #[error("未找到 Cursor 数据库,请用 --db 指定 state.vscdb 路径")]
    DbNotFound,
    #[error("数据库不存在: {path}")]
    DbMissing { path: PathBuf },
    #[error("打开数据库失败: {path}")]
    OpenDb { path: PathBuf, source: rusqlite::Error },
    #[error("打开 workspace 库失败: {path}")]
    OpenWorkspaceDb { path: PathBuf, source: rusqlite::Error },

    // ---- 写安全门 ----
    #[error(
        "-wal 文件 {age_secs} 秒前刚被写入,Cursor 很可能正在运行。\n\
         请完全退出 Cursor(不只是关窗口)后重试。"
    )]
    CursorRunning { age_secs: u64 },
    #[error("数据库正被其它连接占用(Cursor 可能正在运行)。请完全退出 Cursor 后重试。")]
    DbBusy,

    // ---- 目标解析(用户输入) ----
    #[error("请指定会话 id(report/TUI 里可以查到)")]
    NoTargets,
    #[error("找不到会话: {arg}(先用 report/TUI 确认 id;孤儿数据请用 sweep)")]
    TargetNotFound { arg: String },
    #[error("前缀 {arg} 匹配到多个会话: {}", candidates.join(", "))]
    AmbiguousPrefix { arg: String, candidates: Vec<String> },
    #[error("拒绝删除: 删除后将不剩任何主代理会话(如确实需要,请分批并至少保留一个主代理)")]
    RefuseWipeAll,
    #[error("headers 里出现非法 composerId: {raw:?}")]
    BadHeaderId { raw: String, source: crate::types::IdError },

    // ---- 清理安全阀 ----
    #[error("存活会话集合为空,拒绝{action}(库不对或已损坏?)")]
    EmptyLiveSet { action: &'static str },
    #[error(
        "根解码失败率 {:.2}% 超过阈值 {:.0}%,数据可能损坏,拒绝清理",
        .rate * 100.0,
        .limit * 100.0
    )]
    RootErrorRate { rate: f64, limit: f64 },
    #[error("没有成功解码任何会话根,拒绝清理")]
    NoDecodedRoots,

    // ---- 备份/回滚 ----
    #[error("备份文件已存在: {path}")]
    BackupExists { path: PathBuf },
    #[error("备份文件不存在: {path}")]
    BackupMissing { path: PathBuf },
    #[error("备份路径不是合法 UTF-8: {path}")]
    BackupPathNotUtf8 { path: PathBuf },
    #[error("创建备份库失败: {path}")]
    CreateBackup { path: PathBuf, source: rusqlite::Error },

    // ---- 其它带上下文的操作 ----
    #[error("解析 JSON 失败({what})")]
    Json { what: &'static str, source: serde_json::Error },
    #[error("删除文件失败: {path}")]
    RemoveFile { path: PathBuf, source: std::io::Error },
}

impl Error {
    /// 是否由主动取消引发。两种形态: 循环检查点返回的 [`Cancelled`],
    /// 以及连接级 progress handler 打断正在执行的 SQL 产生的
    /// `SQLITE_INTERRUPT`。文案与后续处理都与真实故障不同。
    pub fn is_cancelled(&self) -> bool {
        match self {
            Error::Cancelled(_) => true,
            Error::Ctx { source, .. } => source.is_cancelled(),
            _ => self.sqlite_code() == Some(rusqlite::ErrorCode::OperationInterrupted),
        }
    }

    /// immutable 快照被 Cursor 的 checkpoint 撕裂(见 db.rs 模块文档)。
    /// 这不是真损坏,重建连接重试即可。
    pub fn is_snapshot_torn(&self) -> bool {
        match self {
            Error::Ctx { source, .. } => source.is_snapshot_torn(),
            _ => self.sqlite_code() == Some(rusqlite::ErrorCode::DatabaseCorrupt),
        }
    }

    fn sqlite_code(&self) -> Option<rusqlite::ErrorCode> {
        let e = match self {
            Error::Sqlite(e) => e,
            Error::OpenDb { source, .. }
            | Error::OpenWorkspaceDb { source, .. }
            | Error::CreateBackup { source, .. } => source,
            _ => return None,
        };
        e.sqlite_error_code()
    }

    /// 面向人的整链渲染(等价 anyhow 的 `{:#}`): 自身 Display 后追加
    /// source 链,`": "` 相接。
    pub fn render(&self) -> String {
        let mut s = self.to_string();
        let mut src = std::error::Error::source(self);
        while let Some(e) = src {
            s.push_str(": ");
            s.push_str(&e.to_string());
            src = e.source();
        }
        s
    }
}

/// `.ctx("操作")`: 给任何可转换为 [`Error`] 的错误加一层静态操作标签。
/// 动态上下文(路径、id)不走这里——用携带字段的专门变体。
pub trait Ctx<T> {
    fn ctx(self, op: &'static str) -> Result<T>;
}

impl<T, E: Into<Error>> Ctx<T> for std::result::Result<T, E> {
    fn ctx(self, op: &'static str) -> Result<T> {
        self.map_err(|e| Error::Ctx { op, source: Box::new(e.into()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_joins_source_chain() {
        let inner = Error::DbBusy;
        let e = Error::Ctx { op: "预检", source: Box::new(inner) };
        assert_eq!(e.render(), format!("预检: {}", Error::DbBusy));
    }

    #[test]
    fn cancellation_is_detected_through_ctx() {
        let e = Error::Ctx {
            op: "扫描",
            source: Box::new(Error::Cancelled(crate::progress::Cancelled)),
        };
        assert!(e.is_cancelled());
        assert!(!Error::DbBusy.is_cancelled());
    }
}
