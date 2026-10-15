use super::*;
use crate::{
    document::Document,
    engine::{
        budget::{BudgetManager, Limits},
        sparse::SparseIndex,
        tree::PagedTree,
    },
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
struct Data(PathBuf);
impl Data {
    fn new(text: &str) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s7-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "{}-{}.tmp",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        Self(path)
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn budget() -> BudgetManager {
    BudgetManager::new(Limits {
        memory: 256 * 1024 * 1024,
        disk: 1024 * 1024 * 1024,
    })
}
fn make_inputs(text: &str, b: &BudgetManager) -> (Data, Vec<Input>) {
    let data = Data::new(text);
    let memory = Document::parse_owned_recover_with_budget(text.into(), b.clone())
        .map_err(|(e, _)| e)
        .unwrap();
    let indexed =
        Document::open_indexed_with_budget(&data.0, || false, &Default::default(), b.clone())
            .unwrap();
    let sparse = SparseIndex::open(&data.0, b.clone(), &|| false).unwrap();
    (
        data,
        vec![
            Input::Legacy(Arc::new(memory)),
            Input::Legacy(Arc::new(indexed)),
            Input::Sparse(Box::new(sparse)),
        ],
    )
}
#[test]
fn supported_queries_match_independent_library_in_order_locations_and_duplicates() {
    let text = r#"{"limit":2,"rows":[{"n":0,"tag":null,"enabled":false},{"n":2,"enabled":true},{"n":3,"tag":"x"}],"a/b~":{"中文😀":7},"dup":0,"dup":8}"#;
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    for expression in [
        "$",
        "$.dup",
        "$.rows[1,0,1].n",
        "$..n",
        "$..['n','n']",
        "$.rows[::-1].n",
        "$.rows[-1:-4:-1].n",
        "$.rows[::0]",
        "$.rows[?(@.n>=2 && @.n<4)].n",
        "$.rows[?(@.tag)]",
        "$.rows[?(!@.missing)]",
        "$.rows[?(@.missing==null)]",
        "$.rows[?(@.missing==@.absent)]",
        "$.rows[?(@.n>=$.limit)].n",
        "$.rows[?(@.enabled==true || @.n==0)].n",
        "$..*..n",
        "$['a/b~']['中文😀']",
        "$.rows[?((@.n==0 || @.n==3) && !@.tag)]",
    ] {
        let expected = serde_json_path::JsonPath::parse(expression)
            .unwrap()
            .query_located(&value)
            .iter()
            .map(|n| n.location().to_json_pointer())
            .collect::<Vec<_>>();
        let b = budget();
        let (_data, inputs) = make_inputs(text, &b);
        for input in inputs {
            let provider = input.duplicate().unwrap().provider().unwrap();
            let mut tree = PagedTree::new(provider, &|| false).unwrap();
            let store = run(input, expression, 1, &|| false, &mut |_| Ok(())).unwrap();
            assert_eq!(
                store.state().status,
                Status::Completed,
                "{expression}: {}",
                store.state().error
            );
            let actual = (0..store.state().count)
                .map(|i| {
                    tree.path(&store.location(i, &|| false).unwrap(), 8192, true, &|| {
                        false
                    })
                    .unwrap()
                })
                .collect::<Vec<_>>();
            let wanted = expected
                .iter()
                .map(|p| {
                    let loc = tree.provider.locate(p, &|| false).unwrap();
                    tree.path(&loc, 8192, true, &|| false).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, wanted, "{expression}");
        }
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
}
#[test]
fn decimal_comparisons_are_exact_for_large_integers_and_exponents() {
    let text = "[9007199254740992,9007199254740993,1e1000,10e999,-0,0.000,-123456789012345678901234567890]";
    for (query, ordinals) in [
        ("$[?(@==9007199254740993)]", vec![1]),
        ("$[?(@==1e1000)]", vec![2, 3]),
        ("$[?(@==0)]", vec![4, 5]),
        ("$[?(@ < -123456789012345678901234567889)]", vec![6]),
    ] {
        let b = budget();
        let (_data, inputs) = make_inputs(text, &b);
        for input in inputs {
            let store = run(input, query, 2, &|| false, &mut |_| Ok(())).unwrap();
            assert_eq!(
                store.state().status,
                Status::Completed,
                "{}",
                store.state().error
            );
            assert_eq!(
                (0..store.state().count)
                    .map(|i| store.location(i, &|| false).unwrap().ancestors[0].ordinal)
                    .collect::<Vec<_>>(),
                ordinals
            );
        }
    }
}
#[test]
fn pointer_slice_extremes_and_parser_limits_are_explicit() {
    let b = budget();
    let (_d, inputs) = make_inputs(r#"{"a/b~":[0,1,2],"":4}"#, &b);
    for input in inputs {
        let r = run(input, "/a~1b~0/2", 1, &|| false, &mut |_| Ok(())).unwrap();
        assert_eq!(r.state().count, 1);
    }
    for bad in [
        "$[?length(@)>1]",
        "$[?(@[*]==1)]",
        "$['bad]",
        "$[1.5]",
        "$[?(@.n=1)]",
        "$[?true]",
        "$.",
        "/bad~2key",
    ] {
        assert!(Plan::parse(bad).is_err(), "{bad}");
    }
    assert!(Plan::parse(&format!("$[?({} @.x)]", "!".repeat(40))).is_err());
    assert!(
        Plan::parse(&format!(
            "$[?({})]",
            std::iter::repeat_n("@.x", 100)
                .collect::<Vec<_>>()
                .join(" && ")
        ))
        .is_err()
    );
    assert!(
        Plan::parse(&format!(
            "$[{}]",
            std::iter::repeat_n("0", 65).collect::<Vec<_>>().join(",")
        ))
        .is_err()
    );
    assert!(Plan::parse(&format!("$.{}", "x".repeat(65536))).is_err());
}
#[test]
fn full_memory_library_remains_available_under_compatibility_limits() {
    let text = r#"{"rows":[{"a":[1],"b":[1]},{"a":[2],"b":[3]}]}"#;
    let b = budget();
    let (_d, inputs) = make_inputs(text, &b);
    for input in inputs {
        if let Input::Legacy(doc) = input {
            let containers = doc.query_path("$.rows[?@.a==@.b]");
            let function = doc.query_path("$.rows[?length(@.a)==1]");
            if doc.value().is_some() {
                assert_eq!(containers.unwrap().len(), 1);
                assert_eq!(function.unwrap().len(), 2);
            } else {
                assert!(containers.is_err());
                assert!(function.is_err());
            }
        }
    }
    assert!(Plan::compatibility_guard("$[?length(@.*)>0]", 1000).is_err());
    let text = format!(
        "[{}]",
        std::iter::repeat_n("0", 65_537)
            .collect::<Vec<_>>()
            .join(",")
    );
    let doc = Document::parse_owned_recover_with_budget(text, b)
        .map_err(|(e, _)| e)
        .unwrap();
    assert!(doc.query_path("$[*]").err().unwrap().contains("65536"));
}
#[test]
fn million_reverse_duplicate_results_are_streamed_with_bounded_memory() {
    let text = format!(
        "[{}]",
        std::iter::repeat_n("0", 500_000)
            .collect::<Vec<_>>()
            .join(",")
    );
    let data = Data::new(&text);
    drop(text);
    let b = budget();
    let index = SparseIndex::open(&data.0, b.clone(), &|| false).unwrap();
    let mut first = false;
    let store = run(
        Input::Sparse(Box::new(index)),
        "$[::-1,::-1]",
        1,
        &|| false,
        &mut |event| {
            if let Event::Batch {
                start: 0, count, ..
            } = event
            {
                assert_eq!(count, 1);
                first = true;
            }
            Ok(())
        },
    )
    .unwrap();
    assert!(first);
    assert_eq!(
        store.state().status,
        Status::Completed,
        "{}",
        store.state().error
    );
    assert_eq!(store.state().count, 1_000_000);
    assert_eq!(store.state().ancestors, 1);
    for (hit, ordinal) in [(0, 499_999), (499_999, 0), (500_000, 499_999), (999_999, 0)] {
        assert_eq!(
            store.location(hit, &|| false).unwrap().ancestors[0].ordinal,
            ordinal
        );
    }
    assert!(b.usage().peak_memory < 32 * 1024 * 1024);
    drop(store);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn array_continuations_cross_pages_checkpoints_nested_arrays_and_random_jumps() {
    let text = format!(
        "[{}]",
        (0..2200)
            .map(|i| format!(r#"{{"n":{i},"a":[{i},{}]}}"#, i + 1))
            .collect::<Vec<_>>()
            .join(",")
    );
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    for query in [
        "$[63:1027].n",
        "$[*].a[*]",
        "$[1025,63,64,0,2199,1024,1025].n",
    ] {
        let expected = serde_json_path::JsonPath::parse(query)
            .unwrap()
            .query(&value)
            .all()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let b = budget();
        let (_d, inputs) = make_inputs(&text, &b);
        for input in inputs {
            let output = input.duplicate().unwrap();
            let result = run(input, query, 1, &|| false, &mut |_| Ok(())).unwrap();
            assert_eq!(
                result.state().status,
                Status::Completed,
                "{}",
                result.state().error
            );
            let destination = Data::new("old");
            crate::engine::search::output::export(
                output,
                &result,
                &destination.0,
                false,
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Vec<serde_json::Value>>(
                    &std::fs::read(&destination.0).unwrap()
                )
                .unwrap(),
                expected,
                "{query}"
            );
        }
        assert_eq!(b.usage().memory, 0);
        assert_eq!(b.usage().disk, 0);
    }
    let data = Data::new(&format!(
        "[{}]",
        std::iter::repeat_n("0", 40_000)
            .collect::<Vec<_>>()
            .join(",")
    ));
    let b = BudgetManager::new(Limits {
        memory: 32 * 1024 * 1024,
        disk: 2 * 1024 * 1024,
    });
    let index = SparseIndex::open(&data.0, b.clone(), &|| false).unwrap();
    let result = run(
        Input::Sparse(Box::new(index)),
        "$[*]",
        1,
        &|| false,
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.state().status, Status::Failed);
    assert!(result.state().count > 0 && result.state().count < 40_000);
    assert!(result.state().error.contains("budget"));
    drop(result);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn cancellation_partial_export_and_scalar_errors_preserve_committed_prefix() {
    let b = budget();
    let (_d, mut inputs) = make_inputs("[1,2,3]", &b);
    let input = inputs.pop().unwrap();
    let output = input.duplicate().unwrap();
    let stop = std::cell::Cell::new(false);
    let store = run(input, "$[*]", 4, &|| stop.get(), &mut |e| {
        if matches!(e, Event::Batch { .. }) {
            stop.set(true);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(store.state().status, Status::Cancelled);
    assert_eq!(store.state().count, 1);
    let destination = Data::new("old");
    assert!(
        crate::engine::search::output::export(
            output.duplicate().unwrap(),
            &store,
            &destination.0,
            false,
            None,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .is_err()
    );
    crate::engine::search::output::export(
        output,
        &store,
        &destination.0,
        false,
        Some(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&destination.0).unwrap(), "[1]");
    let huge = format!("[{{\"v\":1}},{{\"v\":\"{}\"}}]", "x".repeat(300 * 1024));
    let (_d, mut inputs) = make_inputs(&huge, &b);
    let store = run(
        inputs.pop().unwrap(),
        "$[?(@.v==1)]",
        5,
        &|| false,
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(store.state().count, 1);
    assert_eq!(store.state().status, Status::Failed);
    assert!(store.state().error.contains("256 KiB"));
}
