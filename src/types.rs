//! 领域类型: 一次解析,携带证据。
//!
//! 这些 newtype 让"已验证/已归一化"成为类型事实——值一旦通过构造器,
//! 下游不再重复校验或归一化;同时把分配点收敛到构造器,调用方对成本可见。

use std::borrow::Borrow;
use std::fmt;

// 全仓库统一 FxHash: 哈希键全部来自本地数据库(自己的数据),
// 没有 HashDoS 攻击面,SipHash 的抗碰撞性纯属浪费吞吐。
use rustc_hash::FxHashSet;

#[derive(Debug, thiserror::Error)]
pub enum IdError {
    #[error("id 为空")]
    Empty,
    #[error("id 太短(至少 {min} 位): {raw}")]
    TooShort { raw: Box<str>, min: usize },
    #[error("id 含非法字符(需 ASCII 且不含 ':'): {raw}")]
    InvalidChars { raw: Box<str> },
}

/// 会话 id(composerId)。
///
/// 构造即证明: 非空、ASCII、不含 `:`(cursorDiskKV key 编码的分隔符)。
/// 因此可以安全地拼进 key 前缀、与 `attribute()` 的 owner 段直接比较。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComposerId(Box<str>);

impl ComposerId {
    /// 从不可信输入建立契约。
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        if raw.is_empty() {
            return Err(IdError::Empty);
        }
        if !raw.is_ascii() || raw.contains(':') {
            return Err(IdError::InvalidChars { raw: raw.into() });
        }
        Ok(Self(raw.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 列表/日志用的 8 位短前缀。
    pub fn short(&self) -> &str {
        &self.0[..8.min(self.0.len())]
    }
}

impl Borrow<str> for ComposerId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComposerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 存活会话集合(两套 header 来源的并集,存活判定的唯一依据)。
///
/// 语义化包装的意义: 全部调用方共享同一构造路径,
/// `contains` 直接接受 `attribute()` 给出的 owner 切片,无需分配。
#[derive(Debug, Default)]
pub struct LiveSet(FxHashSet<ComposerId>);

impl LiveSet {
    pub fn from_ids(ids: impl IntoIterator<Item = ComposerId>) -> Self {
        Self(ids.into_iter().collect())
    }

    pub fn contains(&self, owner: &str) -> bool {
        self.0.contains(owner)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ComposerId> {
        self.0.iter()
    }
}

/// blob id 的小写 hex 形态(`agentKv:blob:` 的 key 后缀,mark 集合的元素)。
///
/// 构造即归一: trim + 小写(与官方解码语义一致),
/// 此后比较/哈希直接按字节,不再有任何一处重复 `to_ascii_lowercase`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHex(Box<str>);

impl BlobHex {
    /// 从原始 blobId 字节编码。**mark 热路径的唯一堆分配点之一**;
    /// 查重请先用 [`BlobHex::encode_view`] 的零分配视图。
    pub fn from_bytes(id: &[u8]) -> Self {
        Self(hex_encode(id).into_boxed_str())
    }

    /// 从外部 hex 字符串归一化(checkpoint 指针值、cloudAgent 元数据)。
    /// 空串返回 None。
    pub fn parse(raw: &str) -> Option<Self> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        Some(Self(t.to_ascii_lowercase().into_boxed_str()))
    }

    /// 栈上编码出零分配的查重视图。`buf` 需 ≥ `id.len()*2`
    /// (实际 blobId 恒为 32 字节,128 字节栈缓冲足够);超长返回 None,
    /// 调用方退回堆编码路径。
    pub fn encode_view<'b>(id: &[u8], buf: &'b mut [u8; 128]) -> Option<&'b str> {
        let n = id.len().checked_mul(2)?;
        if n > buf.len() {
            return None;
        }
        // faster_hex 只在 dst 过短时报错,上面已检查
        faster_hex::hex_encode(id, &mut buf[..n]).ok().map(|s| &*s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for BlobHex {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 输出恒为小写 hex(与官方 key 编码一致)。
pub fn hex_encode(bytes: &[u8]) -> String {
    faster_hex::hex_string(bytes)
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    hex_decode_into(s, &mut out).then_some(out)
}

/// 解码进复用缓冲区。官方解码器语义: 先 trim,大小写同收。
pub fn hex_decode_into(s: &str, out: &mut Vec<u8>) -> bool {
    let b = s.trim().as_bytes();
    if !b.len().is_multiple_of(2) {
        return false;
    }
    out.clear();
    out.resize(b.len() / 2, 0);
    faster_hex::hex_decode(b, out).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_id_contract() {
        assert!(ComposerId::parse("").is_err());
        assert!(ComposerId::parse("has:colon").is_err());
        assert!(ComposerId::parse("中文").is_err());
        let id = ComposerId::parse("task-abc-123").unwrap();
        assert_eq!(id.short(), "task-abc");
    }

    #[test]
    fn blob_hex_normalizes_once() {
        assert_eq!(BlobHex::parse(" AABB ").unwrap().as_str(), "aabb");
        assert!(BlobHex::parse("  ").is_none());
        assert_eq!(BlobHex::from_bytes(&[0xAB, 0x01]).as_str(), "ab01");

        let mut buf = [0u8; 128];
        assert_eq!(BlobHex::encode_view(&[0xAB, 0x01], &mut buf), Some("ab01"));
        assert_eq!(BlobHex::encode_view(&[0u8; 65], &mut buf), None);
    }
}
