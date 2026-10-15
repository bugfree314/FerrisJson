//! Bounded pull-style query evaluation; intermediate node lists are never materialized.
mod plan;
mod source;
mod value;
use super::{
    budget::Resource,
    search::{Event, Input, ResultStore, Status},
    tree::{Edge, Location, TreeNode},
};
pub use plan::Plan;
use plan::{Expr, Operand, Selector, Step};
pub use source::collect;

pub fn run(
    input: Input,
    text: &str,
    generation: u64,
    cancel: &dyn Fn() -> bool,
    notify: &mut dyn FnMut(Event) -> Result<(), String>,
) -> Result<ResultStore, String> {
    let budget = input.budget();
    let _workspace = budget.reserve(
        Resource::Memory,
        16 * 1024 * 1024,
        "bounded query plan, traversal and scalar workspace",
    )?;
    let plan = Plan::parse(text)?;
    let mut source = source::input(input)?;
    let root = Location {
        node: source.root(cancel)?,
        ancestors: Vec::new(),
    };
    let (store, mut writer) = ResultStore::new(root.node.reference.session, generation, budget)?;
    notify(Event::Started(store.clone()))?;
    let mut announced = 0;
    let mut progress = std::time::Instant::now();
    // A single callback owns publication, so progress and hits cannot reorder events.
    let mut emit = |loc: Option<Location>, scanned: u64| -> Result<bool, String> {
        if let Some(loc) = loc {
            writer.location(&loc, cancel)?;
            if writer.count() == 1 || writer.count() - announced >= 128 {
                writer.publish(cancel)?;
                notify(Event::Batch {
                    generation,
                    start: announced,
                    count: writer.count() - announced,
                })?;
                announced = writer.count();
            }
        } else {
            store.progress(scanned);
            if progress.elapsed().as_millis() >= 100 {
                notify(Event::Progress {
                    generation,
                    scanned,
                })?;
                progress = std::time::Instant::now();
            }
        }
        Ok(true)
    };
    let result = {
        let mut runner = Runner {
            source: &mut *source,
            root: root.clone(),
            cancel,
            depth: 0,
            visited: 0,
        };
        let result = runner.eval(&plan, root, 0, &mut emit);
        let total = runner.visited;
        (result, total)
    };
    drop(emit);
    let (result, visited) = result;
    let status = match &result {
        Ok(_) => Status::Completed,
        Err(_) if cancel() => Status::Cancelled,
        Err(_) => Status::Failed,
    };
    if let Err(e) = writer.finish(status, visited, result.err().unwrap_or_default()) {
        store.fail(e);
    }
    let state = store.state();
    if state.count > announced {
        notify(Event::Batch {
            generation,
            start: announced,
            count: state.count - announced,
        })?;
    }
    notify(Event::Finished { generation, state })?;
    Ok(store)
}
type Emit<'a> = dyn FnMut(Option<Location>, u64) -> Result<bool, String> + 'a;
struct Runner<'a> {
    source: &'a mut dyn source::Source,
    root: Location,
    cancel: &'a dyn Fn() -> bool,
    depth: usize,
    visited: u64,
}
impl Runner<'_> {
    fn check(&self) -> Result<(), String> {
        if (self.cancel)() {
            Err("Query cancelled".into())
        } else {
            Ok(())
        }
    }
    fn child(&mut self, parent: &Location, ordinal: u64) -> Result<Location, String> {
        let node = self.source.child(parent.node, ordinal, self.cancel)?;
        self.append(parent, ordinal, node)
    }
    fn append(&self, parent: &Location, ordinal: u64, node: TreeNode) -> Result<Location, String> {
        if parent.ancestors.len() >= 127 {
            return Err("Query source depth exceeds 127".into());
        }
        let mut ancestors = parent.ancestors.clone();
        ancestors.push(Edge {
            parent: parent.node,
            ordinal,
        });
        Ok(Location { node, ancestors })
    }
    fn field(&mut self, parent: &Location, name: &str) -> Result<Option<Location>, String> {
        self.source
            .field(parent.node, name, self.cancel)?
            .map(|(o, n)| self.append(parent, o, n))
            .transpose()
    }
    fn eval(
        &mut self,
        path: &Plan,
        loc: Location,
        step: usize,
        emit: &mut Emit<'_>,
    ) -> Result<bool, String> {
        self.check()?;
        if self.depth >= 192 {
            return Err("Query evaluation stack exceeds 192 frames".into());
        }
        self.depth += 1;
        let result = self.eval_node(path, loc, step, emit);
        self.depth -= 1;
        result
    }
    fn eval_node(
        &mut self,
        path: &Plan,
        loc: Location,
        step: usize,
        emit: &mut Emit<'_>,
    ) -> Result<bool, String> {
        self.visited = self.visited.saturating_add(1);
        if self.visited.is_multiple_of(1024) {
            emit(None, self.visited)?;
        }
        let Some(Step {
            recursive,
            selectors,
        }) = path.steps.get(step)
        else {
            return emit(Some(loc), self.visited);
        };
        for selector in selectors {
            self.check()?;
            match selector {
                Selector::Name(name) => {
                    if let Some(child) = self.field(&loc, name)?
                        && !self.eval(path, child, step + 1, emit)?
                    {
                        return Ok(false);
                    }
                }
                Selector::Pointer(name) => {
                    let child = if loc.node.kind == "array" {
                        let ordinal = if name.is_empty()
                            || !name.bytes().all(|b| b.is_ascii_digit())
                            || (name.len() > 1 && name.starts_with('0'))
                        {
                            None
                        } else {
                            name.parse::<u64>().ok()
                        };
                        ordinal
                            .filter(|i| *i < loc.node.children)
                            .map(|i| self.child(&loc, i))
                            .transpose()?
                    } else {
                        self.field(&loc, name)?
                    };
                    if let Some(child) = child
                        && !self.eval(path, child, step + 1, emit)?
                    {
                        return Ok(false);
                    }
                }
                Selector::Index(index) => {
                    if loc.node.kind == "array" {
                        let n = loc.node.children as i128;
                        let i = if *index < 0 {
                            n + *index as i128
                        } else {
                            *index as i128
                        };
                        if i >= 0 && i < n {
                            let child = self.child(&loc, i as u64)?;
                            if !self.eval(path, child, step + 1, emit)? {
                                return Ok(false);
                            }
                        }
                    }
                }
                Selector::Wildcard | Selector::Filter(_) => {
                    for ordinal in 0..loc.node.children {
                        if ordinal.is_multiple_of(1024) {
                            emit(None, self.visited)?;
                        }
                        let child = self.child(&loc, ordinal)?;
                        if let Selector::Filter(expr) = selector
                            && !self.truth(expr, &child)?
                        {
                            continue;
                        }
                        if !self.eval(path, child, step + 1, emit)? {
                            return Ok(false);
                        }
                    }
                }
                Selector::Slice(start, end, stride) => {
                    if loc.node.kind != "array" || *stride == 0 {
                        continue;
                    }
                    let n = loc.node.children as i128;
                    let forward = *stride > 0;
                    let bound = |i: i64| {
                        let i = i as i128;
                        let i = if i < 0 { n + i } else { i };
                        if forward {
                            i.clamp(0, n)
                        } else {
                            i.clamp(-1, n - 1)
                        }
                    };
                    let mut i = start.map(bound).unwrap_or(if forward { 0 } else { n - 1 });
                    let end = end.map(bound).unwrap_or(if forward { n } else { -1 });
                    while if forward { i < end } else { i > end } {
                        let child = self.child(&loc, i as u64)?;
                        if !self.eval(path, child, step + 1, emit)? {
                            return Ok(false);
                        }
                        i += *stride as i128;
                    }
                }
            }
        }
        if *recursive {
            for ordinal in 0..loc.node.children {
                let child = self.child(&loc, ordinal)?;
                if !self.eval(path, child, step, emit)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    fn truth(&mut self, expr: &Expr, current: &Location) -> Result<bool, String> {
        self.check()?;
        match expr {
            Expr::And(a, b) => Ok(self.truth(a, current)? && self.truth(b, current)?),
            Expr::Or(a, b) => Ok(self.truth(a, current)? || self.truth(b, current)?),
            Expr::Not(e) => Ok(!self.truth(e, current)?),
            Expr::Exists(path) => {
                let start = if path.root {
                    self.root.clone()
                } else {
                    current.clone()
                };
                let mut found = false;
                self.eval(path, start, 0, &mut |loc, _| {
                    if loc.is_some() {
                        found = true;
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                })?;
                Ok(found)
            }
            Expr::Compare(a, op, b) => {
                let a = self.operand(a, current)?;
                let b = self.operand(b, current)?;
                value::compare(a, *op, b)
            }
        }
    }
    fn operand(
        &mut self,
        operand: &Operand,
        current: &Location,
    ) -> Result<Option<serde_json::Value>, String> {
        let Operand::Path(path) = operand else {
            if let Operand::Literal(value) = operand {
                return Ok(Some(value.clone()));
            }
            unreachable!()
        };
        let start = if path.root {
            self.root.clone()
        } else {
            current.clone()
        };
        let mut found = None;
        self.eval(path, start, 0, &mut |loc, _| {
            if let Some(loc) = loc {
                found = Some(loc);
                Ok(false)
            } else {
                Ok(true)
            }
        })?;
        let Some(loc) = found else { return Ok(None) };
        if loc.node.container() {
            return Err(
                "Container comparisons are unsupported; use existence or scalar fields".into(),
            );
        }
        self.source.scalar(loc.node, self.cancel).map(Some)
    }
}

#[cfg(test)]
mod tests;
