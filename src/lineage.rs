//! 会话父子归属模型。
//!
//! **主代理 = 用户手动 new 的会话,即没有父链接的会话**;子代理即使自己
//! 还有子代理也仍是子代理,全部后代递归挂在唯一的主代理下。
//!
//! 父子链接有两种(逆向报告 3.3 节),children 索引同时覆盖:
//! - `subagentInfo.parentComposerId`(真子代理,`isSubagent=1`);
//! - Best-of-N: 父方 `subComposerIds` / 子方 `isBestOfNSubcomposer`
//!   (官方 isSubagent 判定明确排除它)。
//!
//! 孤儿判定保守收窄: 只有**真子代理**的父链断裂才判 [`Attach::Dangling`]
//! (清扫会连 header 一起删);Best-of-N 与旧 `task-` 头(isSubagent 但
//! parent 未知)一律保留,只在 UI 标注。

use rustc_hash::FxHashMap;

use crate::headers::SessionHeader;

/// 会话在父子图中的归属分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// 主代理: 无父链接,用户手动创建。
    Main,
    /// 父链可达某个被保留的祖先(主代理或保守保留的节点)。
    Attached,
    /// 真子代理且父链断裂(父不存在,或父自身是孤儿): 可清扫。
    Dangling,
    /// 无法归属但保守保留: isSubagent 却无父链接(旧 task- 头)、
    /// 父链成环、或非真子代理挂在断裂/孤儿的父链上。
    Unattributable,
}

pub struct Lineage {
    /// 父 id → 存在 header 的子 id 列表(两种链接方式合并,已去重)。
    children: FxHashMap<String, Vec<String>>,
    attach: FxHashMap<String, Attach>,
}

impl Lineage {
    pub fn build(sessions: &[SessionHeader]) -> Self {
        let by_id: FxHashMap<&str, &SessionHeader> =
            sessions.iter().map(|s| (s.composer_id.as_str(), s)).collect();

        // 子 id → 父 id。子方声明优先,父方 subComposerIds 补缺。
        let mut parent_of: FxHashMap<&str, &str> = FxHashMap::default();
        for s in sessions {
            if let Some(p) = &s.parent_composer_id {
                parent_of.insert(s.composer_id.as_str(), p.as_str());
            }
        }
        for s in sessions {
            for child in &s.sub_composer_ids {
                parent_of.entry(child.as_str()).or_insert(s.composer_id.as_str());
            }
        }

        // children 只收录真实存在的子会话(级联删除/树显示的对象);
        // 不存在的子 id 没有 header,其数据行走孤儿行清扫。
        let mut children: FxHashMap<String, Vec<String>> = FxHashMap::default();
        for (child, parent) in &parent_of {
            if by_id.contains_key(child) {
                children.entry((*parent).to_owned()).or_default().push((*child).to_owned());
            }
        }
        for list in children.values_mut() {
            list.sort();
            list.dedup();
        }

        // 逐会话沿父链爬升分类,带记忆化;visited 防环。
        let mut attach: FxHashMap<String, Attach> = FxHashMap::default();
        for s in sessions {
            classify(s.composer_id.as_str(), &by_id, &parent_of, &mut attach);
        }

        Lineage { children, attach }
    }

    pub fn attach(&self, id: &str) -> Attach {
        self.attach.get(id).copied().unwrap_or(Attach::Main)
    }

    /// 孤儿子代理(父链断裂的真子代理)。
    pub fn is_dangling(&self, id: &str) -> bool {
        self.attach(id) == Attach::Dangling
    }

