//! Pull-through evaluation: each selected node flows into the next step immediately.
use super::{Backend, QueryRequest, adapters, predicate, syntax::{self, Plan, Selector}};
use crate::{document::Document, engine::{
    budget::Resource, results::{ResultStore, ResultWriter, State, Status},
    sparse::SparseIndex, tree::{Edge, LegacyProvider, Location, Provider, SparseProvider, TreeNode},
}};
use serde_json_path::JsonPath;
use std::{mem::size_of, sync::Arc, time::Instant};

pub enum Input { Document(Arc<Document>), Sparse(Box<SparseIndex>) }
pub enum Event { Started(ResultStore), Batch { start: u64, count: u64 }, Progress { visited: u64 }, Finished(State) }
enum Program { Pipeline(Plan), Library(JsonPath, Arc<Document>), Pointer(Option<Vec<String>>) }

fn source(expression: &str) -> String {
    let expression = expression.trim();
    if expression == "." { "$".into() }
    else if expression.starts_with('.') || expression.starts_with('[') { format!("${expression}") }
    else { expression.into() }
}
fn preflight(source: &str, cancel: &dyn Fn() -> bool) -> Result<(), String> {
    let (mut depth, mut logical, mut quote, mut escape) = (0usize, 0usize, None, false);
    for (i, b) in source.bytes().enumerate() {
        if i % 1024 == 0 && cancel() { return Err("Query cancelled".into()); }
        if escape { escape = false; continue; }
        if let Some(q) = quote {
            if b == b'\\' { escape = true; } else if b == q { quote = None; }
            continue;
        }
        if b == b'\'' || b == b'"' { quote = Some(b); }
        else if b == b'[' || b == b'(' { depth += 1; }
        else if b == b']' || b == b')' { depth = depth.saturating_sub(1); }
        else if matches!(b, b'&' | b'|' | b'!') { logical += 1; }
        if depth >= syntax::DEPTH || logical >= 4 * syntax::DEPTH { return Err("Query syntax exceeds bounded nesting/operator workspace".into()); }
    }
    Ok(())
}
fn pointer(source: &str) -> Result<Option<Vec<String>>, String> {
    let mut parts = Vec::new();
    for part in source[1..].split('/') {
        if parts.len() >= syntax::DEPTH - 1 { return Err("Query Pointer exceeds depth 127".into()); }
        let mut value = String::new();
        let mut chars = part.chars();
        while let Some(c) = chars.next() {
            value.push(if c == '~' { match chars.next() { Some('0') => '~', Some('1') => '/', _ => return Ok(None) } } else { c });
        }
        parts.push(value);
    }
    Ok(Some(parts))
}

