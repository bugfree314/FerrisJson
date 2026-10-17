#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Move(i32),
    Left,
    Right,
    Match(bool),
    Search(bool),
    First,
    Last,
    History(bool),
    Toggle,
    Open,
    Close,
    ExpandAll,
    CollapseAll,
    Copy(bool),
    Screen(char),
}
#[derive(Debug, PartialEq)]
pub struct Command {
    pub action: Action,
    pub count: usize,
    pub explicit_count: bool,
}
#[derive(Default)]
pub struct Vim {
    pub pending: String,
    pub last: String,
    count: usize,
    prefix: Option<char>,
}
impl Vim {
    pub fn cancel(&mut self) {
        self.pending.clear();
        self.count = 0;
        self.prefix = None;
    }
    #[cfg(any(not(target_os = "linux"), test))]
    pub fn record(&mut self, key: &str) {
        self.cancel();
        self.last = key.into();
    }
    pub fn feed(&mut self, key: char) -> Option<Command> {
        if key.is_ascii_digit() && self.prefix.is_none() && (key != '0' || self.count > 0) {
            self.count = (self.count * 10 + key.to_digit(10).unwrap() as usize).min(10_000);
            self.pending.push(key);
            if self.pending.len() > 8 {
                self.pending = self.count.to_string();
            }
            return None;
        }
        self.pending.push(key);
        let action = match (self.prefix.take(), key) {
            (Some('g'), 'g') => Action::First,
            (Some('z'), 'a') => Action::Toggle,
            (Some('z'), 'o') => Action::Open,
            (Some('z'), 'c') => Action::Close,
            (Some('z'), 'R') => Action::ExpandAll,
            (Some('z'), 'M') => Action::CollapseAll,
            (Some('y'), 'y') => Action::Copy(false),
            (Some('y'), 'p') => Action::Copy(true),
            (None, 'g' | 'z' | 'y') => {
                self.prefix = Some(key);
                return None;
            }
            (None, 'j') => Action::Move(1),
            (None, 'k') => Action::Move(-1),
            (None, 'h') => Action::Left,
            (None, 'l') => Action::Right,
            (None, 'n') => Action::Match(false),
            (None, 'N') => Action::Match(true),
            (None, '/') => Action::Search(false),
            (None, '?') => Action::Search(true),
            (None, 'G') => Action::Last,
            (None, '0') => Action::First,
            (None, '[') => Action::History(true),
            (None, ']') => Action::History(false),
            (None, 'H' | 'M' | 'L') => Action::Screen(key),
            _ => {
                self.cancel();
                return None;
            }
        };
        let command = Command {
            action,
            count: self.count.max(1),
            explicit_count: self.count > 0,
        };
        self.last = self.pending.clone();
        self.cancel();
        Some(command)
    }
    #[cfg(any(not(target_os = "linux"), test))]
    pub fn take_count(&mut self, label: &str) -> usize {
        let count = self.count.max(1);
        self.record(label);
        count
    }
    pub fn display(&self) -> &str {
        if self.pending.is_empty() {
            &self.last
        } else {
            &self.pending
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_sequences_and_cancellation() {
        let mut vim = Vim::default();
        for c in "12".chars() {
            assert!(vim.feed(c).is_none());
        }
        assert_eq!(vim.display(), "12");
        assert_eq!(vim.feed('j').unwrap().count, 12);
        assert_eq!(vim.display(), "12j");
        vim.feed('3');
        vim.feed('g');
        assert_eq!(
            vim.feed('g').unwrap(),
            Command {
                action: Action::First,
                count: 3,
                explicit_count: true
            }
        );
        vim.feed('2');
        assert_eq!(vim.feed('N').unwrap().action, Action::Match(true));
        vim.feed('g');
        vim.cancel();
        assert_eq!(vim.feed('j').unwrap().count, 1);
        for c in "999999999999999".chars() {
            vim.feed(c);
        }
        assert_eq!(vim.feed('l').unwrap().count, 10_000);
        vim.feed('z');
        assert_eq!(vim.feed('R').unwrap().action, Action::ExpandAll);
    }
}