    /// 全部孤儿子代理 id(排序稳定,供清扫/报表)。
    pub fn dangling_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .attach
            .iter()
            .filter(|(_, a)| **a == Attach::Dangling)
            .map(|(id, _)| id.clone())
            .collect();
        out.sort();
        out
    }

    /// 直接子会话(仅含存在 header 的)。
    pub fn children_of(&self, id: &str) -> &[String] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// `id` 的全部后代(不含自身),BFS 序,防环。
    pub fn descendants_of(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
        seen.insert(id);
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        queue.push_back(id);
        while let Some(cur) = queue.pop_front() {
            for child in self.children_of(cur) {
                if seen.insert(child.as_str()) {
                    out.push(child.clone());
                    queue.push_back(child.as_str());
                }
            }
        }
        out
    }
}

/// 链顶的三种收束方式(决定回填的起点与基准)。
enum Top {
    /// 链顶节点(path 末元素)自身就是结论: 无父链接,或父不存在。
    OwnResult(Attach),
    /// 结论来自 path 之上某个已记忆化的节点,path 全体按子级派生。
    FromAbove(Attach),
    /// 成环: `path[起点..]` 是环成员,全部保守保留。
    Cycle(usize),
}

/// 沿父链分类一个会话(迭代实现,顺带把路径上所有节点记忆化)。
fn classify(
    id: &str,
    by_id: &FxHashMap<&str, &SessionHeader>,
    parent_of: &FxHashMap<&str, &str>,
    memo: &mut FxHashMap<String, Attach>,
) {
    if memo.contains_key(id) {
        return;
    }
    // 自底向上收集未分类的链。
    let mut path: Vec<&str> = Vec::new();
    let mut cur = id;
    let top = loop {
        if let Some(a) = memo.get(cur) {
            break Top::FromAbove(*a);
        }
        if let Some(pos) = path.iter().position(|n| *n == cur) {
            break Top::Cycle(pos);
        }
        path.push(cur);
        let header = by_id[cur]; // 只有存在的会话会入链
        match parent_of.get(cur) {
            // 链顶: 无父链接。isSubagent 却不知父是谁(旧 task- 头)保守保留。
            None => {
                break Top::OwnResult(if header.is_subagent {
                    Attach::Unattributable
                } else {
                    Attach::Main
                });
            }
            Some(parent) if by_id.contains_key(parent) => cur = parent,
            // 父不存在: 真子代理判孤儿,其余保守保留。
            Some(_) => {
                break Top::OwnResult(if header.is_subagent {
                    Attach::Dangling
                } else {
                    Attach::Unattributable
                });
            }
        }
    };

    // 自顶向下回填: 先落定链顶(或环段),再逐级派生其下节点。
    let (start, mut above) = match top {
        Top::OwnResult(a) => {
            let last = path.len() - 1;
            memo.insert(path[last].to_owned(), a);
            (last, a)
        }
        Top::FromAbove(a) => (path.len(), a),
        Top::Cycle(pos) => {
            for node in &path[pos..] {
                memo.insert((*node).to_owned(), Attach::Unattributable);
            }
            (pos, Attach::Unattributable)
        }
    };
    for i in (0..start).rev() {
        let this = derive_child(above, by_id[path[i]]);
        memo.insert(path[i].to_owned(), this);
        above = this;
    }
}

