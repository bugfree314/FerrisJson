use super::*;
use crate::document::Document;
use crate::engine::budget::Limits;
use std::{
    cell::Cell,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
pub(super) struct Input(pub(super) PathBuf);
impl Input {
    pub(super) fn new(bytes: impl AsRef<[u8]>) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "FerrisJson-S3-test-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, bytes).unwrap();
        Self(path)
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
pub(super) fn budget() -> BudgetManager {
    BudgetManager::new(Limits {
        memory: 1024 * 1024,
        disk: 32 * 1024 * 1024,
    })
}
fn open(input: &Input, budget: &BudgetManager) -> SparseIndex {
    SparseIndex::open(&input.0, budget.clone(), &|| false).unwrap()
}
pub(super) fn raw(index: &mut SparseIndex, node: Node) -> String {
    let mut bytes = Vec::new();
    index.write_raw(node, &mut bytes, &|| false).unwrap();
    String::from_utf8(bytes).unwrap()
}
fn compare(index: &mut SparseIndex, legacy: &Document, pointer: &str) {
    let node = index.pointer(pointer, &|| false).unwrap();
    let a: serde_json::Value = serde_json::from_str(&raw(index, node)).unwrap();
    let old = legacy
        .locate(if pointer.is_empty() { "$" } else { pointer })
        .unwrap();
    let b: serde_json::Value = serde_json::from_str(&legacy.preview_ref(old).unwrap()).unwrap();
    assert_eq!(a, b, "pointer {pointer}");
}
#[test]
fn decoded_keys_duplicates_order_and_both_old_backends_agree() {
    let text = r#"{"z":0,"items":[0,{"x":1,"x":2},2],"a":3,"\u007a":{"last":4},"~k/":{"\ud83d\ude00":184467440737095516151},"":true}"#;
    let input = Input::new(text);
    let manager = budget();
    let mut index = open(&input, &manager);
    let memory = Document::parse_owned_recover_with_budget(text.into(), BudgetManager::default())
        .map_err(|(e, _)| e)
        .unwrap();
    let old_file = Document::open_indexed_with_budget(
        &input.0,
        || false,
        &LoadProgress::default(),
        BudgetManager::default(),
    )
    .unwrap();
    for legacy in [&memory, &old_file] {
        for pointer in [
            "/z",
            "/z/last",
            "/items/0",
            "/items/1/x",
            "/items/2",
            "/a",
            "/~0k~1/😀",
            "/",
        ] {
            compare(&mut index, legacy, pointer);
        }
    }
    let root = index.root();
    assert_eq!(root.child_count(), 5);
    let page = index.children(root, 0, 2, &|| false).unwrap();
    assert_eq!(page.next, Some(2));
    assert_eq!(raw(&mut index, page.nodes[0]), r#"{"last":4}"#);
    let page = index.children(root, 2, 10, &|| false).unwrap();
    assert_eq!(page.nodes.len(), 3);
    assert_eq!(raw(&mut index, page.nodes[0]), "3");
    assert!(index.pointer("/items/01", &|| false).is_err());
    assert!(index.pointer("/bad~2", &|| false).is_err());
}
#[test]
fn scalar_and_empty_root_cases() {
    for text in [
        " null \n",
        "true",
        "false",
        "-0.123e+99",
        "184467440737095516151",
        r#""😀\ud83d\ude00\n""#,
        "{}",
        "[]",
    ] {
        let input = Input::new(text);
        let manager = budget();
        let mut index = open(&input, &manager);
        let root = index.root();
        let actual: serde_json::Value = serde_json::from_str(&raw(&mut index, root)).unwrap();
        assert_eq!(
            actual,
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
        if root.kind().container() {
            assert!(
                index
                    .children(root, 0, 1, &|| false)
                    .unwrap()
                    .nodes
                    .is_empty()
            );
        }
        drop(index);
        assert_eq!(manager.usage().memory, 0);
        assert_eq!(manager.usage().disk, 0);
    }
}
#[test]
fn strict_errors_and_cross_block_utf8_and_surrogates() {
    for text in [
        "",
        "[1,]",
        "{\"x\":1,}",
        "01",
        "1.",
        "1e+",
        "--1",
        "[truefalse]",
        "{}null",
        "[",
        "{\"x\" 1}",
        r#""\ud800""#,
        r#""\udc00""#,
        r#""\x""#,
        "\"\n\"",
    ] {
        let input = Input::new(text);
        let manager = budget();
        assert!(
            SparseIndex::open(&input.0, manager.clone(), &|| false).is_err(),
            "{text}"
        );
        assert_eq!(manager.usage().memory, 0);
        assert_eq!(manager.usage().disk, 0);
    }
    let input = Input::new([b'"', 0xf0, 0x80, 0x80, 0x80, b'"']);
    assert!(SparseIndex::open(&input.0, budget(), &|| false).is_err());
    let text = format!("[\"{}😀\\ud83d\\ude00\",0]", "x".repeat(65532));
    let input = Input::new(&text);
    let manager = budget();
    let mut index = open(&input, &manager);
    let root = index.root();
    let child = index.child(root, 0, &|| false).unwrap();
    assert_eq!(raw(&mut index, child), text[1..text.len() - 3]);
}
#[test]
fn array_checkpoints_resume_first_middle_last_and_nested_containers() {
    let text = format!(
        "[{}]",
        (0..4097)
            .map(|i| format!("[{{\"n\":{i}}}]"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let input = Input::new(&text);
    let manager = budget();
    let mut index = open(&input, &manager);
    let legacy = Document::parse_owned_recover_with_budget(text, BudgetManager::default())
        .map_err(|(e, _)| e)
        .unwrap();
    for ordinal in [0, 1023, 1024, 2048, 4096] {
        compare(&mut index, &legacy, &format!("/{ordinal}/0/n"));
    }
    let root = index.root();
    let page = index.children(root, 1022, 4, &|| false).unwrap();
    assert_eq!(page.nodes.len(), 4);
    assert_eq!(page.next, Some(1026));
    assert!(index.child(root, 4097, &|| false).is_err());
    assert!(index.children(root, 4098, 1, &|| false).is_err());
    assert!(index.children(root, 0, 257, &|| false).is_err());
}
#[test]
fn scalar_arrays_do_not_allocate_a_record_per_element() {
    let mut peaks = Vec::new();
    for count in [1000, 100_000] {
        let input = Input::new(format!("[{}]", vec!["0"; count].join(",")));
        let manager = budget();
        let mut index = open(&input, &manager);
        assert!(index.index_bytes() < 1_100_000);
        assert_eq!(index.root().child_count(), count as u64);
        let node = index
            .child(index.root(), count as u64 - 1, &|| false)
            .unwrap();
        assert_eq!(raw(&mut index, node), "0");
        peaks.push(manager.usage().peak_memory);
        drop(index);
        assert_eq!(manager.usage().memory, 0);
    }
    assert_eq!(peaks[0], peaks[1]);
}
#[test]
fn huge_values_and_keys_stream_without_resident_key_table() {
    let key = "键".repeat(400_000);
    let text = format!(
        "{{\"{key}\":1,\"{key}\":2,\"array\":[\"{}\",7]}}",
        "x".repeat(2 * 1024 * 1024)
    );
    let input = Input::new(text);
    let manager = budget();
    let mut index = open(&input, &manager);
    assert_eq!(index.root().child_count(), 2);
    let node = index.child(index.root(), 0, &|| false).unwrap();
    assert_eq!(raw(&mut index, node), "2");
    let last = index.pointer("/array/1", &|| false).unwrap();
    assert_eq!(raw(&mut index, last), "7");
    assert!(index.field(index.root(), &key, &|| false).is_err());
    assert!(manager.usage().peak_memory < 512 * 1024);
}
#[test]
fn wide_unique_keys_stay_on_disk_and_duplicates_keep_first_position() {
    let mut text = format!(
        "{{{}",
        (0..10_000)
            .map(|n| format!("\"key{n}\":{n}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    text.push_str(",\"key0\":99}");
    let input = Input::new(text);
    let manager = budget();
    let mut index = open(&input, &manager);
    assert_eq!(index.root().child_count(), 10_000);
    let last = index.pointer("/key9999", &|| false).unwrap();
    assert_eq!(raw(&mut index, last), "9999");
    let first = index.child(index.root(), 0, &|| false).unwrap();
    assert_eq!(raw(&mut index, first), "99");
    assert!(manager.usage().peak_memory < 512 * 1024);
}
#[test]
fn depth_is_explicit_and_resource_limited() {
    for depth in [127, 128, 5000] {
        let input = Input::new(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
        let manager = budget();
        let result = SparseIndex::open(&input.0, manager.clone(), &|| false);
        assert_eq!(result.is_ok(), depth == 127);
        drop(result);
        assert_eq!(manager.usage().memory, 0);
        assert_eq!(manager.usage().disk, 0);
    }
}
#[test]
fn quota_and_cancellation_failures_release_snapshot_and_index() {
    let input = Input::new(format!("[{}]", vec!["0"; 10_000].join(",")));
    for limits in [
        Limits {
            memory: 1,
            disk: 32 * 1024 * 1024,
        },
        Limits {
            memory: 1024 * 1024,
            disk: 500_000,
        },
    ] {
        let manager = BudgetManager::new(limits);
        assert!(SparseIndex::open(&input.0, manager.clone(), &|| false).is_err());
        assert_eq!(manager.usage().memory, 0);
        assert_eq!(manager.usage().disk, 0);
    }
    let manager = budget();
    let calls = Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        calls.get() > 200
    };
    assert!(
        SparseIndex::open(&input.0, manager.clone(), &cancel)
            .err()
            .unwrap()
            .contains("cancel")
    );
    assert_eq!(manager.usage().memory, 0);
    assert_eq!(manager.usage().disk, 0);
    let mut index = open(&input, &manager);
    let calls = Cell::new(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        calls.get() > 40
    };
    assert!(
        index
            .child(index.root(), 1000, &cancel)
            .err()
            .unwrap()
            .contains("cancel")
    );
}
#[test]
fn disk_quota_exhaustion_during_directory_growth_is_explicit() {
    let input = Input::new(format!(
        "{{{}}}",
        (0..5000)
            .map(|n| format!("\"k{n}\":{n}"))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let manager = BudgetManager::new(Limits {
        memory: 1024 * 1024,
        disk: 1_300_000,
    });
    assert!(
        SparseIndex::open(&input.0, manager.clone(), &|| false)
            .err()
            .unwrap()
            .contains("Disk budget")
    );
    assert_eq!(manager.usage().memory, 0);
    assert_eq!(manager.usage().disk, 0);
}
#[test]
fn stale_nodes_and_corrupt_headers_or_records_are_rejected() {
    let input = Input::new("[0,1]");
    let manager = budget();
    let a = open(&input, &manager);
    let mut b = open(&input, &manager);
    assert!(b.child(a.root(), 0, &|| false).is_err());
    let root = b.root();
    b.store.corrupt(root.container);
    assert!(b.child(root, 0, &|| false).is_err());
    let mut b = open(&input, &manager);
    let root = b.root();
    b.store.corrupt(0);
    assert!(b.child(root, 0, &|| false).is_err());
}
#[test]
fn page_retains_memory_charge_and_snapshot_survives_original_replacement() {
    let input = Input::new("[1,2,3]");
    let manager = budget();
    let mut index = open(&input, &manager);
    fs::rename(&input.0, input.0.with_extension("old")).unwrap();
    fs::write(&input.0, "[9]").unwrap();
    let page = index.children(index.root(), 0, 3, &|| false).unwrap();
    assert_eq!(raw(&mut index, page.nodes[2]), "3");
    drop(index);
    assert_eq!(manager.usage().disk, 0);
    assert_eq!(
        manager.usage().memory,
        page.nodes.capacity() * std::mem::size_of::<Node>()
    );
    drop(page);
    assert_eq!(manager.usage().memory, 0);
    fs::remove_file(input.0.with_extension("old")).unwrap();
}

#[test]
fn generated_nested_pages_match_legacy_navigation() {
    fn random(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *seed >> 32
    }
    fn generate(seed: &mut u64, depth: usize) -> String {
        let choice = random(seed) % if depth == 0 { 4 } else { 6 };
        match choice {
            0 => "null".into(),
            1 => random(seed).is_multiple_of(2).to_string(),
            2 => format!("-{}.125e+2", random(seed)),
            3 => serde_json::to_string(&format!("😀中\n{}", random(seed))).unwrap(),
            4 => {
                let count = random(seed) % 5;
                format!(
                    "[{}]",
                    (0..count)
                        .map(|_| generate(seed, depth - 1))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            _ => {
                let count = random(seed) % 5;
                format!(
                    "{{{}}}",
                    (0..count)
                        .map(|_| {
                            let key = ["a", "中", "~/", ""][random(seed) as usize % 4];
                            format!(
                                "{}:{}",
                                serde_json::to_string(key).unwrap(),
                                generate(seed, depth - 1)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
        }
    }
    fn walk(
        index: &mut SparseIndex,
        legacy: &Document,
        node: Node,
        expected: &serde_json::Value,
        pointer: &str,
    ) {
        compare(index, legacy, pointer);
        let children: Vec<(&serde_json::Value, String)> = match expected {
            serde_json::Value::Object(values) => values
                .iter()
                .map(|(key, value)| {
                    (
                        value,
                        format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    )
                })
                .collect(),
            serde_json::Value::Array(values) => values
                .iter()
                .enumerate()
                .map(|(i, value)| (value, format!("{pointer}/{i}")))
                .collect(),
            _ => return,
        };
        assert_eq!(node.count, children.len() as u64);
        if children.is_empty() {
            return;
        }
        let page = index.children(node, 0, 256, &|| false).unwrap();
        for (node, (expected, pointer)) in page.nodes.iter().zip(children) {
            walk(index, legacy, *node, expected, &pointer);
        }
    }
    let mut seed = 42;
    for _ in 0..64 {
        let text = generate(&mut seed, 4);
        let input = Input::new(&text);
        let manager = budget();
        let mut index = open(&input, &manager);
        let legacy =
            Document::parse_owned_recover_with_budget(text.clone(), BudgetManager::default())
                .map_err(|(e, _)| e)
                .unwrap();
        let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
        let root = index.root();
        walk(&mut index, &legacy, root, &expected, "");
    }
}

#[test]
fn corrupt_checkpoint_links_and_object_ordinals_are_rejected() {
    let input = Input::new("[0,1]");
    let manager = budget();
    let mut index = open(&input, &manager);
    let root = index.root();
    let container = index.store.get(root.container, &|| false).unwrap();
    let offset = container[5];
    let mut checkpoint = index.store.get(offset, &|| false).unwrap();
    checkpoint[4] = 99;
    index.store.put(offset, checkpoint, &|| false).unwrap();
    assert!(index.child(root, 0, &|| false).is_err());
    let input = Input::new(r#"{"a":1,"b":2}"#);
    let mut index = open(&input, &manager);
    let root = index.root();
    let container = index.store.get(root.container, &|| false).unwrap();
    let mut field = index.store.get(container[5], &|| false).unwrap();
    field[6] = 99;
    index.store.put(container[5], field, &|| false).unwrap();
    assert!(index.children(root, 0, 2, &|| false).is_err());
}