pub fn run(input: Input, request: &QueryRequest, generation: u64, cancel: &dyn Fn() -> bool, notify: &mut dyn FnMut(Event) -> Result<(), String>) -> Result<ResultStore, String> {
    if cancel() { return Err("Query cancelled".into()); }
    let (mut provider, memory, backend): (Box<dyn Provider>, _, _) = match input {
        Input::Document(doc) => {
            let memory = doc.value().is_some().then(|| doc.clone());
            let backend = if memory.is_some() { Backend::Memory } else { Backend::Indexed };
            (Box::new(LegacyProvider(doc)), memory, backend)
        }
        Input::Sparse(index) => (Box::new(SparseProvider::new(*index)?), None, Backend::Sparse),
    };
    if !provider.complete() { return Err("Pending: JSONPath requires complete source validation".into()); }
    let budget = provider.budget();
    let bytes = request.expression.len().checked_add(1).and_then(|n| n.checked_mul(128)).and_then(|n| n.checked_add(1024 * 1024)).ok_or("Query plan byte overflow")?;
    // Conservative allowance for source, tokens, AST and predicate literal copies.
    // Library/allocator overhead is not an exact whole-process memory cap.
    let _plan_lease = budget.reserve(Resource::Memory, bytes, "streaming query plan and parser allowance")?;
    let normalized = source(&request.expression);
    let program = if normalized.starts_with('/') { Program::Pointer(pointer(&normalized)?) }
    else {
        preflight(&normalized, cancel)?;
        let custom = syntax::compile(&normalized, cancel);
        let parsed = JsonPath::parse(&normalized).map_err(|e| format!("Invalid JSONPath: {e}"))?;
        if cancel() { return Err("Query cancelled".into()); }
        match custom {
            Ok(plan) if backend == Backend::Memory && plan.filters => Program::Library(parsed, memory.ok_or("Memory query source unavailable")?),
            Ok(plan) => Program::Pipeline(plan),
            Err(error) if backend == Backend::Memory && error.starts_with("Streaming JSONPath supports") => Program::Library(parsed, memory.ok_or("Memory query source unavailable")?),
            Err(error) => return Err(error),
        }
    };
    let steps = match &program { Program::Pipeline(plan) => plan.steps.len(), _ => 1 };
    let workspace = (steps + 2) * syntax::DEPTH * (2 * size_of::<TreeNode>() + size_of::<Edge>() + 32) * 3 + 8 * predicate::SCALAR_LIMIT;
    let _workspace = budget.reserve(Resource::Memory, workspace, "query traversal paths and bounded scalar evaluation")?;
    let root = provider.root(cancel)?;
    let (store, mut writer) = ResultStore::new(root.reference.session, generation, budget)?;
    notify(Event::Started(store.clone()))?;
    let mut runtime = Runtime { provider: provider.as_mut(), root, writer: &mut writer, notify, cancel, batch: 0, announced: 0, visited: 0, progress: Instant::now() };
    let mut location = Location { node: root, ancestors: Vec::with_capacity(syntax::DEPTH) };
    let result = match &program {
        Program::Pipeline(plan) => runtime.step(plan, &location, 0),
        Program::Pointer(parts) => (|| {
            let Some(parts) = parts else { return Ok(()); };
            for part in parts {
                let parent = location.node;
                let found = if parent.kind == "array" {
                    if part.starts_with('+') || (part.len() > 1 && part.starts_with('0')) { None }
                    else if let Ok(i) = part.parse::<u64>() {
                        if i < parent.children { Some((i, runtime.provider.child(parent, i, cancel)?)) } else { None }
                    } else { None }
                } else { runtime.provider.named_child(parent, part, cancel)? };
                let Some((ordinal, node)) = found else { return Ok(()); };
                location.ancestors.push(Edge { parent, ordinal }); location.node = node;
            }
            runtime.emit(&location)
        })(),
        Program::Library(query, doc) => (|| {
            // Explicit Memory compatibility route: this library call is not interruptible
            // and materializes its result list. File inputs never enter it.
            let ids = adapters::memory::execute(doc, query, cancel)?;
            for &id in &ids {
                let location = runtime.provider.locate_ref(doc.node_ref(id as usize)?, cancel)?;
                runtime.emit(&location)?;
            }
            Ok(())
        })(),
    };
    let visited = runtime.visited;
    let announced = runtime.announced;
    drop(runtime);
    let status = if cancel() { Status::Cancelled } else if result.is_err() { Status::Failed } else { Status::Completed };
    if let Err(error) = writer.finish(status, visited, result.err().unwrap_or_default()) { store.fail(error); }
    let state = store.state();
    if state.count > announced { notify(Event::Batch { start: announced, count: state.count - announced })?; }
    notify(Event::Finished(state))?;
    Ok(store)
}

