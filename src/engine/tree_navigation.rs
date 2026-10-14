use super::tree::{Location, PagedTree, Provider, TreeFrame, WINDOW};
use std::collections::VecDeque;
#[derive(Clone)]
pub enum Navigation {
    Move(i64),
    First,
    Last,
    Row(u64),
    Left(u64),
    Right(u64),
    Toggle,
    Open,
    Close,
    ExpandAll,
    CollapseAll,
    Select(Location),
    Locate(String),
    Reveal(Location),
    History(bool),
    Screen(char),
    Scroll(i64),
    Anchor(u64),
    CancelPending,
    ViewportRows(u64),
}
pub struct Navigator {
    pub tree: PagedTree,
    pub selected: Location,
    pub anchor: u64,
    history: VecDeque<Location>,
    history_index: usize,
    pending: Option<Navigation>,
    visible_rows: u64,
}
impl Navigator {
    pub fn new(provider: Box<dyn Provider>, cancel: &dyn Fn() -> bool) -> Result<Self, String> {
        let mut tree = PagedTree::new(provider, cancel)?;
        let selected = tree.root(cancel)?;
        let mut history = VecDeque::with_capacity(64);
        history.push_back(selected.clone());
        Ok(Self {
            tree,
            selected,
            anchor: 0,
            history,
            history_index: 0,
            pending: None,
            visible_rows: 24,
        })
    }
    pub fn replace(
        &mut self,
        provider: Box<dyn Provider>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut provider = provider;
        let root = provider.root(cancel)?;
        if root.reference.session != self.selected.node.reference.session {
            return Err("Replacement belongs to another session".into());
        }
        self.tree.provider = provider;
        self.selected = self.tree.normalize(&self.selected, cancel)?;
        if self.tree.provider.complete()
            && let Some(pending) = self.pending.take()
        {
            self.apply(pending, cancel)?;
        }
        Ok(())
    }
    fn visit(
        &mut self,
        location: Location,
        record: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let rank = self.tree.rank(&location, cancel)?;
        self.selected = location;
        if rank < self.anchor || rank >= self.anchor.saturating_add(self.visible_rows) {
            self.anchor = rank.saturating_sub(self.visible_rows / 4);
        }
        if record
            && self
                .history
                .get(self.history_index)
                .is_none_or(|l| l.node.reference != self.selected.node.reference)
        {
            self.history.truncate(self.history_index + 1);
            if self.history.len() == 64 {
                self.history.pop_front();
            }
            self.history.push_back(self.selected.clone());
            self.history_index = self.history.len() - 1;
        }
        Ok(())
    }
    pub fn apply(&mut self, command: Navigation, cancel: &dyn Fn() -> bool) -> Result<(), String> {
        let snapshot = (
            self.selected.clone(),
            self.anchor,
            self.history.clone(),
            self.history_index,
            self.pending.clone(),
        );
        let transactional = !matches!(command, Navigation::ExpandAll | Navigation::CollapseAll);
        if !matches!(
            command,
            Navigation::ViewportRows(_) | Navigation::Scroll(_) | Navigation::Anchor(_)
        ) {
            self.pending = None;
        }
        if transactional {
            self.tree.begin_edit()?;
        }
        let result = self.apply_inner(command, cancel).and_then(|()| {
            if transactional && cancel() {
                Err("Cancelled".into())
            } else {
                Ok(())
            }
        });
        if transactional {
            self.tree.finish_edit(result.is_ok())?;
        }
        if result.is_err() {
            self.selected = snapshot.0;
            self.anchor = snapshot.1;
            self.history = snapshot.2;
            self.history_index = snapshot.3;
            if cancel() {
                self.pending = snapshot.4;
            }
        }
        result
    }
    fn apply_inner(
        &mut self,
        command: Navigation,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.selected = self.tree.normalize(&self.selected, cancel)?;
        match command {
            Navigation::Move(delta) => {
                let (total, _) = self.tree.total(cancel)?;
                let rank = self.tree.rank(&self.selected, cancel)?;
                let next = if delta < 0 {
                    rank.saturating_sub(delta.unsigned_abs())
                } else {
                    rank.saturating_add(delta as u64).min(total - 1)
                };
                let loc = self.tree.row(next, cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::First => {
                let loc = self.tree.root(cancel)?;
                self.visit(loc, true, cancel)?;
                self.anchor = 0;
            }
            Navigation::Row(rank) => {
                let (total, exact) = self.tree.total(cancel)?;
                if !exact && rank >= total {
                    self.pending = Some(Navigation::Row(rank));
                    return Err("Pending: requested row has not been validated".into());
                }
                let loc = self.tree.row(rank.min(total - 1), cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::Last => {
                let (total, exact) = self.tree.total(cancel)?;
                if !exact {
                    self.pending = Some(Navigation::Last);
                    return Err("Pending: final visible row is unknown".into());
                }
                let loc = self.tree.row(total - 1, cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::Left(count) => {
                for _ in 0..count {
                    if self.tree.expanded(self.selected.node, cancel)? {
                        self.tree.set_expanded(&self.selected, false, cancel)?;
                    } else if let Some(edge) = self.selected.ancestors.last() {
                        let loc = Location {
                            node: edge.parent,
                            ancestors: self.selected.ancestors[..self.selected.ancestors.len() - 1]
                                .to_vec(),
                        };
                        self.visit(loc, true, cancel)?;
                    } else {
                        break;
                    }
                }
            }
            Navigation::Right(count) => {
                for _ in 0..count {
                    if !self.selected.node.container() {
                        break;
                    }
                    if !self.tree.expanded(self.selected.node, cancel)? {
                        self.tree.set_expanded(&self.selected, true, cancel)?;
                    } else if self.selected.node.children > 0 {
                        let parent = self.selected.node;
                        let mut loc = self.selected.clone();
                        loc.node = self.tree.provider.child(parent, 0, cancel)?;
                        loc.ancestors.push(super::tree::Edge { parent, ordinal: 0 });
                        self.visit(loc, true, cancel)?;
                    } else {
                        break;
                    }
                }
            }
            Navigation::Toggle | Navigation::Open | Navigation::Close => {
                let open = match command {
                    Navigation::Open => true,
                    Navigation::Close => false,
                    _ => !self.tree.expanded(self.selected.node, cancel)?,
                };
                self.tree.set_expanded(&self.selected, open, cancel)?;
            }
            Navigation::ExpandAll => {
                self.tree.reset(true, cancel)?;
                self.anchor = self
                    .tree
                    .rank(&self.selected, cancel)?
                    .saturating_sub(WINDOW as u64 / 4);
            }
            Navigation::CollapseAll => {
                self.tree.reset(false, cancel)?;
                let loc = self.tree.root(cancel)?;
                self.visit(loc, true, cancel)?;
                self.anchor = 0;
            }
            Navigation::Select(loc) => {
                let loc = self.tree.normalize(&loc, cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::Reveal(loc) => {
                let loc = self.tree.reveal(&loc, cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::Locate(pointer) => match self.tree.provider.locate(&pointer, cancel) {
                Ok(loc) => {
                    let loc = self.tree.reveal(&loc, cancel)?;
                    self.visit(loc, true, cancel)?;
                }
                Err(e) if !self.tree.provider.complete() && e.contains("Pending") => {
                    self.pending = Some(Navigation::Locate(pointer));
                    return Err(e);
                }
                Err(e) => return Err(e),
            },
            Navigation::History(back) => {
                let i = if back {
                    self.history_index.saturating_sub(1)
                } else {
                    (self.history_index + 1).min(self.history.len() - 1)
                };
                if let Some(loc) = self.history.get(i).cloned() {
                    let loc = self.tree.reveal(&loc, cancel)?;
                    self.visit(loc, false, cancel)?;
                    self.history_index = i;
                }
            }
            Navigation::Screen(which) => {
                let (total, _) = self.tree.total(cancel)?;
                let offset = match which {
                    'M' => self.visible_rows / 2,
                    'L' => self.visible_rows.saturating_sub(1),
                    _ => 0,
                };
                let loc = self
                    .tree
                    .row(self.anchor.saturating_add(offset).min(total - 1), cancel)?;
                self.visit(loc, true, cancel)?;
            }
            Navigation::Scroll(delta) => {
                let (total, _) = self.tree.total(cancel)?;
                self.anchor = if delta < 0 {
                    self.anchor.saturating_sub(delta.unsigned_abs())
                } else {
                    self.anchor.saturating_add(delta as u64).min(total - 1)
                };
            }
            Navigation::Anchor(rank) => {
                let (total, _) = self.tree.total(cancel)?;
                self.anchor = rank.min(total - 1);
            }
            Navigation::CancelPending => self.pending = None,
            Navigation::ViewportRows(rows) => self.visible_rows = rows.clamp(1, 64),
        }
        Ok(())
    }
    pub fn frame(&mut self, cancel: &dyn Fn() -> bool) -> Result<TreeFrame, String> {
        self.tree.frame(self.anchor, WINDOW, &self.selected, cancel)
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}
