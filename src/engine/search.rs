use super::budget::{BudgetManager, Reservation, Resource};
use crate::{
    document::{Document, SearchScope, indexed::Reader},
    engine::tree::{LegacyProvider, Provider, TreeNode},
};
use std::sync::Arc;
pub mod output;
mod results;
pub use results::{ResultStore, ResultWriter, State, Status};
#[derive(Clone, Debug)]
pub struct Query {
    pub term: String,
    pub case: bool,
    pub regex: bool,
    pub scope: SearchScope,
}
impl Query {
    pub fn literal(term: impl Into<String>) -> Self {
        Self {
            term: term.into(),
            case: false,
            regex: false,
            scope: SearchScope::All,
        }
    }
}
pub struct Matcher {
    query: Query,
    regex: regex::Regex,
    overlap: usize,
    _lease: Reservation,
}
impl Matcher {
    pub fn new(query: Query, budget: &BudgetManager) -> Result<Self, String> {
        if query.term.len() > 64 * 1024 {
            return Err("Search term exceeds 64 KiB".into());
        }
        let lease = budget.reserve(
            Resource::Memory,
            12 * 1024 * 1024,
            "search matcher and bounded decode workspace",
        )?;
        let pattern = if query.regex {
            query.term.clone()
        } else {
            regex::escape(&query.term)
        };
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!query.case)
            .size_limit(4 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
            .map_err(|e| format!("Invalid regular expression: {e}"))?;
        let overlap = query.term.chars().count() * 4;
        Ok(Self {
            query,
            regex,
            overlap,
            _lease: lease,
        })
    }
    pub fn keys(&self) -> bool {
        self.query.scope != SearchScope::Values
    }
    pub fn values(&self) -> bool {
        self.query.scope != SearchScope::Keys
    }
    pub fn text(&self, text: &str) -> bool {
        !self.query.term.is_empty() && self.regex.is_match(text)
    }
    pub(crate) fn range(
        &self,
        reader: &mut Reader<'_>,
        start: u64,
        end: u64,
        string: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        if self.query.term.is_empty() {
            return Ok(false);
        }
        if self.query.regex && end - start > 256 * 1024 {
            return Err("File regex search requires each key/string/number range <=256 KiB; use literal search or the memory backend".into());
        }
        reader.pos = start;
        let numeric = !string && matches!(reader.peek()?, Some(b'-' | b'0'..=b'9'));
        if string {
            reader.expect(b'"')?;
        }
        let capacity = (end - start + u64::from(numeric)).min(if self.query.regex {
            256 * 1024 + 1
        } else {
            (64 * 1024 + self.overlap + 4) as u64
        }) as usize;
        let mut window = String::with_capacity(capacity);
        let mut added = 0;
        loop {
            let c = if string {
                reader.character()?
            } else if reader.pos < end {
                Some(reader.next()? as char)
            } else {
                None
            };
            let Some(c) = c else {
                break;
            };
            // serde_json arbitrary_precision preserves digits but canonicalizes
            // exponent markers and supplies a missing positive exponent sign.
            if numeric && matches!(c, 'e' | 'E') {
                window.push('e');
                if !matches!(reader.peek()?, Some(b'+' | b'-')) {
                    window.push('+');
                    added += 1;
                }
            } else {
                window.push(c);
            }
            added += c.len_utf8();
            if added >= 64 * 1024 {
                if cancel() {
                    return Err("Search cancelled".into());
                }
                if !self.query.regex {
                    if self.text(&window) {
                        return Ok(true);
                    }
                    let mut boundary = window.len().saturating_sub(self.overlap);
                    while !window.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    window.drain(..boundary);
                }
                added = 0;
            }
        }
        if cancel() {
            return Err("Search cancelled".into());
        }
        Ok(self.text(&window))
    }
}
pub enum Input {
    Legacy(Arc<Document>),
    Sparse(Box<super::sparse::SparseIndex>),
}
impl Input {
    pub fn budget(&self) -> BudgetManager {
        match self {
            Self::Legacy(d) => d.budget().clone(),
            Self::Sparse(s) => s.budget().clone(),
        }
    }
    pub fn provider(self) -> Result<Box<dyn Provider>, String> {
        match self {
            Self::Legacy(d) => Ok(Box::new(LegacyProvider(d))),
            Self::Sparse(s) => Ok(Box::new(super::tree::SparseProvider::new(*s)?)),
        }
    }
    pub fn duplicate(&self) -> Result<Self, String> {
        match self {
            Self::Legacy(d) => Ok(Self::Legacy(d.clone())),
            Self::Sparse(s) => Ok(Self::Sparse(Box::new(s.read_view()?))),
        }
    }
    pub fn session(&self) -> super::session::SessionId {
        match self {
            Self::Legacy(d) => d.session_id(),
            Self::Sparse(s) => s.session_id(),
        }
    }
    pub fn run(
        &mut self,
        query: Query,
        generation: u64,
        cancel: &dyn Fn() -> bool,
        notify: &mut dyn FnMut(Event) -> Result<(), String>,
    ) -> Result<ResultStore, String> {
        let budget = self.budget();
        let matcher = Matcher::new(query, &budget)?;
        let (store, mut writer) = ResultStore::new(self.session(), generation, budget)?;
        notify(Event::Started(store.clone()))?;
        let mut scanned = 0;
        let mut links: Vec<(TreeNode, u64, Option<u64>)> = Vec::with_capacity(128);
        let mut batch = 0;
        let mut announced = 0;
        let mut progress = std::time::Instant::now();
        let result = if matcher.query.term.is_empty() {
            Ok(())
        } else {
            self.visit(&matcher, cancel, &mut |node, ordinal, depth, matched| {
                if cancel() {
                    return Err("Search cancelled".into());
                }
                if depth >= 128 || depth > links.len() {
                    return Err("Search traversal depth/order limit".into());
                }
                links.truncate(depth);
                if matched {
                    let mut parent = u64::MAX;
                    for (p, o, saved) in &mut links {
                        parent = if let Some(id) = *saved {
                            id
                        } else {
                            let id = writer.ancestor(*p, parent, *o, cancel)?;
                            *saved = Some(id);
                            id
                        };
                    }
                    writer.hit(node, parent, ordinal, cancel)?;
                    batch += 1;
                    if writer.count() == 1 || batch >= 128 {
                        writer.publish(cancel)?;
                        notify(Event::Batch {
                            generation,
                            start: writer.count() - batch,
                            count: batch,
                        })?;
                        announced = writer.count();
                        batch = 0;
                    }
                }
                if node.container() {
                    links.push((node, ordinal, None));
                }
                scanned += 1;
                if scanned % 1024 == 0 {
                    store.progress(scanned);
                    if progress.elapsed().as_millis() >= 100 {
                        notify(Event::Progress {
                            generation,
                            scanned,
                        })?;
                        progress = std::time::Instant::now();
                    }
                }
                Ok(())
            })
        };
        // Finalize already complete records even after cancellation or a semantic error.
        let status = match &result {
            Ok(()) => Status::Completed,
            Err(_) if cancel() => Status::Cancelled,
            Err(_) => Status::Failed,
        };
        let finalized = writer.finish(
            status,
            scanned,
            result.as_ref().err().cloned().unwrap_or_default(),
        );
        if let Err(e) = finalized {
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
    fn visit(
        &mut self,
        matcher: &Matcher,
        cancel: &dyn Fn() -> bool,
        visit: &mut dyn FnMut(TreeNode, u64, usize, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Legacy(doc) => {
                let provider = LegacyProvider(doc.clone());
                let mut ordinals = [0u64; 128];
                doc.search_visit(matcher, cancel, &mut |id, matched| {
                    let info = doc.node_info(id);
                    let depth = info.depth as usize;
                    if depth >= 128 {
                        return Err("Search depth limit".into());
                    }
                    let ordinal = ordinals[depth];
                    ordinals[depth] += 1;
                    if depth + 1 < 128 {
                        ordinals[depth + 1] = 0;
                    }
                    visit(provider.node(id), ordinal, depth, matched)
                })
            }
            Self::Sparse(index) => index.search_visit(matcher, cancel, visit),
        }
    }
}
#[derive(Clone)]
pub enum Event {
    Started(ResultStore),
    Batch {
        generation: u64,
        start: u64,
        count: u64,
    },
    Progress {
        generation: u64,
        scanned: u64,
    },
    Finished {
        generation: u64,
        state: State,
    },
}
impl Event {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Started(s) => s.generation(),
            Self::Batch { generation, .. }
            | Self::Progress { generation, .. }
            | Self::Finished { generation, .. } => *generation,
        }
    }
}
#[cfg(test)]
mod tests;
