use super::*;
use crate::engine::{budget::Limits, sparse::SparseIndex, tree::PagedTree};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Data(PathBuf);
impl Data {
    fn new(text: &str) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s6-tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!(
            "input-{}-{}.tmp",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&p, text).unwrap();
        Self(p)
    }
}
impl Drop for Data {
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
fn inputs(text: &str, b: &BudgetManager) -> (Data, Vec<Input>) {
    let data = Data::new(text);
    let m = Document::parse_owned_recover_with_budget(text.into(), b.clone())
        .map_err(|(e, _)| e)
        .unwrap();
    let i = Document::open_indexed_with_budget(
        &data.0,
        || false,
        &crate::document::LoadProgress::default(),
        b.clone(),
    )
    .unwrap();
    let s = SparseIndex::open(&data.0, b.clone(), &|| false).unwrap();
    (
        data,
        vec![
            Input::Legacy(Arc::new(m)),
            Input::Legacy(Arc::new(i)),
            Input::Sparse(Box::new(s)),
        ],
    )
}
fn paths(mut input: Input, q: Query) -> (Vec<String>, Status) {
    let provider = input.duplicate().unwrap().provider().unwrap();
    let store = input.run(q, 1, &|| false, &mut |_| Ok(())).unwrap();
    let state = store.state();
    let mut tree = PagedTree::new(provider, &|| false).unwrap();
    let paths = (0..state.count)
        .map(|i| {
            tree.path(&store.location(i, &|| false).unwrap(), 8192, true, &|| {
                false
            })
            .unwrap()
        })
        .collect();
    (paths, state.status)
}
#[test]
fn three_backends_keep_order_scopes_unicode_duplicates_and_one_hit_per_node() {
    let text = r#"{"k":"old","k":{"x":"KKσ😀","quote":"a\"b"},"a":["0",0,true,null],"n":12345678901234567890,"slash/~":"\u0061"}"#;
    for (term, case, regex, scope) in [
        ("k", false, false, SearchScope::All),
        ("σ", false, false, SearchScope::Values),
        ("0", true, false, SearchScope::All),
        ("^.*$", true, true, SearchScope::Keys),
        ("a\"b", true, false, SearchScope::Values),
        ("old", true, false, SearchScope::Values),
        ("a", true, false, SearchScope::Values),
        ("", false, false, SearchScope::All),
    ] {
        let b = budget();
        let (_d, inputs) = inputs(text, &b);
        let mut results = Vec::new();
        for input in inputs {
            let r = paths(
                input,
                Query {
                    term: term.into(),
                    case,
                    regex,
                    scope,
                },
            );
            assert_eq!(r.1, Status::Completed);
            results.push(r.0);
        }
        assert_eq!(results[0], results[1], "{term}");
        assert_eq!(results[0], results[2], "{term}");
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
}
#[test]
fn streaming_literal_crosses_blocks_and_regex_failure_keeps_committed_prefix() {
    let mut text = String::from("[\"HIT\",\"");
    text.extend(std::iter::repeat_n('x', 64 * 1024 - 2));
    text.push_str("\\u004b😀END");
    text.extend(std::iter::repeat_n('x', 300 * 1024));
    text.push_str("\"]");
    let b = budget();
    let (_d, ins) = inputs(&text, &b);
    for input in ins {
        let r = paths(
            input,
            Query {
                term: "k😀end".into(),
                case: false,
                regex: false,
                scope: SearchScope::Values,
            },
        );
        assert_eq!(r.0, vec!["$[1]"]);
    }
    let d = Data::new(&text);
    let s = SparseIndex::open(&d.0, b.clone(), &|| false).unwrap();
    let mut input = Input::Sparse(Box::new(s));
    let store = input
        .run(
            Query {
                term: "HIT".into(),
                case: true,
                regex: true,
                scope: SearchScope::Values,
            },
            2,
            &|| false,
            &mut |_| Ok(()),
        )
        .unwrap();
    assert_eq!(store.state().status, Status::Failed);
    assert_eq!(store.state().count, 1);
    assert!(store.state().error.contains("256 KiB"));
}
#[test]
fn shared_index_reader_keeps_disk_charge_and_matches_after_original_closes() {
    let d = Data::new("[1,{\"x\":2}]");
    let b = budget();
    let index = SparseIndex::open(&d.0, b.clone(), &|| false).unwrap();
    let before = b.usage().disk;
    let view = index.read_view().unwrap();
    assert_eq!(b.usage().disk, before);
    drop(index);
    assert_eq!(b.usage().disk, before);
    let mut input = Input::Sparse(Box::new(view));
    let r = input
        .run(Query::literal("2"), 1, &|| false, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(r.state().count, 1);
    drop(r);
    drop(input);
    assert_eq!(b.usage().disk, 0);
    assert_eq!(b.usage().memory, 0);
}
#[test]
fn million_hits_have_bounded_memory_and_random_result_pages() {
    let text = format!(
        "[{}]",
        std::iter::repeat_n("0", 1_000_000)
            .collect::<Vec<_>>()
            .join(",")
    );
    let d = Data::new(&text);
    drop(text);
    let b = budget();
    let index = SparseIndex::open(&d.0, b.clone(), &|| false).unwrap();
    let mut input = Input::Sparse(Box::new(index));
    let mut early = false;
    let store = input
        .run(
            Query {
                term: "0".into(),
                case: true,
                regex: false,
                scope: SearchScope::Values,
            },
            4,
            &|| false,
            &mut |e| {
                if let Event::Started(ref s) = e {
                    assert_eq!(s.state().count, 0);
                }
                if let Event::Batch { start, count, .. } = e
                    && start == 0
                {
                    assert_eq!(count, 1);
                    early = true;
                }
                Ok(())
            },
        )
        .unwrap();
    assert!(early);
    assert_eq!(store.state().count, 1_000_000);
    assert_eq!(store.state().ancestors, 1);
    for i in [0, 127, 128, 500_000, 999_999] {
        let loc = store.location(i, &|| false).unwrap();
        assert_eq!(loc.ancestors[0].ordinal, i);
    }
    assert!(b.usage().peak_memory < 32 * 1024 * 1024);
    drop(store);
    drop(input);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn cancellation_after_first_batch_preserves_one_visible_result() {
    let b = budget();
    let (_d, mut inputs) = inputs("[1,1,1,1]", &b);
    let mut input = inputs.pop().unwrap();
    let stop = std::cell::Cell::new(false);
    let store = input
        .run(Query::literal("1"), 3, &|| stop.get(), &mut |e| {
            if matches!(e, Event::Batch { .. }) {
                stop.set(true);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(store.state().status, Status::Cancelled);
    assert_eq!(store.state().count, 1);
    assert!(store.location(0, &|| false).is_ok());
}
#[test]
fn number_spelling_matches_old_semantics() {
    let text = "[-0,1e+2,1E2,1.00,123456789012345678901234567890]";
    for term in ["-0", "e+2", "E2", "1.00", "12345678901234567890"] {
        let b = budget();
        let (_d, ins) = inputs(text, &b);
        let mut found = Vec::new();
        for input in ins {
            found.push(
                paths(
                    input,
                    Query {
                        term: term.into(),
                        case: true,
                        regex: false,
                        scope: SearchScope::Values,
                    },
                )
                .0,
            );
        }
        assert_eq!(found[0], found[1], "{term}");
        assert_eq!(found[0], found[2], "{term}");
    }
}

#[test]
fn cursor_export_complete_partial_failed_and_cancelled_is_atomic() {
    use std::sync::atomic::AtomicBool;
    let b = budget();
    let (_d, ins) = inputs(r#"{"x":["yes",{"yes":2}],"no":false}"#, &b);
    for mut input in ins {
        let store = input
            .run(Query::literal("yes"), 7, &|| false, &mut |_| Ok(()))
            .unwrap();
        let dest = Data::new("old");
        output::export(
            input.duplicate().unwrap(),
            &store,
            &dest.0,
            true,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&dest.0).unwrap(),
            serde_json::to_string(&vec![r#"$["x"][0]"#, r#"$["x"][1]["yes"]"#]).unwrap()
        );
        output::export(
            input.duplicate().unwrap(),
            &store,
            &dest.0,
            false,
            Some(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&dest.0).unwrap(), r#"["yes"]"#);
        assert!(
            output::export(
                input.duplicate().unwrap(),
                &store,
                &dest.0,
                false,
                None,
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&dest.0).unwrap(), r#"["yes"]"#);
        store.fail("test failure".into());
        assert!(
            output::export(
                input.duplicate().unwrap(),
                &store,
                &dest.0,
                false,
                None,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        output::export(
            input.duplicate().unwrap(),
            &store,
            &dest.0,
            false,
            Some(2),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&dest.0).unwrap(), r#"["yes",2]"#);
    }
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}

#[test]
fn compatibility_collector_rejects_over_limit_without_all_hit_allocation() {
    let text = format!(
        "[{}]",
        std::iter::repeat_n("0", 65_537)
            .collect::<Vec<_>>()
            .join(",")
    );
    let b = budget();
    let doc = Document::parse_owned_recover_with_budget(text, b.clone())
        .map_err(|(e, _)| e)
        .unwrap();
    assert!(
        doc.search("0", true, false, SearchScope::Values)
            .err()
            .unwrap()
            .contains("65536")
    );
}

#[test]
fn invalid_regex_and_disk_quota_release_results() {
    let d = Data::new("[1,1,1,1]");
    let b = BudgetManager::new(Limits {
        memory: 32 * 1024 * 1024,
        disk: 2 * 1024 * 1024,
    });
    let index = SparseIndex::open(&d.0, b.clone(), &|| false).unwrap();
    let root = index.root();
    let node = TreeNode::from_sparse(root, 5);
    let (store, mut writer) = ResultStore::new(index.session_id(), 9, b.clone()).unwrap();
    for _ in 0..30_000 {
        if writer.hit(node, u64::MAX, 0, &|| false).is_err() {
            break;
        }
    }
    writer.finish(Status::Failed, 0, "quota".into()).unwrap();
    assert!(store.state().count > 0 && store.state().count < 30_000);
    drop(writer);
    drop(store);
    let mut input = Input::Sparse(Box::new(index));
    assert!(
        input
            .run(
                Query {
                    term: "[".into(),
                    regex: true,
                    case: true,
                    scope: SearchScope::All
                },
                10,
                &|| false,
                &mut |_| Ok(())
            )
            .is_err()
    );
    drop(input);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
