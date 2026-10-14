use super::*;
use crate::engine::budget::Limits;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Input(PathBuf);
impl Input {
    fn new(text: &str) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s5-tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!(
            "tree-{}-{}.tmp",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&p, text).unwrap();
        Self(p)
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn budget() -> BudgetManager {
    BudgetManager::new(Limits {
        memory: 256 * 1024 * 1024,
        disk: 1024 * 1024 * 1024,
    })
}
fn trees(text: &str) -> (Input, Vec<PagedTree>) {
    let input = Input::new(text);
    let b = budget();
    let memory = Document::parse_owned_recover_with_budget(text.into(), b.clone())
        .map_err(|(e, _)| e)
        .unwrap();
    let file = Document::open_indexed_with_budget(
        &input.0,
        || false,
        &crate::document::LoadProgress::default(),
        b.clone(),
    )
    .unwrap();
    let sparse = SparseIndex::open(&input.0, b.clone(), &|| false).unwrap();
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(LegacyProvider(Arc::new(memory))),
        Box::new(LegacyProvider(Arc::new(file))),
        Box::new(SparseProvider::new(sparse).unwrap()),
    ];
    (
        input,
        providers
            .into_iter()
            .map(|p| PagedTree::new(p, &|| false).unwrap())
            .collect(),
    )
}
fn rows(tree: &mut PagedTree) -> Vec<(String, &'static str, u64, bool)> {
    let total = tree.total(&|| false).unwrap().0;
    assert!(total < 1000);
    (0..total)
        .map(|rank| {
            let loc = tree.row(rank, &|| false).unwrap();
            assert_eq!(tree.rank(&loc, &|| false).unwrap(), rank);
            let path = tree.path(&loc, 8192, true, &|| false).unwrap();
            (
                path,
                loc.node.kind,
                loc.node.children,
                tree.expanded(loc.node, &|| false).unwrap(),
            )
        })
        .collect()
}
fn agree(trees: &mut [PagedTree]) {
    let expected = rows(&mut trees[0]);
    for tree in &mut trees[1..] {
        assert_eq!(rows(tree), expected);
    }
}
#[test]
fn three_backends_agree_on_mixed_expansion_full_rows_duplicates_and_reveal() {
    let (_input, mut t) = trees(
        r#"{"a":[0,{"y":[1,2]},3],"duplicate":{"old":[1,2,3]},"duplicate":{"new":4},"~/😀":{"":true},"z":null}"#,
    );
    agree(&mut t);
    for pointer in ["/a", "/a/1", "/a/1/y", "/duplicate", "/~0~1😀"] {
        for tree in &mut t {
            let loc = tree.provider.locate(pointer, &|| false).unwrap();
            let loc = tree.reveal(&loc, &|| false).unwrap();
            tree.set_expanded(&loc, true, &|| false).unwrap();
        }
        agree(&mut t);
    }
    for tree in &mut t {
        tree.reset(true, &|| false).unwrap();
    }
    agree(&mut t);
    assert_eq!(t[0].total(&|| false).unwrap().0, 13);
    for tree in &mut t {
        let loc = tree.provider.locate("/a", &|| false).unwrap();
        tree.set_expanded(&loc, false, &|| false).unwrap();
    }
    agree(&mut t);
    for tree in &mut t {
        let loc = tree.provider.locate("/a/1/y/1", &|| false).unwrap();
        let loc = tree.reveal(&loc, &|| false).unwrap();
        assert_eq!(
            tree.path(&loc, 1024, true, &|| false).unwrap(),
            "$[\"a\"][1][\"y\"][1]"
        );
    }
    agree(&mut t);
    for tree in &mut t {
        tree.reset(false, &|| false).unwrap();
        assert_eq!(tree.total(&|| false).unwrap().0, 1);
    }
}
#[test]
fn page_window_is_bounded_and_rank_jumps_do_not_materialize_global_rows() {
    let text = format!(
        "[{}]",
        (0..20000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let input = Input::new(&text);
    let b = budget();
    let index = SparseIndex::open(&input.0, b.clone(), &|| false).unwrap();
    let mut tree =
        PagedTree::new(Box::new(SparseProvider::new(index).unwrap()), &|| false).unwrap();
    tree.reset(true, &|| false).unwrap();
    let last = tree.row(20000, &|| false).unwrap();
    assert_eq!(tree.path(&last, 128, true, &|| false).unwrap(), "$[19999]");
    let frame = tree.frame(19950, 64, &last, &|| false).unwrap();
    assert_eq!(frame.rows.len(), 51);
    assert_eq!(frame.selected_rank, 20000);
    assert_eq!(frame.total, 20001);
    assert!(tree.state_bytes() < 1100000);
    drop(frame);
    drop(tree);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn expansion_cancellation_rolls_back_disk_state_and_old_view_remains_correct() {
    let (_input, mut trees) = trees(r#"{"a":[{"b":[1,2]},3],"c":4}"#);
    for t in &mut trees {
        let loc = t.provider.locate("/a/0/b", &|| false).unwrap();
        t.reveal(&loc, &|| false).unwrap();
        let before = rows(t);
        let calls = std::cell::Cell::new(0);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 40
        };
        let result = t.set_expanded(&loc, true, &cancel);
        assert!(result.is_err());
        assert_eq!(rows(t), before);
        t.set_expanded(&loc, true, &|| false).unwrap();
    }
    agree(&mut trees);
}
#[test]
fn search_reference_bridge_reveals_legacy_matches_and_paths_are_never_silently_truncated() {
    let (_input, mut t) = trees(r#"{"long name":{"x":[1,2]},"a\"/😀":3}"#);
    for tree in &mut t {
        let loc = tree.provider.locate("/long name/x/1", &|| false).unwrap();
        if matches!(loc.node.token, Token::Legacy(_)) {
            let revealed = tree.reveal_ref(loc.node.reference, &|| false).unwrap();
            assert_eq!(
                tree.path(&revealed, 1024, true, &|| false).unwrap(),
                "$[\"long name\"][\"x\"][1]"
            );
        }
        assert!(tree.path(&loc, 4, true, &|| false).is_err());
        assert!(tree.path(&loc, 4, false, &|| false).unwrap().ends_with('…'));
    }
}

#[test]
fn navigation_history_viewport_and_pending_end_match_across_backends() {
    use crate::engine::tree_navigation::{Navigation, Navigator};
    let (_input, trees) = trees(r#"{"a":[{"x":1},2],"b":[3,4],"c":5}"#);
    for tree in trees {
        let mut nav = Navigator::new(tree.provider, &|| false).unwrap();
        nav.apply(Navigation::ViewportRows(3), &|| false).unwrap();
        nav.apply(Navigation::Right(1), &|| false).unwrap();
        nav.apply(Navigation::Right(1), &|| false).unwrap();
        nav.apply(Navigation::Right(2), &|| false).unwrap();
        assert_eq!(
            nav.tree.path(&nav.selected, 1024, true, &|| false).unwrap(),
            "$[\"a\"][0]"
        );
        nav.apply(Navigation::History(true), &|| false).unwrap();
        assert_eq!(
            nav.tree.path(&nav.selected, 1024, true, &|| false).unwrap(),
            "$[\"a\"]"
        );
        nav.apply(Navigation::ExpandAll, &|| false).unwrap();
        nav.apply(Navigation::Last, &|| false).unwrap();
        assert_eq!(
            nav.tree.path(&nav.selected, 1024, true, &|| false).unwrap(),
            "$[\"c\"]"
        );
        for _ in 0..100 {
            nav.apply(Navigation::Move(-1), &|| false).unwrap();
            nav.apply(Navigation::Move(1), &|| false).unwrap();
        }
        assert!(nav.history_len() <= 64);
        nav.apply(Navigation::CollapseAll, &|| false).unwrap();
        assert_eq!(nav.tree.total(&|| false).unwrap().0, 1);
    }
}
struct Virtual {
    session: crate::engine::session::SessionId,
    budget: BudgetManager,
    count: u64,
}
impl Provider for Virtual {
    fn budget(&self) -> BudgetManager {
        self.budget.clone()
    }
    fn root(&mut self, _: &dyn Fn() -> bool) -> Result<TreeNode, String> {
        Ok(TreeNode {
            reference: NodeRef {
                session: self.session,
                address: 0,
            },
            kind: "array",
            children: self.count,
            full_rows: self.count + 1,
            token: Token::Legacy(0),
        })
    }
    fn child(
        &mut self,
        _: TreeNode,
        ordinal: u64,
        _: &dyn Fn() -> bool,
    ) -> Result<TreeNode, String> {
        if ordinal >= self.count {
            return Err("out of range".into());
        }
        Ok(TreeNode {
            reference: NodeRef {
                session: self.session,
                address: ordinal + 1,
            },
            kind: "number",
            children: 0,
            full_rows: 1,
            token: Token::Legacy(0),
        })
    }
    fn prefix(&mut self, _: TreeNode, ordinal: u64, _: &dyn Fn() -> bool) -> Result<u64, String> {
        Ok(ordinal)
    }
    fn key(
        &mut self,
        _: TreeNode,
        ordinal: u64,
        _: usize,
        _: &dyn Fn() -> bool,
    ) -> Result<(String, bool), String> {
        Ok((ordinal.to_string(), false))
    }
    fn preview(
        &mut self,
        node: TreeNode,
        _: usize,
        _: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        Ok(node.reference.address.to_string())
    }
    fn output(&mut self, _: TreeNode, _: &dyn Fn() -> bool) -> Result<OutputSelection, String> {
        Err("virtual fixture".into())
    }
}
#[test]
fn integer_row_positions_above_floating_point_precision_are_exact() {
    let b = budget();
    let count = (1u64 << 56) + 123;
    let provider = Virtual {
        session: Document::parse("[]").unwrap().session_id(),
        budget: b.clone(),
        count,
    };
    let mut tree = PagedTree::new(Box::new(provider), &|| false).unwrap();
    tree.reset(true, &|| false).unwrap();
    let loc = tree.row(count - 7, &|| false).unwrap();
    assert_eq!(loc.node.reference.address, count - 7);
    assert_eq!(tree.rank(&loc, &|| false).unwrap(), count - 7);
    assert_eq!(
        tree.frame(count - 12, 64, &loc, &|| false)
            .unwrap()
            .rows
            .len(),
        13
    );
    drop(tree);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn wide_object_far_ordinals_and_weighted_state_match_memory() {
    let mut text = String::from("{");
    for n in 0..2048 {
        if n > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"k{n}\":[{n},{{\"x\":{n}}}]"));
    }
    text.push('}');
    let (_input, mut t) = trees(&text);
    for tree in &mut t {
        tree.reset(true, &|| false).unwrap();
        assert_eq!(tree.total(&|| false).unwrap().0, 8193);
        let last = tree.row(8192, &|| false).unwrap();
        assert_eq!(
            tree.path(&last, 1024, true, &|| false).unwrap(),
            "$[\"k2047\"][1][\"x\"]"
        );
        for n in [0, 1, 1023, 2047] {
            let loc = tree.provider.locate(&format!("/k{n}"), &|| false).unwrap();
            tree.set_expanded(&loc, false, &|| false).unwrap();
        }
        assert_eq!(tree.total(&|| false).unwrap().0, 8181);
    }
    for rank in [0, 1, 2, 100, 5000, 8180] {
        let expected = t[0].row(rank, &|| false).unwrap();
        let expected = t[0].path(&expected, 1024, true, &|| false).unwrap();
        for tree in &mut t[1..] {
            let loc = tree.row(rank, &|| false).unwrap();
            assert_eq!(tree.rank(&loc, &|| false).unwrap(), rank);
            assert_eq!(tree.path(&loc, 1024, true, &|| false).unwrap(), expected);
        }
    }
}