struct Runtime<'a> {
    provider: &'a mut dyn Provider, root: TreeNode, writer: &'a mut ResultWriter,
    notify: &'a mut dyn FnMut(Event) -> Result<(), String>, cancel: &'a dyn Fn() -> bool,
    batch: u64, announced: u64, visited: u64, progress: Instant,
}
impl Runtime<'_> {
    fn check(&mut self) -> Result<(), String> {
        if (self.cancel)() { return Err("Query cancelled".into()); }
        self.visited = self.visited.checked_add(1).ok_or("Query visit counter overflow")?;
        if self.visited % 1024 == 0 && self.progress.elapsed().as_millis() >= 100 {
            if self.batch > 0 {
                self.writer.publish(self.cancel)?;
                (self.notify)(Event::Batch { start: self.writer.count() - self.batch, count: self.batch })?;
                self.announced = self.writer.count(); self.batch = 0;
            }
            (self.notify)(Event::Progress { visited: self.visited })?; self.progress = Instant::now();
        }
        Ok(())
    }
    fn emit(&mut self, location: &Location) -> Result<(), String> {
        self.check()?; self.writer.selection(location, self.cancel)?; self.batch += 1;
        if self.writer.count() == 1 || self.batch >= 128 {
            self.writer.publish(self.cancel)?;
            (self.notify)(Event::Batch { start: self.writer.count() - self.batch, count: self.batch })?;
            self.announced = self.writer.count(); self.batch = 0;
        }
        Ok(())
    }
    fn child_location(&self, parent: &Location, ordinal: u64, node: TreeNode) -> Result<Location, String> {
        if parent.ancestors.len() >= syntax::DEPTH - 1 { return Err("Query tree depth exceeds 127".into()); }
        let mut ancestors = Vec::with_capacity(syntax::DEPTH);
        ancestors.extend_from_slice(&parent.ancestors); ancestors.push(Edge { parent: parent.node, ordinal });
        Ok(Location { node, ancestors })
    }
    fn step(&mut self, plan: &Plan, location: &Location, at: usize) -> Result<(), String> {
        self.check()?;
        if at == plan.steps.len() { return self.emit(location); }
        if !plan.steps[at].recursive { return self.select(plan, location, at); }
        struct Frame { node: TreeNode, next: u64, previous: Option<TreeNode> }
        let mut current = Location { node: location.node, ancestors: Vec::with_capacity(syntax::DEPTH) };
        current.ancestors.extend_from_slice(&location.ancestors);
        self.select(plan, &current, at)?;
        if !current.node.container() { return Ok(()); }
        let mut stack = Vec::with_capacity(syntax::DEPTH);
        stack.push(Frame { node: current.node, next: 0, previous: None });
        while let Some(frame) = stack.last_mut() {
            self.check()?;
            if frame.next == frame.node.children {
                stack.pop();
                if let Some(edge) = current.ancestors.pop() { current.node = edge.parent; }
                continue;
            }
            let ordinal = frame.next;
            let child = self.provider.next_child(frame.node, ordinal, frame.previous, self.cancel)?;
            frame.next += 1; frame.previous = Some(child);
            if current.ancestors.len() >= syntax::DEPTH - 1 { return Err("Query tree depth exceeds 127".into()); }
            current.ancestors.push(Edge { parent: frame.node, ordinal }); current.node = child;
            self.select(plan, &current, at)?;
            if child.container() { stack.push(Frame { node: child, next: 0, previous: None }); }
            else if let Some(edge) = current.ancestors.pop() { current.node = edge.parent; }
        }
        Ok(())
    }
    fn select(&mut self, plan: &Plan, location: &Location, at: usize) -> Result<(), String> {
        for selector in &plan.steps[at].selectors {
            self.check()?;
            match selector {
                Selector::Name(name) => {
                    if let Some((ordinal, node)) = self.provider.named_child(location.node, name, self.cancel)? {
                        let child = self.child_location(location, ordinal, node)?; self.step(plan, &child, at + 1)?;
                    }
                }
                Selector::Index(index) if location.node.kind == "array" => {
                    if let Some(ordinal) = predicate::index(*index, location.node.children) {
                        let node = self.provider.child(location.node, ordinal, self.cancel)?;
                        let child = self.child_location(location, ordinal, node)?; self.step(plan, &child, at + 1)?;
                    }
                }
                Selector::Wildcard | Selector::Filter(_) if location.node.container() => {
                    let mut previous = None;
                    for ordinal in 0..location.node.children {
                        self.check()?;
                        let node = self.provider.next_child(location.node, ordinal, previous, self.cancel)?; previous = Some(node);
                        if let Selector::Filter(filter) = selector {
                            if !predicate::evaluate(self.provider, filter, node, self.root, self.cancel)? { continue; }
                        }
                        let child = self.child_location(location, ordinal, node)?; self.step(plan, &child, at + 1)?;
                    }
                }
                Selector::Slice(start, end, stride) if location.node.kind == "array" && *stride != 0 => {
                    let len = i128::from(location.node.children);
                    let normalize = |v: Option<i64>, default| v.map_or(default, |v| if v < 0 { len + i128::from(v) } else { i128::from(v) });
                    let (mut index, end) = if *stride > 0 { (normalize(*start, 0).clamp(0, len), normalize(*end, len).clamp(0, len)) }
                        else { (normalize(*start, len - 1).clamp(-1, len - 1), normalize(*end, -1).clamp(-1, len - 1)) };
                    while if *stride > 0 { index < end } else { index > end } {
                        self.check()?;
                        let node = self.provider.child(location.node, index as u64, self.cancel)?;
                        let child = self.child_location(location, index as u64, node)?; self.step(plan, &child, at + 1)?;
                        index += i128::from(*stride);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
