//! Composer completions. File discovery stays off the input/render thread.
use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Session,
    Command,
    File(PathBuf),
    Path,
    Directory,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub description: String,
    pub insert: String,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Slash,
    Mention,
    Path,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    pub range: Range<usize>,
    pub query: String,
}
#[derive(Clone, Debug)]
pub struct Replacement {
    pub token: Token,
    pub original: String,
}
impl Replacement {
    pub fn apply(&self, text: &str, insert: &str) -> Option<(String, usize)> {
        if text.get(self.token.range.clone())? != self.original {
            return None;
        }
        let existing_space = insert.ends_with(' ') && text[self.token.range.end..].starts_with(' ');
        let replacement = if existing_space {
            &insert[..insert.len() - 1]
        } else {
            insert
        };
        let mut result = text.to_owned();
        result.replace_range(self.token.range.clone(), replacement);
        Some((
            result,
            self.token.range.start + replacement.len() + usize::from(existing_space),
        ))
    }
}
pub fn token(text: &str, cursor: usize) -> Option<Token> {
    let before = text.get(..cursor)?;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    let leading = line.len() - line.trim_start().len();
    if line[leading..].starts_with('/')
        && !line[leading + 1..].contains(char::is_whitespace)
        && !line[leading + 1..].contains('/')
    {
        let end = text[cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |i| cursor + i);
        return Some(Token {
            kind: Kind::Slash,
            range: line_start + leading..end,
            query: line[leading + 1..].into(),
        });
    }
    let at = before.rfind('@')?;
    if at > 0
        && !before[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace() || "([{（，：".contains(c))
    {
        return None;
    }
    let raw = &before[at + 1..];
    let quoted = raw.starts_with('"');
    if (!quoted && raw.contains(char::is_whitespace))
        || raw.contains('\n')
        || raw.contains('@')
        || (quoted && raw[1..].contains('"'))
    {
        return None;
    }
    let end = if quoted {
        text[cursor..].find('"').map_or(cursor, |i| cursor + i + 1)
    } else {
        text[cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |i| cursor + i)
    };
    Some(Token {
        kind: Kind::Mention,
        range: at..end,
        query: raw.trim_start_matches('"').into(),
    })
}
fn path_boundary(c: char) -> bool {
    c.is_whitespace() || "()[]{}，。；：！？、（）【】".contains(c)
}
fn path_token(text: &str, cursor: usize) -> Option<Token> {
    let before = text.get(..cursor)?;
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in before.char_indices() {
        if escaped {
            escaped = false;
        } else if quote == Some('"') && c == '\\' {
            escaped = true;
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() {
            if path_boundary(c) {
                start = i + c.len_utf8();
            } else if (c == '"' || c == '\'') && i == start {
                quote = Some(c);
            }
        }
    }
    let raw = &before[start..];
    if raw.is_empty() || raw.starts_with(['@', '$']) || raw.contains('\n') {
        return None;
    }
    let opening = raw.chars().next().filter(|c| *c == '"' || *c == '\'');
    let query = if let Some(opening) = opening {
        let content = raw.strip_prefix(opening)?;
        let content = if quote.is_none() {
            content.strip_suffix(opening)?
        } else {
            content
        };
        if opening == '"' {
            content.replace("\\\"", "\"").replace("\\\\", "\\")
        } else {
            content.to_owned()
        }
    } else {
        raw.to_owned()
    };
    let mut end = cursor;
    let mut escaped = false;
    for c in text[cursor..].chars() {
        if let Some(opening) = quote {
            end += c.len_utf8();
            if escaped {
                escaped = false;
            } else if opening == '"' && c == '\\' {
                escaped = true;
            } else if c == opening {
                break;
            } else if c == '\n' {
                return None;
            }
        } else if path_boundary(c) {
            break;
        } else {
            end += c.len_utf8();
        }
    }
    Some(Token {
        kind: Kind::Path,
        range: start..end,
        query,
    })
}
pub fn commands() -> Vec<Candidate> {
    [
        ("help", "Keyboard shortcuts"),
        ("session", "Insert a frozen session quotation"),
        ("references", "Review or remove draft session references"),
        ("context", "Inspect and edit shared model input"),
        (
            "compact",
            "Compact context with the configured model policy",
        ),
        (
            "context-rebuild",
            "Rebuild an original branch with selected persistent edits",
        ),
        ("style", "Display and input settings"),
        ("search", "Search loaded branch content"),
        ("attach", "Attach a content snapshot"),
        ("attachments", "Review, refresh or remove attachments"),
        ("queue", "Review queued messages"),
        ("copy", "Copy the last answer"),
        ("recover", "Recover this session's local draft"),
        ("inspect", "Toggle full output inspector"),
        ("live", "Plugin presentation views"),
        ("shell", "Run a user shell command"),
        ("shells", "Cancel a separate shell run"),
        ("artifacts", "Read complete retained tool outputs"),
        ("config", "Open instance settings"),
        ("queue-mode", "Set one/all delivery"),
        ("quit", "Detach this frontend"),
        ("stop-host", "Stop the shared host"),
        ("commands", "Commands"),
    ]
    .into_iter()
    .map(|(name, description)| Candidate {
        label: format!("/{name}"),
        description: description.into(),
        insert: format!("/{name} "),
        action: Action::Command,
    })
    .collect()
}
fn references() -> Vec<Candidate> {
    vec![Candidate {
        label: "@session".into(),
        description: "Choose a saved session and branch to quote".into(),
        insert: String::new(),
        action: Action::Session,
    }]
}
fn matches(candidate: &Candidate, query: &str) -> bool {
    let q = query.to_lowercase();
    candidate.label.to_lowercase().contains(&q) || candidate.description.to_lowercase().contains(&q)
}
struct Request {
    generation: u64,
    query: String,
    kind: Kind,
}
struct Response {
    generation: u64,
    items: Vec<Candidate>,
}
pub struct Autocomplete {
    pub token: Option<Token>,
    pub items: Vec<Candidate>,
    pub selected: usize,
    pub open: bool,
    pub pending: bool,
    pub extra: Vec<Candidate>,
    stamp: Option<(String, usize, bool)>,
    generation: Arc<AtomicU64>,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Response>,
}
impl Autocomplete {
    pub fn new(root: PathBuf) -> Self {
        let (tx, requests) = mpsc::channel::<Request>();
        let (results, rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let epoch = generation.clone();
        std::thread::spawn(move || {
            while let Ok(mut request) = requests.recv() {
                while let Ok(newer) = requests.try_recv() {
                    request = newer;
                }
                let items = files(&root, &request.query, &request.kind, || {
                    epoch.load(Ordering::Relaxed) != request.generation
                });
                if epoch.load(Ordering::Relaxed) == request.generation {
                    let _ = results.send(Response {
                        generation: request.generation,
                        items,
                    });
                }
            }
        });
        Self {
            token: None,
            items: vec![],
            selected: 0,
            open: false,
            pending: false,
            extra: vec![],
            stamp: None,
            generation,
            tx,
            rx,
        }
    }
    pub fn refresh(&mut self, text: &str, cursor: usize, enabled: bool) {
        let paths = self.open
            && self.token.as_ref().is_some_and(|previous| {
                previous.kind == Kind::Path
                    && path_token(text, cursor)
                        .is_some_and(|current| current.range.start == previous.range.start)
            });
        self.refresh_inner(text, cursor, enabled, paths);
    }
    fn refresh_inner(&mut self, text: &str, cursor: usize, enabled: bool, paths: bool) {
        if self
            .stamp
            .as_ref()
            .is_some_and(|(old, pos, on)| old == text && *pos == cursor && *on == enabled)
        {
            return;
        }
        self.stamp = Some((text.into(), cursor, enabled));
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.token = if enabled {
            token(text, cursor).or_else(|| paths.then(|| path_token(text, cursor)).flatten())
        } else {
            None
        };
        self.items.clear();
        self.selected = 0;
        self.open = self.token.is_some();
        self.pending = false;
        let Some(token) = &self.token else {
            return;
        };
        self.items = match token.kind {
            Kind::Slash => commands().into_iter().chain(self.extra.clone()).collect(),
            Kind::Mention => references(),
            Kind::Path => vec![],
        }
        .into_iter()
        .filter(|c| matches(c, &token.query))
        .collect();
        let query = token.query.to_lowercase();
        self.items.sort_by_key(|item| {
            let label = item.label.trim_start_matches(['/', '@']).to_lowercase();
            if label == query {
                0
            } else if label.starts_with(&query) {
                1
            } else if label.contains(&query) {
                2
            } else {
                3
            }
        });
        if token.kind == Kind::Path
            || (token.kind == Kind::Mention && !token.query.starts_with("session:"))
        {
            self.pending = self
                .tx
                .send(Request {
                    generation,
                    query: token.query.clone(),
                    kind: token.kind.clone(),
                })
                .is_ok();
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.rx.try_recv() {
            if reply.generation != self.generation.load(Ordering::Relaxed) || !self.open {
                continue;
            }
            let selected = self.items.get(self.selected).cloned();
            self.items.extend(reply.items);
            self.pending = false;
            self.selected = selected
                .and_then(|item| self.items.iter().position(|c| *c == item))
                .unwrap_or(0);
            changed = true;
        }
        changed
    }
    pub fn dismiss(&mut self) {
        self.open = false;
        self.pending = false;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
    pub fn force(&mut self, text: &str, cursor: usize) {
        self.stamp = None;
        self.refresh_inner(text, cursor, true, true);
    }
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.items.len().saturating_sub(1));
    }
    pub fn take(
        &mut self,
        index: usize,
        text: &str,
        cursor: usize,
    ) -> Option<(Replacement, Candidate)> {
        if !self
            .stamp
            .as_ref()
            .is_some_and(|(source, pos, _)| source == text && *pos == cursor)
        {
            return None;
        }
        let current = token(text, cursor).or_else(|| path_token(text, cursor))?;
        if !self.open || self.token.as_ref() != Some(&current) {
            return None;
        }
        let item = self.items.get(index)?.clone();
        let original = text.get(current.range.clone())?.to_owned();
        self.dismiss();
        Some((
            Replacement {
                token: current,
                original,
            },
            item,
        ))
    }
}
impl Drop for Autocomplete {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}
fn files(root: &Path, query: &str, kind: &Kind, cancelled: impl Fn() -> bool) -> Vec<Candidate> {
    let (prefix, needle) = query
        .rsplit_once('/')
        .map_or(("", query), |(p, n)| (&query[..p.len() + 1], n));
    let directory = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return vec![];
    };
    let mut items = vec![];
    for entry in entries.flatten() {
        if cancelled() {
            return vec![];
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // The composer cannot represent terminal control characters in file names.
        if name.chars().any(char::is_control) {
            continue;
        }
        if !name.to_lowercase().contains(&needle.to_lowercase())
            || (needle.is_empty() && name.starts_with('.'))
        {
            continue;
        }
        let dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let path = format!("{prefix}{name}{}", if dir { "/" } else { "" });
        let plain = *kind == Kind::Path;
        let escaped = if plain
            && path
                .chars()
                .any(|c| path_boundary(c) || "\"'\\".contains(c))
        {
            let path = path.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{path}{}", if dir { "" } else { "\"" })
        } else if path.chars().any(char::is_whitespace) {
            if dir {
                format!("\"{path}")
            } else {
                format!("\"{path}\"")
            }
        } else {
            path.clone()
        };
        items.push(Candidate {
            label: format!("{}{path}", if plain { "" } else { "@" }),
            description: if dir {
                "Directory · continue browsing".into()
            } else {
                "File path · insert into draft".into()
            },
            insert: format!(
                "{}{escaped}{}",
                if plain { "" } else { "@" },
                if dir { "" } else { " " }
            ),
            action: if dir {
                Action::Directory
            } else if plain {
                Action::Path
            } else {
                Action::File(entry.path())
            },
        });
    }
    items.sort_by(|a, b| {
        matches!(b.action, Action::Directory)
            .cmp(&matches!(a.action, Action::Directory))
            .then_with(|| a.label.cmp(&b.label))
    });
    items
}
#[cfg(test)]
mod tests {
    use super::*;
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "eden-completion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("src folder")).unwrap();
            fs::write(path.join("src folder/中文.rs"), "fixture").unwrap();
            fs::write(path.join("sample.rs"), "fixture").unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn await_files(a: &mut Autocomplete) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while a.pending && std::time::Instant::now() < deadline {
            a.poll();
            std::thread::yield_now();
        }
        assert!(!a.pending);
    }
    #[test]
    fn plain_path_tab_inserts_path_and_preserves_punctuation() {
        let root = TestDirectory::new();
        let mut a = Autocomplete::new(root.0.clone());
        let text = "检查：sam，继续";
        let cursor = "检查：sam".len();
        a.refresh(text, cursor, true);
        assert!(!a.open);
        a.force(text, cursor);
        await_files(&mut a);
        assert_eq!(a.items.len(), 1);
        let (replacement, item) = a.take(0, text, cursor).unwrap();
        assert!(!matches!(item.action, Action::File(_)));
        assert_eq!(
            replacement.apply(text, &item.insert).unwrap().0,
            "检查：sample.rs ，继续"
        );
    }
    #[test]
    fn quoted_directory_tab_can_continue_to_file() {
        let root = TestDirectory::new();
        let mut a = Autocomplete::new(root.0.clone());
        a.force("src", 3);
        await_files(&mut a);
        let (replacement, item) = a.take(0, "src", 3).unwrap();
        let (text, cursor) = replacement.apply("src", &item.insert).unwrap();
        assert_eq!(text, "\"src folder/");
        a.force(&text, cursor);
        await_files(&mut a);
        let (replacement, item) = a.take(0, &text, cursor).unwrap();
        assert_eq!(
            replacement.apply(&text, &item.insert).unwrap().0,
            "\"src folder/中文.rs\" "
        );
    }
    #[test]
    fn path_selection_rejects_changed_draft_and_old_results() {
        let root = TestDirectory::new();
        let mut a = Autocomplete::new(root.0.clone());
        a.force("sam", 3);
        await_files(&mut a);
        assert_eq!(a.items.len(), 1);
        assert!(a.take(0, "sam-extra", 3).is_none());
        a.force("src", 3);
        a.force("missing", 7);
        await_files(&mut a);
        assert!(a.items.is_empty());
    }
    #[test]
    fn quoted_path_replaces_entire_source_and_keeps_suffix() {
        let root = TestDirectory::new();
        let mut a = Autocomplete::new(root.0.clone());
        for text in [
            "检查：\"src folder/中-old\"，后文",
            "检查：'src folder/中-old'，后文",
        ] {
            let cursor = text.find("-old").unwrap();
            a.force(text, cursor);
            await_files(&mut a);
            let (replacement, item) = a.take(0, text, cursor).unwrap();
            assert_eq!(
                replacement.apply(text, &item.insert).unwrap().0,
                "检查：\"src folder/中文.rs\" ，后文"
            );
            assert!(replacement.apply("different draft", &item.insert).is_none());
        }
        a.force("中文", 1);
        assert!(!a.open);
    }
    #[test]
    fn explicit_mention_still_returns_attachment_action() {
        let root = TestDirectory::new();
        let mut a = Autocomplete::new(root.0.clone());
        a.refresh("@sam", 4, true);
        await_files(&mut a);
        let (_, item) = a.take(0, "@sam", 4).unwrap();
        assert_eq!(item.action, Action::File(root.0.join("sample.rs")));
        assert_eq!(item.insert, "@sample.rs ");
    }
    #[test]
    fn exact_command_ranks_before_substring_match() {
        let mut a = Autocomplete::new(PathBuf::from("."));
        a.refresh("/live", 5, true);
        assert_eq!(a.items[0].label, "/live");
    }
    #[test]
    fn prefixes_obey_cursor_and_word_boundaries() {
        for (text, cursor, kind) in [
            ("/ski", 4, Kind::Slash),
            ("请检查 @sess 后文", 17, Kind::Mention),
        ] {
            let cursor = if text.starts_with('请') {
                "请检查 @sess".len()
            } else {
                cursor
            };
            assert_eq!(token(text, cursor).unwrap().kind, kind);
        }
        for text in [
            "mail@example.com",
            "$HOME",
            "$review",
            "path/part",
            "word/@file",
        ] {
            assert!(token(text, text.len()).is_none(), "{text}");
        }
    }
    #[test]
    fn accepting_replaces_only_token_and_keeps_suffix() {
        let text = "中文 /bad";
        assert!(token(text, text.len()).is_none());
        let text = "检查 @olddoc.rs 再继续";
        let cursor = "检查 @old".len();
        let t = token(text, cursor).unwrap();
        let r = Replacement {
            original: text[t.range.clone()].into(),
            token: t,
        };
        assert_eq!(r.apply(text, "@new.rs ").unwrap().0, "检查 @new.rs 再继续");
    }
    #[test]
    fn slash_skill_candidates_are_real_composer_results() {
        let mut a = Autocomplete::new(PathBuf::from("/missing"));
        a.extra = vec![
            Candidate {
                label: "/skill:review".into(),
                description: "Installed review skill".into(),
                insert: "/skill:review ".into(),
                action: Action::Command,
            },
            Candidate {
                label: "/skill:test-first".into(),
                description: "Installed test skill".into(),
                insert: "/skill:test-first ".into(),
                action: Action::Command,
            },
        ];

        a.refresh("/skill:", 7, true);
        assert_eq!(a.items.len(), 2);
        a.dismiss();
        a.refresh("/skill:", 7, true);
        assert!(!a.open);
        a.force("/skill:", 7);
        assert!(a.open);
    }
    #[test]
    fn stale_file_reply_cannot_reopen_dismissed_popup() {
        let mut a = Autocomplete::new(PathBuf::from("."));
        a.refresh("@", 1, true);
        a.dismiss();
        for _ in 0..100 {
            a.poll();
        }
        assert!(!a.open && !a.pending);
    }
}
