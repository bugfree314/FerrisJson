use FerrisJson::{
    document::{Document, LoadProgress},
    engine::{
        budget::{BudgetManager, Limits, Resource},
        session::SessionStatus,
    },
    export,
};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

struct Input(PathBuf);
impl Input {
    fn new(text: &str) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "FerrisJson-S1-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, text).unwrap();
        Self(path)
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn budget() -> BudgetManager {
    BudgetManager::new(Limits {
        memory: 32 * 1024 * 1024,
        disk: 1024 * 1024,
    })
}

#[test]
fn both_adapters_page_display_order_and_reject_stale_references() {
    // Shared key IDs and lookup sorting must not reorder a later object's fields.
    let text = r#"{"first":{"a":1,"z":2},"second":{"z":3,"a":4,"z":5},"items":[0,{"x":1},2]}"#;
    let input = Input::new(text);
    let manager = budget();
    let memory = Document::parse_owned_recover_with_budget(text.into(), manager.clone())
        .map_err(|(error, _)| error)
        .unwrap();
    let indexed = Document::open_indexed_with_budget(
        &input.0,
        || false,
        &LoadProgress::default(),
        manager.clone(),
    )
    .unwrap();
    for doc in [&memory, &indexed] {
        assert_eq!(doc.status(), SessionStatus::Ready);
        let second = doc.locate("$.second").unwrap();
        let first = doc.children_page(second, 0, 1).unwrap();
        assert_eq!(first.next, Some(1));
        assert_eq!(first.total, Some(2));
        assert_eq!(
            doc.path_ref(first.children[0].reference).unwrap(),
            "$.second.z"
        );
        assert_eq!(doc.preview_ref(first.children[0].reference).unwrap(), "5");
        let last = doc.children_page(second, first.next.unwrap(), 1).unwrap();
        assert_eq!(last.next, None);
        assert_eq!(
            doc.path_ref(last.children[0].reference).unwrap(),
            "$.second.a"
        );
        assert!(doc.children_page(second, 2, 1).unwrap().children.is_empty());
        assert!(doc.children_page(second, 3, 1).is_err());
        assert!(doc.children_page(second, 0, 0).is_err());
        assert!(doc.children_page(second, 0, 4097).is_err());
    }
    let old = memory.locate("$.items[1]").unwrap();
    assert!(
        indexed
            .metadata(old)
            .unwrap_err()
            .contains("different document session")
    );
    assert!(indexed.preview_ref(old).is_err());
    assert!(indexed.children_page(old, 0, 1).is_err());
    drop(memory);
    drop(indexed);
    assert_eq!(manager.usage().memory, 0);
    assert_eq!(manager.usage().disk, 0);
}

#[test]
fn retained_results_and_subtrees_keep_budget_after_document_drops() {
    let input = Input::new(r#"{"items":[1,2,3]}"#);
    let manager = budget();
    let doc = Document::open_indexed_with_budget(
        &input.0,
        || false,
        &LoadProgress::default(),
        manager.clone(),
    )
    .unwrap();
    let value = doc.read_subtree(doc.find_path("$.items").unwrap()).unwrap();
    let results = Arc::new(doc.query_path("$.items[*]").unwrap());
    let retained = results.clone();
    drop(results);
    drop(doc);
    assert_eq!(manager.usage().disk, 0);
    assert!(manager.usage().memory > 0);
    assert_eq!(value.as_array().unwrap().len(), 3);
    drop(value);
    assert_eq!(manager.usage().memory, retained.capacity() * 4);
    drop(retained);
    assert_eq!(manager.usage().memory, 0);
}

#[test]
fn quota_failures_are_explicit_and_do_not_leak_or_replace_output() {
    let text = "[1,2,3]";
    let input = Input::new(text);
    let limited = BudgetManager::new(Limits { memory: 0, disk: 0 });
    assert!(Document::parse_owned_recover_with_budget(text.into(), limited.clone()).is_err());
    assert!(
        Document::open_indexed_with_budget(
            &input.0,
            || false,
            &LoadProgress::default(),
            limited.clone()
        )
        .err()
        .unwrap()
        .contains("Disk budget")
    );
    assert_eq!(limited.usage().memory, 0);
    assert_eq!(limited.usage().disk, 0);

    let manager = budget();
    let doc = Document::parse_owned_recover_with_budget(text.into(), manager.clone())
        .map_err(|(error, _)| error)
        .unwrap();
    let used = manager.usage().memory;
    let hold = manager
        .reserve(
            Resource::Memory,
            32 * 1024 * 1024 - used,
            "exhaust remaining budget",
        )
        .unwrap();
    assert!(doc.children_page(doc.node_ref(0).unwrap(), 0, 3).is_err());
    assert!(doc.query_path("$[*]").is_err());
    assert!(doc.visible(&[true; 4]).is_err());
    assert_eq!(manager.usage().memory, 32 * 1024 * 1024);
    drop(hold);
    assert_eq!(doc.query_path("$[*]").unwrap(), vec![1, 2, 3]);

    let output = Input::new("original destination");
    let export_budget = BudgetManager::new(Limits {
        memory: 128 * 1024,
        disk: 2,
    });
    assert!(
        export::atomic_write_with_budget(
            &output.0,
            &AtomicBool::new(false),
            &export_budget,
            |writer| doc.write_node(writer, 0)
        )
        .unwrap_err()
        .contains("Disk budget")
    );
    assert_eq!(
        fs::read_to_string(&output.0).unwrap(),
        "original destination"
    );
    assert_eq!(export_budget.usage().disk, 0);
    assert_eq!(export_budget.usage().memory, 0);
    drop(doc);
    assert_eq!(manager.usage().memory, 0);
}

#[test]
fn indexed_parallel_previews_use_the_snapshot_after_original_replacement() {
    use FerrisJson::engine::file_source::{Cancellation, Priority, with_io_context};
    let input =
        Input::new(r#"{"one":"한국어","two":[1,2,3],"huge":123456789012345678901234567890}"#);
    let manager = budget();
    let doc = Arc::new(
        Document::open_indexed_with_budget(
            &input.0,
            || false,
            &LoadProgress::default(),
            manager.clone(),
        )
        .unwrap(),
    );
    let one = doc.locate("$.one").unwrap();
    let two = doc.locate("$.two").unwrap();
    fs::write(&input.0, r#"{"replaced":true}"#).unwrap();
    let jobs: Vec<_> = (0..8)
        .map(|index| {
            let doc = Arc::clone(&doc);
            std::thread::spawn(move || {
                with_io_context(
                    Priority::Interactive,
                    Cancellation::Flag(Arc::new(AtomicBool::new(false))),
                    || {
                        let (reference, expected) = if index % 2 == 0 {
                            (one, "\"한국어\"")
                        } else {
                            (two, "[\n  1,\n  2,\n  3\n]")
                        };
                        assert_eq!(doc.preview_ref(reference).unwrap(), expected);
                    },
                )
            })
        })
        .collect();
    for job in jobs {
        job.join().unwrap();
    }
    with_io_context(
        Priority::Interactive,
        Cancellation::Flag(Arc::new(AtomicBool::new(true))),
        || assert!(doc.preview_ref(one).is_err()),
    );
    drop(doc);
    assert_eq!(manager.usage().disk, 0);
    assert_eq!(manager.usage().memory, 0);
}
