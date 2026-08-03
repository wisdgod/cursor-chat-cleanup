//! workspaceId → 人类可读标签/路径的解析。
//!
//! 归属数据只存在于 header(`composerHeaders.workspaceId` 列与 header JSON
//! 的 `workspaceIdentifier`),孤儿数据因此不可归属。路径解析优先用 header
//! 自带的 `workspaceIdentifier.uri`(零额外 I/O),缺失时读
//! `workspaceStorage/<id>/workspace.json` 兜底;两者都没有的是临时窗口
//! (实测: 纯数字时间戳 id 与 `empty-window` 的目录里没有 workspace.json)。

use std::path::Path;

use rustc_hash::FxHashMap;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    /// 列表列/过滤用的短标签: 项目路径末段,或临时窗口的特判文案。
    pub label: String,
    /// 完整 URI / 路径(详情页显示)。None 表示解析不到路径。
    pub folder: Option<String>,
}

/// 解析 `sessions` 涉及的全部 workspaceId。
///
/// 优先级: 任一 header 自带的 `workspaceIdentifier.uri` →
/// `workspaceStorage/<id>/workspace.json` → 特判文案 → id 短前缀。
pub fn resolve(
    sessions: &[crate::headers::SessionHeader],
    db_path: &Path,
) -> FxHashMap<String, WorkspaceInfo> {
    let mut map: FxHashMap<String, WorkspaceInfo> = FxHashMap::default();

    for s in sessions {
        let (Some(wid), Some(folder)) = (&s.workspace_id, &s.workspace_folder) else {
            continue;
        };
        map.entry(wid.clone()).or_insert_with(|| from_folder(folder));
    }

    // globalStorage/state.vscdb 的上上级就是 User 目录(与 delete::workspace_dbs 同一推导)
    let ws_root = db_path
        .parent()
        .and_then(Path::parent)
        .map(|user| user.join("workspaceStorage"));
    for s in sessions {
        let Some(wid) = &s.workspace_id else { continue };
        if map.contains_key(wid.as_str()) {
            continue;
        }
        let info = ws_root
            .as_ref()
            .and_then(|root| folder_from_json(&root.join(wid).join("workspace.json")))
            .map(|f| from_folder(&f))
            .unwrap_or_else(|| fallback(wid));
        map.insert(wid.clone(), info);
    }
    map
}

fn from_folder(folder: &str) -> WorkspaceInfo {
    WorkspaceInfo { label: label_of(folder), folder: Some(folder.to_owned()) }
}

/// 路径末段(percent 解码后)作短标签。
fn label_of(folder: &str) -> String {
    let seg = folder
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(folder);
    percent_decode(seg)
}

/// 没有任何路径信息时的标签。
fn fallback(wid: &str) -> WorkspaceInfo {
    let label = if wid == "empty-window" {
        "(无工作区)".to_owned()
    } else if !wid.is_empty() && wid.bytes().all(|b| b.is_ascii_digit()) {
        format!("(临时窗口 {wid})")
    } else {
        format!("({}…)", wid.chars().take(8).collect::<String>())
    };
    WorkspaceInfo { label, folder: None }
}

/// `workspaceStorage/<id>/workspace.json`: 单目录工作区是 `folder`,
/// 多根工作区是 `workspace`(指向 .code-workspace 文件)。
fn folder_from_json(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    ["folder", "workspace"]
        .iter()
        .find_map(|k| json.get(k).and_then(Value::as_str))
        .map(str::to_owned)
}

/// 最小 percent 解码(仅为标签显示;非法序列原样保留)。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_takes_last_segment_decoded() {
        assert_eq!(
            label_of("vscode-remote://wsl%2Bdebian/home/wisd/repos/cursor-chat-cleanup"),
            "cursor-chat-cleanup"
        );
        assert_eq!(label_of("file:///c%3A/My%20Project/"), "My Project");
        assert_eq!(label_of("plain"), "plain");
    }

    #[test]
    fn fallback_labels() {
        assert_eq!(fallback("empty-window").label, "(无工作区)");
        assert_eq!(fallback("1785751935302").label, "(临时窗口 1785751935302)");
        assert_eq!(fallback("ec906c28db45012cbcdadbace50901a9").label, "(ec906c28…)");
    }

    #[test]
    fn percent_decode_keeps_invalid_sequences() {
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    /// 端到端: header 自带路径优先,其次 workspace.json,最后特判。
    #[test]
    fn resolve_priority() {
        let root = std::env::temp_dir().join(format!("ccc-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let global = root.join("User/globalStorage/state.vscdb");
        let ws_dir = root.join("User/workspaceStorage/abc123");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(
            ws_dir.join("workspace.json"),
            r#"{"folder": "vscode-remote://wsl%2Bdebian/tmp/from-json"}"#,
        )
        .unwrap();

        let mk = |wid: &str, folder: Option<&str>| crate::headers::SessionHeader {
            composer_id: format!("c-{wid}"),
            name: None,
            created_at: None,
            last_updated_at: None,
            recency: 0,
            is_archived: false,
            is_subagent: false,
            workspace_id: Some(wid.to_owned()),
            workspace_folder: folder.map(str::to_owned),
            parent_composer_id: None,
            in_header_table: true,
            in_legacy_blob: false,
        };
        let sessions = vec![
            mk("hdr", Some("file:///home/x/proj")), // header 路径优先
            mk("abc123", None),                     // 走 workspace.json
            mk("empty-window", None),               // 特判
        ];
        let map = resolve(&sessions, &global);
        assert_eq!(map["hdr"].label, "proj");
        assert_eq!(map["abc123"].label, "from-json");
        assert_eq!(map["abc123"].folder.as_deref(), Some("vscode-remote://wsl%2Bdebian/tmp/from-json"));
        assert_eq!(map["empty-window"].label, "(无工作区)");

        let _ = std::fs::remove_dir_all(&root);
    }
}
