//! The search engine: indexes, pagination and cached result views.
use super::scope::Stamp;
use fff_search::{
    FFFMode, FFFQuery, FilePicker, FilePickerOptions, FuzzyQuery, FuzzySearchOptions, GrepMode,
    GrepSearchOptions, PaginationArgs, SharedFilePicker, SharedFrecency,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
struct Index {
    picker: SharedFilePicker,
    stamp: Stamp,
}
impl Drop for Index {
    fn drop(&mut self) {
        self.picker.cancel();
        self.picker.shutdown_watches_and_wait();
    }
}
struct Cached {
    key: Value,
    stamp: Stamp,
    scope: PathBuf,
    follow: bool,
    excluded: Vec<PathBuf>,
    rows: Vec<Value>,
    matches: usize,
    metadata: Value,
    fallback: bool,
}
#[derive(Default)]
pub(crate) struct Engine {
    indexes: BTreeMap<(PathBuf, bool), Index>,
    pages: BTreeMap<String, (String, usize)>,
    results: BTreeMap<String, Cached>,
    serial: u64,
    progress: Option<fn(&str)>,
    viewed: BTreeMap<PathBuf, String>,
}
fn text<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(default)
}
fn fail(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn query_text(pattern: &str) -> FFFQuery<'_> {
    FFFQuery {
        raw_query: pattern,
        constraints: vec![],
        fuzzy_query: FuzzyQuery::Text(pattern),
        location: None,
    }
}
fn stamp(path: &Path, follow: bool, excluded: &[PathBuf]) -> Result<Stamp, String> {
    super::scope::stamp(path, follow, excluded)
}
impl Engine {
    pub(crate) fn with_progress(progress: fn(&str)) -> Self {
        Self {
            progress: Some(progress),
            ..Default::default()
        }
    }
    fn index(
        &mut self,
        scope: &Path,
        follow: bool,
        broad: bool,
        refresh: bool,
        current: &Stamp,
    ) -> Result<&Index, String> {
        let key = (scope.to_owned(), follow);
        if refresh
            || self
                .indexes
                .get(&key)
                .is_some_and(|index| &index.stamp != current)
        {
            self.indexes.remove(&key);
        }
        if !self.indexes.contains_key(&key) {
            let is_file = scope.is_file();
            let root = if is_file {
                scope.parent().ok_or("file has no parent")?
            } else {
                scope
            };
            let options = FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                mode: FFFMode::Ai,
                enable_content_indexing: !is_file,
                enable_mmap_cache: false,
                follow_symlinks: follow,
                watch: !is_file,
                enable_home_dir_scanning: broad || is_file,
                enable_fs_root_scanning: broad || is_file,
                ..Default::default()
            };
            let picker = SharedFilePicker::default();
            let frecency = SharedFrecency::noop();
            if is_file {
                let mut local = FilePicker::new(options).map_err(fail)?;
                local
                    .add_new_file(scope)
                    .ok_or("cannot index the requested file")?;
                *picker.write().map_err(fail)? = Some(local);
            } else {
                FilePicker::new_with_shared_state(picker.clone(), frecency.clone(), options)
                    .map_err(fail)?;
                if let Some(progress) = self.progress {
                    progress("index_started");
                }
                // The owning process can be cancelled at any point. Never interpret
                // an elapsed timeout as a completed scan or index.
                picker.wait_for_indexing_complete(Duration::MAX);
                picker.wait_for_watcher(Duration::MAX);
            }
            self.indexes.insert(
                key.clone(),
                Index {
                    picker,
                    stamp: current.clone(),
                },
            );
        }
        Ok(&self.indexes[&key])
    }
    pub(crate) fn query(&mut self, request: &Value) -> Result<Value, String> {
        let args = &request["arguments"];
        if request["name"] == "__record_read" {
            if let Some(directory) = args["_history_dir"].as_str() {
                let cwd = std::fs::canonicalize(text(request, "cwd", "")).map_err(fail)?;
                let path = std::fs::canonicalize(cwd.join(text(args, "path", ""))).map_err(fail)?;
                super::history::record(
                    Path::new(directory),
                    &cwd,
                    &path,
                    self.viewed.get(&path).cloned(),
                )?;
            }
            return Ok(json!({ "recorded": args["_history_dir"].is_string() }));
        }
        let limit = args.get("limit").map_or(Ok(30), |v| {
            v.as_u64()
                .filter(|n| (1..=200).contains(n))
                .ok_or("limit must be between 1 and 200")
        })? as usize;
        let mut key = json!({
            "cwd": request["cwd"],
            "name": request["name"],
            "arguments": request["arguments"],
        });
        if let Some(object) = key.get_mut("arguments").and_then(Value::as_object_mut) {
            object.remove("cursor");
            object.remove("limit");
            object.remove("refresh");
        }
        if let Some(cursor) = args.get("cursor").and_then(Value::as_str) {
            let (id, offset) = self
                .pages
                .get(cursor)
                .cloned()
                .ok_or("stale or unknown cursor")?;
            let cached = self.results.get(&id).ok_or("stale cursor")?;
            if cached.key != key {
                return Err("cursor belongs to a different query".into());
            }
            if stamp(&cached.scope, cached.follow, &cached.excluded)
                .ok()
                .as_ref()
                != Some(&cached.stamp)
            {
                return Err("stale cursor: search scope changed; start a new query".into());
            }
            return self.page(&id, offset, limit);
        }
        let cwd = Path::new(text(request, "cwd", ""));
        if !cwd.is_absolute() {
            return Err("cwd must be absolute".into());
        }
        let scope = std::fs::canonicalize(
            eden_workspace::paths::resolve_path(cwd, Path::new(text(args, "path", ".")))
                .map_err(fail)?,
        )
        .map_err(fail)?;
        if !scope.is_dir() && !scope.is_file() {
            return Err("scope must be a regular file or directory".into());
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .and_then(|p| std::fs::canonicalize(p).ok());
        if scope.is_dir()
            && (scope.parent().is_none() || home.as_ref() == Some(&scope))
            && args["allow_broad_scan"] != true
        {
            return Err("broad scan requires explicit search configuration".into());
        }
        let follow = args["follow_symlinks"] == true;
        let excluded: Vec<PathBuf> = args.get("_excluded_paths").map_or(Ok(vec![]), |value| {
            value
                .as_array()
                .ok_or("excluded_paths must be an array")?
                .iter()
                .map(|p| {
                    p.as_str()
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute())
                        .ok_or("excluded_paths must contain absolute paths")
                })
                .collect::<Result<_, _>>()
        })?;
        let before = stamp(&scope, follow, &excluded)?;
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("pattern must be nonempty")?;
        let name = text(request, "name", "grep");
        let mode = text(
            args,
            "mode",
            if name == "find" { "fuzzy" } else { "literal" },
        );
        let ranking = text(args, "ranking", "relevance");
        if !["relevance", "git", "definition", "history"].contains(&ranking)
            || (name == "find" && ranking == "definition")
        {
            return Err("unsupported ranking".into());
        }
        if ranking == "history" && !args["_history_dir"].is_string() {
            return Err(
                "history ranking requires explicit search.persist_history configuration".into(),
            );
        }
        let case = text(args, "case", "sensitive");
        if !["sensitive", "insensitive", "smart"].contains(&case) {
            return Err("invalid case mode".into());
        }
        if let Some(fallback) = args.get("fallback")
            && (fallback != "fuzzy" || name != "grep" || mode != "literal")
        {
            return Err("fallback fuzzy requires literal grep".into());
        }
        let insensitive =
            case == "insensitive" || (case == "smart" && !pattern.chars().any(char::is_uppercase));
        let mut excludes = globset::GlobSetBuilder::new();
        if let Some(values) = args.get("exclude") {
            for value in values.as_array().ok_or("exclude must be an array")? {
                excludes.add(
                    globset::Glob::new(value.as_str().ok_or("exclude entries must be strings")?)
                        .map_err(fail)?,
                );
            }
        }
        let excludes = excludes.build().map_err(fail)?;
        let mut includes = globset::GlobSetBuilder::new();
        if let Some(values) = args.get("include") {
            for value in values.as_array().ok_or("include must be an array")? {
                includes.add(
                    globset::Glob::new(value.as_str().ok_or("include entries must be strings")?)
                        .map_err(fail)?,
                );
            }
        }
        let includes = includes.build().map_err(fail)?;
        let selected = |path: &str| {
            !excludes.is_match(path) && (includes.is_empty() || includes.is_match(path))
        };
        let context = args.get("context").map_or(Ok(0), |value| {
            value
                .as_u64()
                .filter(|lines| *lines <= 100)
                .ok_or("context must be between 0 and 100")
        })?;
        let progress = self.progress;
        let index = self.index(
            &scope,
            follow,
            args["allow_broad_scan"] == true,
            args["refresh"] == true,
            &before,
        )?;
        // Keep the write lock through search so watcher events cannot resurrect
        // excluded files between pruning and the search/skip inventory reads.
        let mut guard = index.picker.write().map_err(fail)?;
        let picker = guard.as_mut().ok_or("index unavailable")?;
        let removed: Vec<_> = picker
            .get_files()
            .iter()
            .filter(|file| !file.is_deleted())
            .map(|file| file.absolute_path(&*picker, picker.base_path()))
            .filter(|path| super::scope::is_excluded(path, &excluded))
            .collect();
        for path in removed {
            picker.remove_file_by_path(path);
        }
        let picker: &FilePicker = picker;
        let mut rows = vec![];
        let mut fallback = false;
        let mut metadata = json!({
            "complete": true,
            "index": {
                "state": "ready",
                "content_indexed": picker.bigram_index().is_some(),
                "persistent_content": false,
                "worker_pid": std::process::id(),
            },
            "scope": {
                "path": scope,
                "ignore": "FFF gitignore and hidden-file rules; .git excluded",
                "follow_symlinks": follow,
            },
            "mode": mode,
            "case": case,
            "skipped": [],
            "truncation": { "result_limit": false, "line_display_bytes": 512 },
        });
        if name == "find" {
            if !["glob", "fuzzy"].contains(&mode) {
                return Err("find mode must be fuzzy or glob".into());
            }
            if mode == "glob" {
                let folded_pattern = if insensitive {
                    pattern.to_lowercase()
                } else {
                    pattern.into()
                };
                let matcher = globset::GlobBuilder::new(&folded_pattern)
                    .case_insensitive(insensitive)
                    .build()
                    .map_err(fail)?
                    .compile_matcher();
                for file in picker.get_files().iter().filter(|f| !f.is_deleted()) {
                    let path = file.relative_path(picker);
                    let matched_path = if insensitive {
                        path.to_lowercase()
                    } else {
                        path.clone()
                    };
                    if matcher.is_match(&matched_path) && !excludes.is_match(&path) {
                        rows.push(json!({ "path": path }));
                    }
                }
            } else {
                let options = FuzzySearchOptions {
                    pagination: PaginationArgs {
                        offset: 0,
                        limit: picker.live_file_count().max(1),
                    },
                    ..Default::default()
                };
                let query = query_text(pattern);
                let found = picker.fuzzy_search(&query, None, options);
                let mut sensitive = neo_frizbee::Matcher::new(
                    pattern,
                    &neo_frizbee::Config {
                        casing: neo_frizbee::CaseMatching::Respect,
                        max_typos: Some((pattern.len() / 3).min(2) as u16),
                        ..Default::default()
                    },
                );
                for (file, score) in found.items.iter().zip(&found.scores) {
                    let path = file.relative_path(picker);
                    if excludes.is_match(&path)
                        || (!insensitive && sensitive.match_one(&path, 0).is_none())
                    {
                        continue;
                    }
                    rows.push(json!({
                        "path": path,
                        "score": if ranking == "git" {
                                score.base_score
                                    + score.filename_bonus
                                    + score.special_filename_bonus
                                    + score.path_alignment_bonus
                                    + score.git_status_boost
                            } else {
                                score.base_score
                                    + score.filename_bonus
                                    + score.special_filename_bonus
                                    + score.path_alignment_bonus
                            },
                    }));
                }
            }
            if mode == "fuzzy" {
                // FFF's byte prefilters can discard Unicode case variants. Its
                // own matcher can evaluate those paths without the byte filter.
                let mut matcher = unicode_matcher(pattern, insensitive, false);
                for file in picker.get_files().iter().filter(|f| !f.is_deleted()) {
                    let path = file.relative_path(picker);
                    if (pattern.is_ascii() && path.is_ascii())
                        || excludes.is_match(&path)
                        || rows.iter().any(|row| row["path"] == path)
                    {
                        continue;
                    }
                    if let Some(matched) = matcher.match_one(&path, 0) {
                        rows.push(json!({ "path": path, "score": matched.score }));
                    }
                }
            }
            if mode == "glob" {
                rows.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            } else {
                sort_fuzzy(&mut rows);
            }
        } else if name == "grep" {
            if !["literal", "regex", "fuzzy"].contains(&mode) {
                return Err("grep mode must be literal, regex or fuzzy".into());
            }
            let max_size = args
                .get("max_file_bytes")
                .map_or(Ok(5 * 1024 * 1024), |v| {
                    v.as_u64()
                        .filter(|n| *n > 0 && *n <= 10 * 1024 * 1024)
                        .ok_or("max_file_bytes must be between 1 and 10485760")
                })?;
            let direct = scope.is_file()
                && mode != "fuzzy"
                && std::fs::metadata(&scope).map_err(fail)?.len() > max_size;
            let mut streaming = None;
            for file in picker.get_files().iter().filter(|f| !f.is_deleted()) {
                let path = file.relative_path(picker);
                if !selected(&path) {
                    continue;
                }
                let reason = if file.is_binary() {
                    Some("binary")
                } else if file.size > max_size && !direct {
                    Some("size_limit")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    metadata["skipped"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({ "path": path, "reason": reason }));
                } else {
                    let absolute = file.absolute_path(picker, picker.base_path());
                    std::fs::File::open(&absolute)
                        .map_err(|e| format!("cannot read {path}: {e}"))?;
                    if direct {
                        if let Some(progress) = progress {
                            progress("stream_started");
                        }
                        let found = super::stream::search(
                            &absolute,
                            &path,
                            pattern,
                            mode == "literal",
                            insensitive,
                        )?;
                        metadata["skipped"]
                            .as_array_mut()
                            .unwrap()
                            .extend(found.skipped);
                        metadata["index"]["direct_stream"] = json!(true);
                        metadata["truncation"]["stream_line_bytes"] = json!(10 * 1024 * 1024);
                        streaming = Some(found.rows);
                    }
                }
            }
            if !metadata["skipped"].as_array().unwrap().is_empty() {
                metadata["complete"] = json!(false);
            }
            let run = |mode: &str| -> Result<Vec<Value>, String> {
                // FFF disables Unicode by default. Keep character semantics independent
                // of case selection, and bypass its query-language case inference.
                let native_pattern = if mode != "fuzzy" {
                    format!(
                        "(?u{}:{})",
                        if insensitive { "i" } else { "-i" },
                        if mode == "literal" {
                            literal_pattern(pattern)
                        } else {
                            safe_regex(pattern)
                        }
                    )
                } else {
                    pattern.into()
                };
                let native_mode = if mode == "fuzzy" {
                    GrepMode::Fuzzy
                } else {
                    GrepMode::Regex
                };
                if native_mode == GrepMode::Regex {
                    regex::bytes::RegexBuilder::new(&native_pattern)
                        .multi_line(true)
                        .unicode(false)
                        .build()
                        .map_err(|e| format!("invalid regex: {e}"))?;
                }
                let query = query_text(&native_pattern);
                let found = picker.grep(
                    &query,
                    &GrepSearchOptions {
                        mode: native_mode,
                        classify_definitions: true,
                        smart_case: mode == "fuzzy" && insensitive,
                        max_file_size: max_size,
                        max_matches_per_file: usize::MAX,
                        page_limit: usize::MAX,
                        before_context: 0,
                        after_context: 0,
                        ..Default::default()
                    },
                );
                if let Some(error) = found.regex_fallback_error {
                    return Err(format!("invalid regex: {error}"));
                }
                if found.next_file_offset != 0 || found.literal_fallback {
                    return Err(
                        "native search did not complete with the requested semantics".into(),
                    );
                }
                let mut output = vec![];
                let mut sensitive = neo_frizbee::Matcher::new(
                    pattern,
                    &neo_frizbee::Config {
                        casing: neo_frizbee::CaseMatching::Respect,
                        max_typos: Some((pattern.len() / 3).min(2) as u16),
                        ..Default::default()
                    },
                );
                for item in found.matches {
                    let path = found.files[item.file_index].relative_path(picker);
                    if !selected(&path) {
                        continue;
                    }
                    use std::io::{BufRead, Seek};
                    let mut file = std::fs::File::open(
                        found.files[item.file_index].absolute_path(picker, picker.base_path()),
                    )
                    .map_err(fail)?;
                    file.seek(std::io::SeekFrom::Start(item.byte_offset))
                        .map_err(fail)?;
                    let mut line = String::new();
                    std::io::BufReader::new(file)
                        .read_line(&mut line)
                        .map_err(fail)?;
                    let original = line.trim_end_matches(['\r', '\n']);
                    if mode == "fuzzy" && !insensitive && sensitive.match_one(original, 0).is_none()
                    {
                        continue;
                    }
                    output.push(json!({
                        "path": path,
                        "line": item.line_number,
                        "column": item.col,
                        "text": item.line_content,
                        "score": item.fuzzy_score,
                        "definition_hint": item.is_definition,
                        "git_changed":
                            found.files[item.file_index].git_status.is_some(),
                        "line_truncated": original.len() > item.line_content.len(),
                        "line_bytes": original.len(),
                    }));
                }
                if mode == "fuzzy" {
                    use std::io::BufRead;
                    let mut matcher = unicode_matcher(pattern, insensitive, true);
                    for file in picker
                        .get_files()
                        .iter()
                        .filter(|f| !f.is_deleted() && !f.is_binary() && f.size <= max_size)
                    {
                        let path = file.relative_path(picker);
                        if !selected(&path) {
                            continue;
                        }
                        let input =
                            std::fs::File::open(file.absolute_path(picker, picker.base_path()))
                                .map_err(fail)?;
                        for (line_index, line) in std::io::BufReader::new(input).lines().enumerate()
                        {
                            let line = line.map_err(fail)?;
                            let number = line_index + 1;
                            if (pattern.is_ascii() && line.is_ascii())
                                || line.len() > 512
                                || output
                                    .iter()
                                    .any(|row| row["path"] == path && row["line"] == number)
                            {
                                continue;
                            }
                            if let Some((score, column)) =
                                unicode_line_match(&mut matcher, pattern, &line)
                            {
                                output.push(json!({
                                    "path": path,
                                    "line": number,
                                    "column": column,
                                    "text": line,
                                    "score": score,
                                    "definition_hint": false,
                                    "git_changed": file.git_status.is_some(),
                                    "line_truncated": false,
                                    "line_bytes": line.len(),
                                }));
                            }
                        }
                    }
                    sort_fuzzy(&mut output);
                } else {
                    output.sort_by(|a, b| {
                        a["path"]
                            .as_str()
                            .cmp(&b["path"].as_str())
                            .then(a["line"].as_u64().cmp(&b["line"].as_u64()))
                    });
                }
                Ok(output)
            };
            let streamed = streaming.is_some();
            rows = match streaming {
                Some(rows) => rows,
                None => run(mode)?,
            };
            if streamed
                && rows.is_empty()
                && args["fallback"] == "fuzzy"
                && metadata["complete"] == true
            {
                metadata["skipped"].as_array_mut().unwrap().push(json!({
                    "path": scope.file_name().unwrap_or_default().to_string_lossy(),
                    "reason": "fuzzy_large_file_unsupported",
                }));
                metadata["complete"] = json!(false);
                metadata["mode"] = json!("fuzzy");
                fallback = true;
            }
            if !fallback
                && rows.is_empty()
                && mode == "literal"
                && args["fallback"] == "fuzzy"
                && metadata["skipped"].as_array().unwrap().is_empty()
            {
                rows = run("fuzzy")?;
                fallback = true;
                metadata["mode"] = json!("fuzzy");
            }
            if mode == "fuzzy" || fallback {
                use std::io::BufRead;
                for file in picker
                    .get_files()
                    .iter()
                    .filter(|f| !f.is_deleted() && !f.is_binary() && f.size <= max_size)
                {
                    let path = file.relative_path(picker);
                    if !selected(&path) {
                        continue;
                    }
                    let input = std::fs::File::open(file.absolute_path(picker, picker.base_path()))
                        .map_err(fail)?;
                    for (index, line) in std::io::BufReader::new(input).split(b'\n').enumerate() {
                        if line.map_err(fail)?.len() > 512 {
                            metadata["complete"] = json!(false);
                            metadata["skipped"].as_array_mut().unwrap().push(json!({
                                "path": path,
                                "line": index + 1,
                                "reason": "fuzzy_long_line_not_fully_evaluated",
                                "continuation":
                                    "read this file at the reported line",
                            }));
                        }
                    }
                }
            }
        } else {
            return Err("unknown search tool".into());
        }
        // Canonical identity deduplicates explicit symlink aliases. Keep the first
        // result in the requested ordering and never duplicate a physical line.
        let root = if scope.is_file() {
            scope.parent().unwrap()
        } else {
            &scope
        };
        let mut seen = std::collections::BTreeSet::new();
        rows.retain(|row| {
            std::fs::canonicalize(root.join(row["path"].as_str().unwrap_or("")))
                .is_ok_and(|path| seen.insert((path, row["line"].as_u64())))
        });
        if ranking == "definition" {
            rows.sort_by_key(|row| std::cmp::Reverse(row["definition_hint"] == true));
        }
        if ranking == "git" && name == "grep" {
            rows.sort_by_key(|row| std::cmp::Reverse(row["git_changed"] == true));
        }
        if ranking == "history" {
            let scores = super::history::scores(
                Path::new(args["_history_dir"].as_str().unwrap()),
                &std::fs::canonicalize(cwd).map_err(fail)?,
                pattern,
            )?;
            let score = |row: &Value| {
                std::fs::canonicalize(root.join(row["path"].as_str().unwrap_or("")))
                    .ok()
                    .and_then(|path| scores.get(&path).copied())
                    .unwrap_or_default()
            };
            rows.sort_by(|a, b| score(b).total_cmp(&score(a)));
        }
        let matches = rows.len();
        if name == "grep" && context > 0 {
            rows = super::stream::with_context(rows, root, context)?;
        }
        metadata["context_lines"] = json!(context);
        metadata["ranking"] = json!(ranking);
        metadata["index"]["history_persistent"] = json!(args["_history_dir"].is_string());
        drop(guard);
        let after = stamp(&scope, follow, &excluded)?;
        if before != after {
            return Err("search scope changed during query; refresh and retry".into());
        }
        self.serial += 1;
        metadata["index"]["revision"] = json!(self.serial);
        let id = format!("{}-{}", std::process::id(), self.serial);
        // Bounded cache eviction is visible as a stale cursor, never a silent restart.
        if self.results.len() >= 16 {
            self.results.clear();
            self.pages.clear();
        }
        self.results.insert(
            id.clone(),
            Cached {
                key,
                stamp: after,
                scope,
                follow,
                excluded,
                rows,
                matches,
                metadata,
                fallback,
            },
        );
        self.page(&id, 0, limit)
    }
    fn page(&mut self, id: &str, offset: usize, limit: usize) -> Result<Value, String> {
        let cached = self.results.get(id).ok_or("stale cursor")?;
        let skipped = cached.metadata["skipped"].as_array().expect("skip list");
        let total = cached.rows.len() + skipped.len();
        let mut end = offset;
        let mut bytes = 0;
        for item in cached
            .rows
            .iter()
            .chain(skipped.iter())
            .skip(offset)
            .take(limit)
        {
            let size = serde_json::to_vec(item).map_err(fail)?.len();
            if end > offset && bytes + size > 16384 {
                break;
            }
            bytes += size;
            end += 1;
        }
        let row_start = offset.min(cached.rows.len());
        let row_end = end.min(cached.rows.len());
        let mut groups: Vec<Value> = vec![];
        for row in &cached.rows[row_start..row_end] {
            if groups.last().is_none_or(|g| g["path"] != row["path"]) {
                groups.push(json!({ "path": row["path"], "matches": [], "context": [] }));
            }
            let mut item = row.clone();
            item.as_object_mut().unwrap().remove("path");
            let field = if row["context"] == true {
                "context"
            } else {
                "matches"
            };
            groups.last_mut().unwrap()[field]
                .as_array_mut()
                .unwrap()
                .push(item);
        }
        if self.viewed.len() > 1024 {
            self.viewed.clear();
        }
        let root = if cached.scope.is_file() {
            cached.scope.parent().unwrap()
        } else {
            &cached.scope
        };
        for row in &cached.rows[row_start..row_end] {
            if let Ok(path) = std::fs::canonicalize(root.join(row["path"].as_str().unwrap_or(""))) {
                self.viewed.insert(
                    path,
                    cached.key["arguments"]["pattern"]
                        .as_str()
                        .unwrap_or("")
                        .into(),
                );
            }
        }
        let mut page = cached.metadata.clone();
        page["groups"] = json!(groups);
        page["skipped_count"] = json!(skipped.len());
        page["skipped"] = json!(
            &skipped
                [offset.saturating_sub(cached.rows.len())..end.saturating_sub(cached.rows.len())]
        );
        page["total_matches"] = if page["complete"] == true {
            json!(cached.matches)
        } else {
            Value::Null
        };
        page["matched_so_far"] = json!(cached.matches);
        page["returned"] = json!(row_end - row_start);
        page["returned_matches"] = json!(
            cached.rows[row_start..row_end]
                .iter()
                .filter(|row| row["context"] != true)
                .count()
        );
        page["has_more"] = json!(end < total);
        if end < total {
            let cursor = format!("{id}:{end}");
            self.pages.insert(cursor.clone(), (id.into(), end));
            page["cursor"] = json!(cursor);
        }
        if cached.fallback {
            Ok(json!({
                "exact": {
                    "mode": "literal",
                    "complete": true,
                    "total_matches": 0,
                    "has_more": false,
                },
                "candidates": page,
            }))
        } else {
            Ok(page)
        }
    }
}
// FFF's regex builder rewrites every textual backslash+n, including escaped
// backslashes. Encode escaped backslashes before that second compilation.
fn safe_regex(pattern: &str) -> String {
    let mut output = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => output.push_str(r"\x5c"),
                Some(next) => {
                    output.push(ch);
                    output.push(next);
                }
                None => output.push(ch),
            }
        } else {
            output.push(ch);
        }
    }
    output
}
fn literal_pattern(pattern: &str) -> String {
    safe_regex(&regex::escape(pattern))
}
// The same matching engine as FFF, with character-count quality guards for
// Unicode (FFF 0.10.6's byte-count guards can reject a one-character é match).
fn unicode_matcher(pattern: &str, insensitive: bool, content: bool) -> neo_frizbee::Matcher {
    let scoring = if content {
        neo_frizbee::Scoring {
            exact_match_bonus: 100,
            prefix_bonus: 0,
            capitalization_bonus: if insensitive { 0 } else { 4 },
            ..Default::default()
        }
    } else {
        Default::default()
    };
    neo_frizbee::Matcher::new(
        pattern,
        &neo_frizbee::Config {
            casing: if insensitive {
                neo_frizbee::CaseMatching::Ignore
            } else {
                neo_frizbee::CaseMatching::Respect
            },
            unicode: neo_frizbee::UnicodeMatching::Always,
            max_typos: Some((pattern.chars().count() / 3).min(2) as u16),
            scoring,
            ..Default::default()
        },
    )
}
fn unicode_line_match(
    matcher: &mut neo_frizbee::Matcher,
    pattern: &str,
    line: &str,
) -> Option<(u16, usize)> {
    let mut matched = matcher.match_one_indices(line, 0)?;
    matched.indices.sort_unstable();
    let count = pattern.chars().count();
    let typos = (count / 3).min(2);
    let first = *matched.indices.first()?;
    let last = *matched.indices.last()?;
    let span = last - first + 1;
    let density = if matched.indices.len() >= count {
        45
    } else {
        65
    };
    if usize::from(matched.score) < count * 8
        || matched.indices.len() < count.saturating_sub(typos).max(1)
        || span > count * 3
        || matched.indices.len() * 100 / span < density
        || matched
            .indices
            .windows(2)
            .filter(|pair| pair[1] != pair[0] + 1)
            .count()
            > (count / 3).max(2)
    {
        return None;
    }
    Some((matched.score, line.char_indices().nth(first)?.0))
}
fn sort_fuzzy(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        b["score"]
            .as_i64()
            .cmp(&a["score"].as_i64())
            .then(a["path"].as_str().cmp(&b["path"].as_str()))
            .then(a["line"].as_u64().cmp(&b["line"].as_u64()))
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eden-search-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn request(&self, pattern: &str) -> Value {
            json!({ "cwd": self.0, "name": "grep", "arguments": { "pattern": pattern } })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn grep_include_filters_skips_and_context_is_deduplicated_before_pagination() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("a.rs"),
            "one\ntwo\nneedle\nfour\nneedle\nsix\nseven\n",
        )
        .unwrap();
        std::fs::write(fixture.0.join("excluded.txt"), vec![b'x'; 6 * 1024 * 1024]).unwrap();
        let mut request = fixture.request("needle");
        request["arguments"]["include"] = json!(["*.rs"]);
        request["arguments"]["context"] = json!(1);
        request["arguments"]["limit"] = json!(2);
        let mut engine = Engine::default();
        let mut lines = vec![];
        loop {
            let page = engine.query(&request).unwrap();
            assert_eq!(page["complete"], true);
            assert_eq!(page["total_matches"], 2);
            assert_eq!(page["skipped_count"], 0);
            for group in page["groups"].as_array().unwrap() {
                for field in ["matches", "context"] {
                    if let Some(rows) = group[field].as_array() {
                        lines.extend(rows.iter().map(|row| row["line"].as_u64().unwrap()));
                    }
                }
            }
            if page["has_more"] == false {
                break;
            }
            request["arguments"]["cursor"] = page["cursor"].clone();
        }
        lines.sort_unstable();
        assert_eq!(lines, vec![2, 3, 4, 5, 6]);
    }
    #[test]
    fn explicit_large_file_streams_tail_and_keeps_stale_cursor_checks() {
        use std::io::Write;
        let fixture = Fixture::new();
        let path = fixture.0.join("large.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        for _ in 0..11 {
            file.write_all(&[b'x'; 1024 * 1024]).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.write_all("before\nα needle\nbetween\nω needle\nafter\n".as_bytes())
            .unwrap();
        drop(file);
        let mut engine = Engine::default();
        let mut request = fixture.request("needle");
        let skipped = engine.query(&request).unwrap();
        assert_eq!(skipped["complete"], false);
        assert_eq!(skipped["skipped"][0]["reason"], "size_limit");
        request["arguments"]["path"] = json!("large.txt");
        request["arguments"]["context"] = json!(1);
        request["arguments"]["limit"] = json!(2);
        let page = engine.query(&request).unwrap();
        assert_eq!(page["complete"], true);
        assert_eq!(page["total_matches"], 2);
        assert_eq!(page["index"]["direct_stream"], true);
        assert_eq!(page["groups"][0]["matches"][0]["text"], "α needle");
        request["arguments"]["cursor"] = page["cursor"].clone();
        let mut changed = request.clone();
        changed["arguments"]["context"] = json!(2);
        assert!(
            engine
                .query(&changed)
                .unwrap_err()
                .contains("different query")
        );
        assert_eq!(engine.query(&request).unwrap()["returned"], 2);
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"changed\n")
            .unwrap();
        assert!(engine.query(&request).unwrap_err().contains("stale"));
    }
    #[test]
    fn excluded_large_file_does_not_add_fallback_omissions() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("text.txt"), "before\nneedle\nafter\n").unwrap();
        let mut request = fixture.request("absent");
        request["arguments"]["path"] = json!("text.txt");
        request["arguments"]["include"] = json!(["*.rs"]);
        request["arguments"]["max_file_bytes"] = json!(2);
        request["arguments"]["fallback"] = json!("fuzzy");
        let page = Engine::default().query(&request).unwrap();
        assert_eq!(page["exact"]["complete"], true);
        assert_eq!(page["candidates"]["complete"], true);
        assert_eq!(page["candidates"]["skipped_count"], 0);
        assert_eq!(page["candidates"]["total_matches"], 0);
    }
    #[test]
    fn streaming_rejects_file_anchors_without_reinterpreting_them_as_line_anchors() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("text.txt"), "before\nneedle\nafter\n").unwrap();
        let mut engine = Engine::default();
        for pattern in [r"\Aneedle", r"needle\z", r"(?-m:^needle$)"] {
            let mut request = fixture.request(pattern);
            request["arguments"]["path"] = json!("text.txt");
            request["arguments"]["mode"] = json!("regex");
            request["arguments"]["max_file_bytes"] = json!(1024);
            assert_eq!(engine.query(&request).unwrap()["total_matches"], 0);
            request["arguments"]["max_file_bytes"] = json!(2);
            let page = engine.query(&request).unwrap();
            assert_eq!(page["complete"], false, "pattern={pattern}");
            assert_eq!(page["total_matches"], Value::Null);
            assert_eq!(
                page["skipped"][0]["reason"],
                "file_anchor_unsupported_for_large_file"
            );
        }
        let mut request = fixture.request("^needle$");
        request["arguments"]["path"] = json!("text.txt");
        request["arguments"]["mode"] = json!("regex");
        request["arguments"]["max_file_bytes"] = json!(2);
        let page = engine.query(&request).unwrap();
        assert_eq!(page["complete"], true);
        assert_eq!(page["total_matches"], 1);
    }
    #[test]
    fn explicit_large_file_reports_multiline_and_oversized_line_limits() {
        let fixture = Fixture::new();
        let path = fixture.0.join("large.txt");
        let mut content = vec![b'x'; 11 * 1024 * 1024];
        content.extend_from_slice(b"\nneedle\n");
        std::fs::write(path, content).unwrap();
        let mut request = fixture.request("needle");
        request["arguments"]["path"] = json!("large.txt");
        let mut engine = Engine::default();
        let page = engine.query(&request).unwrap();
        assert_eq!(page["complete"], false);
        assert_eq!(page["matched_so_far"], 1);
        assert_eq!(page["groups"][0]["matches"][0]["line"], 2);
        assert_eq!(page["skipped"][0]["reason"], "line_size_limit");
        request["arguments"]["mode"] = json!("regex");
        request["arguments"]["pattern"] = json!(r"x\sneedle");
        let page = engine.query(&request).unwrap();
        assert_eq!(page["complete"], false);
        assert_eq!(
            page["skipped"][0]["reason"],
            "multiline_pattern_unsupported_for_large_file"
        );
    }
    #[test]
    fn literal_is_case_sensitive_and_regex_is_explicit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("code.txt"), "a.b\naXb\nA.B\n").unwrap();
        let mut engine = Engine::default();
        let literal = engine.query(&fixture.request("a.b")).unwrap();
        assert_eq!(literal["total_matches"], 1);
        assert_eq!(literal["groups"][0]["matches"][0]["line"], 1);
        let mut regex = fixture.request("a.b");
        regex["arguments"]["mode"] = json!("regex");
        assert_eq!(engine.query(&regex).unwrap()["total_matches"], 2);
        regex["arguments"]["pattern"] = json!("[");
        assert!(engine.query(&regex).unwrap_err().contains("regex"));
    }
    #[test]
    fn same_file_pagination_retains_every_match_and_rejects_changed_query() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("large.txt"), "needle\n".repeat(251)).unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("needle");
        request["arguments"]["limit"] = json!(17);
        let mut lines = vec![];
        loop {
            let page = engine.query(&request).unwrap();
            assert_eq!(page["complete"], true);
            for group in page["groups"].as_array().unwrap() {
                for item in group["matches"].as_array().unwrap() {
                    lines.push(item["line"].as_u64().unwrap());
                }
            }
            if page["has_more"] == false {
                break;
            }
            request["arguments"]["cursor"] = page["cursor"].clone();
            request["call_id"] = json!(format!("next-{}", lines.len()));
            let mut changed = request.clone();
            changed["arguments"]["pattern"] = json!("other");
            assert!(engine.query(&changed).unwrap_err().contains("cursor"));
        }
        assert_eq!(lines, (1..=251).collect::<Vec<_>>());
    }
    #[test]
    fn optional_fuzzy_fallback_keeps_exact_zero_and_scope() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("inside")).unwrap();
        std::fs::write(fixture.0.join("inside/a.txt"), "schema\n").unwrap();
        std::fs::write(fixture.0.join("outside.txt"), "shcema\n").unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("shcema");
        request["arguments"]["path"] = json!("inside");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 0);
        request["arguments"]["fallback"] = json!("fuzzy");
        let page = engine.query(&request).unwrap();
        assert_eq!(page["exact"]["total_matches"], 0);
        assert_eq!(page["exact"]["complete"], true);
        assert_eq!(page["candidates"]["groups"][0]["path"], "a.txt");
    }
    #[test]
    fn changed_files_invalidate_cursors_and_refresh_finds_new_content() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("a.txt"), "old\nold\n").unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("old");
        request["arguments"]["limit"] = json!(1);
        let page = engine.query(&request).unwrap();
        std::fs::write(fixture.0.join("a.txt"), "new content\n").unwrap();
        request["arguments"]["cursor"] = page["cursor"].clone();
        assert!(engine.query(&request).unwrap_err().contains("cursor"));
        let mut refreshed = fixture.request("new content");
        refreshed["arguments"]["refresh"] = json!(true);
        assert_eq!(engine.query(&refreshed).unwrap()["total_matches"], 1);
    }
    #[test]
    fn fuzzy_respects_case_and_glob_can_explicitly_ignore_case() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("UPPER.TXT"), "SCHEMA\n").unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("schema");
        request["arguments"]["mode"] = json!("fuzzy");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 0);
        request["arguments"]["case"] = json!("insensitive");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
        request["name"] = json!("find");
        request["arguments"]["mode"] = json!("glob");
        request["arguments"]["pattern"] = json!("*.txt");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
    }
    #[test]
    fn automatic_update_and_literal_backslashes() {
        let fixture = Fixture::new();
        let file = fixture.0.join("text.txt");
        std::fs::write(&file, "old\n").unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("needle");
        request["arguments"]["path"] = json!("text.txt");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 0);
        std::fs::write(&file, "old\nneedle\\n\\r\\n\n").unwrap();
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
        request["arguments"]["pattern"] = json!(r"needle\n\r\n");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
    }
    #[test]
    fn fuzzy_long_lines_do_not_claim_complete_or_exact_totals() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("long.txt"),
            format!("{}schema\n", "x ".repeat(400)),
        )
        .unwrap();
        let mut request = fixture.request("shcema");
        request["arguments"]["mode"] = json!("fuzzy");
        let page = Engine::default().query(&request).unwrap();
        assert_eq!(page["complete"], false);
        assert_eq!(page["total_matches"], Value::Null);
        assert!(!page["skipped"].as_array().unwrap().is_empty());
    }
    #[test]
    fn ignored_files_do_not_invalidate_pages_but_new_source_files_do() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join(".eden")).unwrap();
        std::fs::write(fixture.0.join(".ignore"), ".eden/\n").unwrap();
        std::fs::write(fixture.0.join("a.txt"), "needle\nneedle\n").unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request("needle");
        request["arguments"]["limit"] = json!(1);
        let page = engine.query(&request).unwrap();
        request["arguments"]["cursor"] = page["cursor"].clone();
        std::fs::write(fixture.0.join(".eden/history.jsonl"), "needle").unwrap();
        assert_eq!(engine.query(&request).unwrap()["returned"], 1);
        std::fs::write(fixture.0.join("new.txt"), "needle").unwrap();
        assert!(engine.query(&request).unwrap_err().contains("cursor"));
    }
    #[test]
    fn history_records_actual_reads_only_and_reopens_for_explicit_ranking() {
        let fixture = Fixture::new();
        let history = fixture.0.join(".history");
        std::fs::write(fixture.0.join(".ignore"), ".history/\n").unwrap();
        std::fs::write(fixture.0.join("a.txt"), "needle").unwrap();
        std::fs::write(fixture.0.join("z.txt"), "needle").unwrap();
        let mut request = fixture.request("needle");
        request["arguments"]["_history_dir"] = json!(history);
        let mut engine = Engine::default();
        assert_eq!(
            engine.query(&request).unwrap()["groups"][0]["path"],
            "a.txt"
        );
        assert!(!history.exists());
        let read = json!({
            "name": "__record_read",
            "cwd": fixture.0,
            "arguments": { "path": "z.txt", "_history_dir": history },
        });
        engine.query(&read).unwrap();
        drop(engine);
        request["arguments"]["ranking"] = json!("history");
        assert_eq!(
            Engine::default().query(&request).unwrap()["groups"][0]["path"],
            "z.txt"
        );
        request["arguments"]
            .as_object_mut()
            .unwrap()
            .remove("ranking");
        assert_eq!(
            Engine::default().query(&request).unwrap()["groups"][0]["path"],
            "a.txt"
        );
    }
    #[test]
    fn skipped_diagnostics_are_bounded_and_pageable() {
        let fixture = Fixture::new();
        for i in 0..35 {
            std::fs::write(fixture.0.join(format!("{i}.txt")), "large").unwrap();
        }
        let mut request = fixture.request("absent");
        request["arguments"]["limit"] = json!(7);
        request["arguments"]["max_file_bytes"] = json!(1);
        let mut engine = Engine::default();
        let mut count = 0;
        loop {
            let page = engine.query(&request).unwrap();
            assert_eq!(page["complete"], false);
            assert_eq!(page["total_matches"], Value::Null);
            let skipped = page["skipped"].as_array().unwrap();
            assert!(skipped.len() <= 7);
            count += skipped.len();
            if page["has_more"] == false {
                break;
            }
            request["arguments"]["cursor"] = page["cursor"].clone();
        }
        assert_eq!(count, 35);
    }
    #[test]
    fn regex_unicode_classes_and_scalars_are_independent_of_case_mode() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("text.txt"), "é\n中\nα\nΑ\n🙂\na\n").unwrap();
        let mut engine = Engine::default();
        for case in ["sensitive", "insensitive"] {
            for (pattern, expected) in [
                (r"^\w$", vec![1, 2, 3, 4, 6]),
                (r"^.$", vec![1, 2, 3, 4, 5, 6]),
                (r"^\p{Greek}$", vec![3, 4]),
            ] {
                let mut request = fixture.request(pattern);
                request["arguments"]["mode"] = json!("regex");
                request["arguments"]["case"] = json!(case);
                let page = engine.query(&request).unwrap();
                let lines: Vec<_> = page["groups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|group| group["matches"].as_array().unwrap())
                    .map(|row| row["line"].as_u64().unwrap())
                    .collect();
                assert_eq!(lines, expected, "pattern={pattern} case={case}");
            }
        }
    }
    #[test]
    fn regex_preserves_escaped_backslash_and_unicode_insensitive_literals() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.0.join("text.txt"),
            "needle\\n\nneedle\néclair\nÉCLAIR\n",
        )
        .unwrap();
        let mut engine = Engine::default();
        let mut request = fixture.request(r"needle\\n");
        request["arguments"]["mode"] = json!("regex");
        let page = engine.query(&request).unwrap();
        assert_eq!(page["groups"][0]["matches"][0]["line"], 1);
        for mode in ["literal", "regex"] {
            request["arguments"]["mode"] = json!(mode);
            request["arguments"]["pattern"] = json!("éclair");
            request["arguments"]["case"] = json!("insensitive");
            assert_eq!(engine.query(&request).unwrap()["total_matches"], 2);
        }
    }
    #[test]
    fn regex_escape_protection_preserves_multiline_and_literal_newlines() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("text.txt"), "needle\nnext\nneedle\\n\n").unwrap();
        let mut request = fixture.request("needle\nnext");
        let mut engine = Engine::default();
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
        request["arguments"]["mode"] = json!("regex");
        request["arguments"]["pattern"] = json!(r"needle\nnext");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
    }
    #[test]
    fn excluded_owned_state_does_not_pollute_git_search_or_invalidate_pages() {
        let fixture = Fixture::new();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&fixture.0)
                .status()
                .unwrap()
                .success()
        );
        let sessions = fixture.0.join(".eden/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let history = sessions.join("active.jsonl");
        std::fs::write(&history, "needle\n").unwrap();
        std::fs::write(fixture.0.join("a.txt"), "needle\nneedle\n").unwrap();
        let mut request = fixture.request("needle");
        request["arguments"]["_excluded_paths"] = json!([sessions]);
        request["arguments"]["limit"] = json!(1);
        let mut engine = Engine::default();
        let first = engine.query(&request).unwrap();
        assert_eq!(first["total_matches"], 2);
        std::fs::write(&history, "needle\nneedle\n").unwrap();
        std::fs::write(sessions.join("binary.bin"), [0u8; 4096]).unwrap();
        request["arguments"]["cursor"] = first["cursor"].clone();
        assert_eq!(engine.query(&request).unwrap()["returned"], 1);
        request["arguments"]
            .as_object_mut()
            .unwrap()
            .remove("cursor");
        let page = engine.query(&request).unwrap();
        assert_eq!(page["total_matches"], 2);
        assert_eq!(page["complete"], true);
        assert_eq!(page["skipped_count"], 0);
    }
    #[test]
    fn renamed_symlink_alias_invalidates_cursor() {
        let fixture = Fixture::new();
        let target = Fixture::new();
        std::fs::write(target.0.join("text.txt"), "needle\nneedle\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target.0, fixture.0.join("old")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target.0, fixture.0.join("old")).unwrap();
        let mut request = fixture.request("needle");
        request["arguments"]["follow_symlinks"] = json!(true);
        request["arguments"]["limit"] = json!(1);
        let mut engine = Engine::default();
        let first = engine.query(&request).unwrap();
        std::fs::rename(fixture.0.join("old"), fixture.0.join("new")).unwrap();
        request["arguments"]["cursor"] = first["cursor"].clone();
        assert!(engine.query(&request).unwrap_err().contains("stale"));
    }
    #[test]
    fn unicode_case_is_consistent_across_search_modes() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("É.TXT"), "É\n").unwrap();
        let mut request = fixture.request("é");
        request["arguments"]["case"] = json!("insensitive");
        request["arguments"]["mode"] = json!("fuzzy");
        let mut engine = Engine::default();
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
        request["name"] = json!("find");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
        request["arguments"]["mode"] = json!("glob");
        request["arguments"]["pattern"] = json!("é.txt");
        assert_eq!(engine.query(&request).unwrap()["total_matches"], 1);
    }
    #[test]
    fn nonexistent_scope_does_not_widen_to_parent() {
        let fixture = Fixture::new();
        let mut request = fixture.request("a");
        request["arguments"]["path"] = json!("missing");
        assert!(Engine::default().query(&request).is_err());
    }
}