/// 父的分类 → 子的分类。
fn derive_child(parent: Attach, child: &SessionHeader) -> Attach {
    match parent {
        // 父被保留(主代理/挂靠/保守保留): 子挂靠其下
        Attach::Main | Attach::Attached | Attach::Unattributable => Attach::Attached,
        // 父是孤儿: 真子代理随之孤儿,其余保守保留
        Attach::Dangling => {
            if child.is_subagent {
                Attach::Dangling
            } else {
                Attach::Unattributable
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, parent: Option<&str>, is_subagent: bool) -> SessionHeader {
        SessionHeader {
            composer_id: id.into(),
            name: None,
            created_at: None,
            last_updated_at: None,
            recency: 0,
            is_archived: false,
            is_subagent,
            is_best_of_n: false,
            workspace_id: None,
            workspace_folder: None,
            parent_composer_id: parent.map(str::to_owned),
            sub_composer_ids: Vec::new(),
            in_header_table: true,
            in_legacy_blob: false,
        }
    }

    #[test]
    fn main_attached_and_descendants() {
        // main ← sub1 ← sub2(孙);另一个 main2 无子
        let sessions = vec![
            session("main", None, false),
            session("sub1", Some("main"), true),
            session("sub2", Some("sub1"), true),
            session("main2", None, false),
        ];
        let l = Lineage::build(&sessions);
        assert_eq!(l.attach("main"), Attach::Main);
        assert_eq!(l.attach("sub1"), Attach::Attached);
        assert_eq!(l.attach("sub2"), Attach::Attached, "子代理的子代理仍挂靠,不是主代理");
        assert_eq!(l.attach("main2"), Attach::Main);
        assert_eq!(l.descendants_of("main"), vec!["sub1", "sub2"]);
        assert!(l.descendants_of("main2").is_empty());
        assert!(l.dangling_ids().is_empty());
    }

    #[test]
    fn dangling_chain_and_conservative_edges() {
        let sessions = vec![
            // 父不存在的真子代理: 孤儿;它的真子代理后代也孤儿
            session("d1", Some("ghost"), true),
            session("d2", Some("d1"), true),
            // 孤儿链上的非真子代理: 保守保留
            session("bn", Some("d1"), false),
            // isSubagent 但 parent 未知(旧 task- 头): 保守保留
            session("task-old", None, true),
            // 挂在保留节点下的子代理: 挂靠
            session("sub", Some("task-old"), true),
        ];
        let l = Lineage::build(&sessions);
        assert_eq!(l.attach("d1"), Attach::Dangling);
        assert_eq!(l.attach("d2"), Attach::Dangling);
        assert_eq!(l.attach("bn"), Attach::Unattributable);
        assert_eq!(l.attach("task-old"), Attach::Unattributable);
        assert_eq!(l.attach("sub"), Attach::Attached);
        assert_eq!(l.dangling_ids(), vec!["d1", "d2"]);
    }

    #[test]
    fn cycle_is_kept_conservatively() {
        let sessions = vec![
            session("a", Some("b"), true),
            session("b", Some("a"), true),
            session("c", Some("a"), true), // 挂在环上
        ];
        let l = Lineage::build(&sessions);
        assert_eq!(l.attach("a"), Attach::Unattributable);
        assert_eq!(l.attach("b"), Attach::Unattributable);
        assert_eq!(l.attach("c"), Attach::Attached);
        assert!(l.dangling_ids().is_empty());
    }

    #[test]
    fn best_of_n_links_via_parent_side() {
        // Best-of-N: 父方 subComposerIds 声明,子方无 parentComposerId
        let mut parent = session("main", None, false);
        parent.sub_composer_ids = vec!["bn1".into(), "bn2".into(), "missing".into()];
        let mut bn1 = session("bn1", None, false);
        bn1.is_best_of_n = true;
        let mut bn2 = session("bn2", None, false);
        bn2.is_best_of_n = true;
        let sessions = vec![parent, bn1, bn2];
        let l = Lineage::build(&sessions);
        assert_eq!(l.attach("bn1"), Attach::Attached);
        assert_eq!(l.attach("bn2"), Attach::Attached);
        assert_eq!(l.children_of("main"), ["bn1", "bn2"], "不存在的子 id 不进 children");
        assert_eq!(l.descendants_of("main"), vec!["bn1", "bn2"]);
    }

    #[test]
    fn live_set_excludes_dangling() {
        let sessions = vec![
            session("main", None, false),
            session("sub", Some("main"), true),
            session("dangling", Some("ghost"), true),
        ];
        let live = crate::headers::live_set(&sessions).unwrap();
        assert!(live.contains("main"));
        assert!(live.contains("sub"));
        assert!(!live.contains("dangling"), "孤儿子代理不算存活");
    }
}
