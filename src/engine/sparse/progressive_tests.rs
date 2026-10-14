use super::tests::{Input, budget, raw};
use super::*;
use crate::engine::session::SessionStatus;
use std::{cell::Cell, fs, path::PathBuf};
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let i = Input::new([]);
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/s4-tests");
        fs::create_dir_all(&base).unwrap();
        let p = base.join(i.0.file_name().unwrap()).with_extension("cache");
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn options(&self) -> OpenOptions {
        OpenOptions {
            cache_dir: Some(self.0.clone()),
            ..Default::default()
        }
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn first_page_precedes_validation_and_tail_error_invalidates_session() {
    let i = Input::new(b"[1,2,3] trailing");
    let d = Dir::new();
    let b = budget();
    let mut states = Vec::new();
    let mut page_seen = false;
    let result = open_progressive(&i.0, b.clone(), d.options(), 7, &|| false, &mut |e| {
        assert_eq!(e.generation, 7);
        if let Some(p) = e.page {
            assert_eq!(p.previews[0], "1");
            assert_eq!(p.total, None);
            page_seen = true;
        }
        states.push(e.status);
        Ok(())
    });
    assert!(result.is_err());
    assert!(page_seen);
    assert_eq!(states.last(), Some(&SessionStatus::Failed));
    assert!(!states.contains(&SessionStatus::Ready));
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn cancel_at_committed_page_then_resume_and_strictly_reuse() {
    let i = Input::new(b"[0,{\"x\":1,\"x\":2},2,3]");
    let d = Dir::new();
    let b = budget();
    let stop = Cell::new(false);
    assert!(
        open_progressive(&i.0, b.clone(), d.options(), 1, &|| stop.get(), &mut |e| {
            if e.status == SessionStatus::Validating && e.page.is_some() {
                stop.set(true);
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
    let mut resumed = false;
    let mut restored_page = false;
    let mut index = open_progressive(&i.0, b.clone(), d.options(), 2, &|| false, &mut |e| {
        resumed |= e.resumed;
        if e.status == SessionStatus::Validating
            && let Some(page) = e.page
        {
            assert_eq!(page.previews[0], "0");
            restored_page = true;
            assert!(e.committed_frontier > 0);
        }
        Ok(())
    })
    .unwrap();
    assert!(resumed);
    assert!(restored_page);
    let n = index.pointer("/1/x", &|| false).unwrap();
    assert_eq!(raw(&mut index, n), "2");
    drop(index);
    let mut reused = false;
    let index = open_progressive(&i.0, b.clone(), d.options(), 3, &|| false, &mut |e| {
        reused |= e.reused;
        Ok(())
    })
    .unwrap();
    assert!(reused);
    drop(index);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn same_size_rewrite_replacement_and_truncation_cannot_reuse() {
    let i = Input::new(b"[1,2,3]");
    let d = Dir::new();
    let b = budget();
    drop(open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |_| Ok(())).unwrap());
    for (case, bytes) in [b"[4,5,6]".as_slice(), b"[7,8,9]", b"[1]"]
        .into_iter()
        .enumerate()
    {
        let time = fs::metadata(&i.0).unwrap().modified().unwrap();
        if case == 1 {
            fs::remove_file(&i.0).unwrap();
        }
        fs::write(&i.0, bytes).unwrap();
        if case == 0 {
            fs::File::options()
                .write(true)
                .open(&i.0)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        let mut reused = false;
        let mut index = open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |e| {
            reused |= e.reused || e.resumed;
            Ok(())
        })
        .unwrap();
        assert!(!reused);
        let root = index.root();
        assert_eq!(raw(&mut index, root).as_bytes(), bytes);
    }
}
#[test]
fn object_is_not_published_until_duplicate_keys_are_final() {
    let i = Input::new(br#"{"a":1,"b":2,"a":3}"#);
    let d = Dir::new();
    let mut seen = false;
    drop(
        open_progressive(&i.0, budget(), d.options(), 0, &|| false, &mut |e| {
            if let Some(p) = e.page {
                assert_eq!(e.status, SessionStatus::Ready);
                assert_eq!(p.previews, ["3", "2"]);
                assert_eq!(p.labels, ["a", "b"]);
                seen = true;
            }
            Ok(())
        })
        .unwrap(),
    );
    assert!(seen);
}
#[test]
fn corrupted_index_manifest_and_state_safely_rebuild() {
    for file in [
        "manifest.bin",
        "state-0.bin",
        "state-1.bin",
        "index-0.bin",
        "index-1.bin",
    ] {
        let i = Input::new(b"[1,2,3]");
        let d = Dir::new();
        let b = budget();
        drop(
            open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |_| Ok(())).unwrap(),
        );
        fs::write(d.0.join(file), b"bad").unwrap();
        let mut index =
            open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |_| Ok(())).unwrap();
        let n = index.pointer("/2", &|| false).unwrap();
        assert_eq!(raw(&mut index, n), "3");
    }
}
#[test]
fn lock_excludes_other_writer_and_is_released_on_drop() {
    let d = Dir::new();
    let b = budget();
    let c = cache::Cache::open(&d.0, &b).unwrap();
    assert!(cache::Cache::open(&d.0, &b).is_err());
    drop(c);
    assert!(cache::Cache::open(&d.0, &b).is_ok());
}
#[test]
fn checkpoint_frames_and_ready_format_are_bounded_and_versioned() {
    assert!(progressive::Checkpoint::decode(b"bad").is_err());
    let i = Input::new(b"[[0],1]");
    let d = Dir::new();
    let b = budget();
    let stop = Cell::new(false);
    assert!(
        open_progressive(&i.0, b.clone(), d.options(), 0, &|| stop.get(), &mut |e| {
            if e.page.is_some() {
                stop.set(true);
            }
            Ok(())
        })
        .is_err()
    );
    let mut index = open_progressive(&i.0, b, d.options(), 0, &|| false, &mut |_| Ok(())).unwrap();
    let n = index.pointer("/0/0", &|| false).unwrap();
    assert_eq!(raw(&mut index, n), "0");
}
#[test]
fn persistent_disk_budget_failure_never_emits_ready() {
    let i = Input::new(b"[1,2,3]");
    let d = Dir::new();
    let b = BudgetManager::new(crate::engine::budget::Limits {
        memory: 1024 * 1024,
        disk: 1024 * 1024,
    });
    let mut ready = false;
    assert!(
        open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |e| {
            ready |= e.status == SessionStatus::Ready;
            Ok(())
        })
        .is_err()
    );
    assert!(!ready);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}

#[test]
fn interruption_at_each_commit_stage_keeps_previous_generation_readable() {
    for stage in 1..=5 {
        let i = Input::new(b"[0,1,2]");
        let d = Dir::new();
        let b = budget();
        drop(
            open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |_| Ok(())).unwrap(),
        );
        {
            let mut cache = cache::Cache::open(&d.0, &b).unwrap();
            let cp = cache.load(&|| false).unwrap().unwrap();
            assert!(cp.root.is_some());
            let mut store = Store::new(&b, &|| false).unwrap();
            cache.restore(&mut store, &cp, &|| false).unwrap();
            cache::FAULT.with(|f| f.set(stage));
            let result = cache.commit(&mut store, &cp, &|| false);
            cache::FAULT.with(|f| f.set(0));
            assert!(result.is_err());
        }
        let mut reused = false;
        let mut index = open_progressive(&i.0, b.clone(), d.options(), 0, &|| false, &mut |e| {
            reused |= e.reused;
            Ok(())
        })
        .unwrap();
        assert!(reused);
        let n = index.pointer("/2", &|| false).unwrap();
        assert_eq!(raw(&mut index, n), "2");
    }
}

#[test]
fn published_pages_and_clipboard_payload_keep_their_budget_after_index_drop() {
    let i = Input::new(b"[\"hello\",2]");
    let b = budget();
    let mut index = open_progressive(
        &i.0,
        b.clone(),
        OpenOptions::default(),
        0,
        &|| false,
        &mut |_| Ok(()),
    )
    .unwrap();
    let root = index.root();
    let page = index.published_children(root, 0, &|| false).unwrap();
    let copied = index.copy_value(page.nodes[0], &|| false).unwrap();
    assert_eq!(copied.text, "\"hello\"");
    drop(index);
    assert!(b.usage().memory > 0);
    drop(page);
    assert!(b.usage().memory > 0);
    drop(copied);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn cache_ownership_marker_protects_unrelated_files() {
    let d = Dir::new();
    fs::write(d.0.join("source.bin"), b"unrelated").unwrap();
    assert!(cache::Cache::open(&d.0, &budget()).is_err());
    assert_eq!(fs::read(d.0.join("source.bin")).unwrap(), b"unrelated");
}
#[test]
fn sparse_export_cancellation_keeps_existing_destination_and_success_is_complete() {
    let i = Input::new(b"[1,2,3]");
    let d = Dir::new();
    let b = budget();
    let mut index = open_progressive(&i.0, b, OpenOptions::default(), 0, &|| false, &mut |_| {
        Ok(())
    })
    .unwrap();
    let root = index.root();
    let target = d.0.join("export.tmp");
    fs::write(&target, b"old").unwrap();
    let flag = std::sync::atomic::AtomicBool::new(true);
    assert!(index.export_value(root, &target, &flag).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"old");
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    index.export_value(root, &target, &flag).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"[1,2,3]");
}

#[test]
fn immutable_live_view_can_navigate_while_builder_is_still_validating() {
    let i = Input::new(b"[0,{\"x\":1,\"x\":2},2,3]");
    let b = budget();
    let mut views = Vec::new();
    let mut ready = false;
    let index = open_progressive(
        &i.0,
        b.clone(),
        OpenOptions {
            live_views: true,
            ..Default::default()
        },
        4,
        &|| false,
        &mut |mut e| {
            ready |= e.status == SessionStatus::Ready;
            if let Some(mut view) = e.view.take() {
                assert!(!ready);
                assert!(!view.complete());
                let root = view.root();
                assert!(!view.output_selection(root, &|| false).is_ok());
                if root.child_count() > 0 {
                    let child = view.child(root, 0, &|| false).unwrap();
                    assert_eq!(view.tree_preview(child, 160, &|| false).unwrap(), "0");
                }
                views.push(view);
            }
            Ok(())
        },
    )
    .unwrap();
    let mut first = views
        .into_iter()
        .find(|v| v.root().child_count() == 1)
        .unwrap();
    assert_eq!(first.root().child_count(), 1);
    assert!(
        first
            .pointer_edges("/1", &|| false)
            .unwrap_err()
            .contains("Pending")
    );
    drop(index);
    let n = first.child(first.root(), 0, &|| false).unwrap();
    assert_eq!(first.tree_preview(n, 160, &|| false).unwrap(), "0");
    drop(first);
    assert_eq!(b.usage().memory, 0);
    assert_eq!(b.usage().disk, 0);
}
#[test]
fn output_snapshot_keeps_its_disk_charge_after_index_closes() {
    let i = Input::new(b"[1,2,3]");
    let b = budget();
    let mut index = open_progressive(
        &i.0,
        b.clone(),
        OpenOptions::default(),
        0,
        &|| false,
        &mut |_| Ok(()),
    )
    .unwrap();
    let node = index.child(index.root(), 1, &|| false).unwrap();
    let selection = index.output_selection(node, &|| false).unwrap();
    drop(index);
    assert!(b.usage().disk > 0);
    assert_eq!(selection.copy(&|| false).unwrap().text, "2");
    drop(selection);
    assert_eq!(b.usage().disk, 0);
    assert_eq!(b.usage().memory, 0);
}
#[test]
fn old_manifest_format_is_rebuilt_without_reuse() {
    let i = Input::new(b"[1,{\"x\":2}]");
    let d = Dir::new();
    let index =
        open_progressive(&i.0, budget(), d.options(), 1, &|| false, &mut |_| Ok(())).unwrap();
    drop(index);
    let path = d.0.join("manifest.bin");
    let mut bytes = fs::read(&path).unwrap();
    for entry in bytes.chunks_mut(32) {
        entry[..8].copy_from_slice(b"FJCM0001");
    }
    fs::write(path, bytes).unwrap();
    let mut reused = false;
    let mut index = open_progressive(&i.0, budget(), d.options(), 2, &|| false, &mut |e| {
        reused |= e.reused || e.resumed;
        Ok(())
    })
    .unwrap();
    assert!(!reused);
    let node = index.pointer("/1/x", &|| false).unwrap();
    assert_eq!(raw(&mut index, node), "2");
}
#[test]
fn pending_end_runs_after_ready_and_a_later_jump_cancels_it() {
    use crate::engine::{
        tree::SparseProvider,
        tree_navigation::{Navigation, Navigator},
    };
    let i = Input::new(b"[1,2,3]");
    for cancel_pending in [false, true] {
        let mut nav = None;
        let ready = open_progressive(
            &i.0,
            BudgetManager::new(crate::engine::budget::Limits {
                memory: 256 * 1024 * 1024,
                disk: 1024 * 1024 * 1024,
            }),
            OpenOptions {
                live_views: true,
                ..Default::default()
            },
            5,
            &|| false,
            &mut |mut e| {
                if nav.is_none()
                    && let Some(view) = e.view.take()
                    && view.root().child_count() == 1
                {
                    let mut n =
                        Navigator::new(Box::new(SparseProvider::new(*view).unwrap()), &|| false)
                            .unwrap();
                    assert!(
                        n.apply(Navigation::Last, &|| false)
                            .unwrap_err()
                            .contains("Pending")
                    );
                    assert!(n.pending());
                    if cancel_pending {
                        n.apply(Navigation::First, &|| false).unwrap();
                        assert!(!n.pending());
                    }
                    nav = Some(n);
                }
                Ok(())
            },
        )
        .unwrap();
        let mut nav = nav.unwrap();
        nav.replace(Box::new(SparseProvider::new(ready).unwrap()), &|| false)
            .unwrap();
        assert!(!nav.pending());
        assert_eq!(
            nav.frame(&|| false).unwrap().selected_rank,
            if cancel_pending { 0 } else { 3 }
        );
    }
}
#[test]
fn resumed_root_object_keeps_draft_duplicate_fields_hidden_until_ready() {
    let mut text = String::from("{\"k\":1,\"payload\":\"");
    text.extend(std::iter::repeat_n('x', 17 * 1024 * 1024));
    text.push_str("\",\"k\":2}");
    let i = Input::new(text);
    let d = Dir::new();
    let stop = Cell::new(false);
    let b = || {
        BudgetManager::new(crate::engine::budget::Limits {
            memory: 1024 * 1024,
            disk: 128 * 1024 * 1024,
        })
    };
    let options = || OpenOptions {
        cache_dir: Some(d.0.clone()),
        live_views: true,
        ..Default::default()
    };
    assert!(
        open_progressive(&i.0, b(), options(), 1, &|| stop.get(), &mut |e| {
            if e.status == SessionStatus::Validating && e.committed_frontier > 16 * 1024 * 1024 {
                stop.set(true);
            }
            Ok(())
        })
        .is_err()
    );
    assert!(stop.get());
    let mut resumed = false;
    let mut index = open_progressive(&i.0, b(), options(), 2, &|| false, &mut |e| {
        resumed |= e.resumed;
        if e.status == SessionStatus::Validating {
            assert!(e.view.is_none());
        }
        Ok(())
    })
    .unwrap();
    assert!(resumed);
    assert_eq!(index.root().child_count(), 2);
    let last = index.pointer("/k", &|| false).unwrap();
    assert_eq!(raw(&mut index, last), "2");
}
