use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use reedline::{Completer, Span, Suggestion};

use crate::{
    kernel::{SharedKernel, lock_kernel},
    profiler::profile_duration,
    theme::{BuiltinTheme, Theme, ThemeHandle, ThemeRegistry, ThemeStyles},
    wl::{
        OPTIONS_QUERY_WL, SYMBOL_COMPLETION_QUERY_WL, SYMBOL_DETAILS_BATCH_QUERY_WL,
        wolfram_function_call, wolfram_string_literal,
    },
    wolfram_syntax::{
        cursor_is_in_wolfram_string, is_qualified_symbol_name, option_context, short_symbol_name,
        string_path_completion_context, symbol_start,
    },
};

const BUILTIN_SYMBOLS: &str = include_str!(concat!(env!("OUT_DIR"), "/builtin_symbols.tsv"));
static BUILTIN_SYMBOL_CACHE: std::sync::OnceLock<Vec<CompletionItem>> = std::sync::OnceLock::new();

#[derive(Clone)]
/// A cached value tagged with the completion epoch it was produced under, so a
/// stale in-flight result (e.g. the kernel gained new definitions while a
/// background fetch was running) is detected by comparison rather than needing
/// coordinated cache invalidation.
pub(crate) enum CacheEntry<V> {
    Pending(u64),
    Ready(u64, V),
}

pub(crate) enum CachePoll<V> {
    Ready(V),
    Pending,
    /// No fresh entry exists; the caller just claimed it (marked `Pending`) and
    /// is responsible for spawning a background fetch.
    Spawn,
}

/// A `Mutex`-backed cache shared between the input thread (reader) and the
/// completion worker thread (writer). Reads and writes are both just a brief
/// lock of an in-memory map, so callers on the input thread never block on IO.
pub(crate) struct AsyncCache<K, V> {
    pub(crate) entries: Arc<Mutex<HashMap<K, CacheEntry<V>>>>,
}

impl<K, V> Clone for AsyncCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
        }
    }
}

impl<K: Eq + std::hash::Hash + Clone, V: Clone> AsyncCache<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn poll_or_claim(&self, key: &K, epoch: u64) -> CachePoll<V> {
        let mut entries = match self.entries.try_lock() {
            Ok(entries) => entries,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return CachePoll::Pending,
        };
        match entries.get(key) {
            Some(CacheEntry::Ready(entry_epoch, value)) if *entry_epoch == epoch => {
                CachePoll::Ready(value.clone())
            }
            Some(CacheEntry::Pending(entry_epoch)) if *entry_epoch == epoch => CachePoll::Pending,
            _ => {
                entries.insert(key.clone(), CacheEntry::Pending(epoch));
                CachePoll::Spawn
            }
        }
    }

    pub(crate) fn insert(&self, key: K, epoch: u64, value: V) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(key, CacheEntry::Ready(epoch, value));
    }
}

/// The blocking, kernel-touching half of completion. Calls here can take
/// anywhere from a few milliseconds to multiple seconds (rendering a usage
/// message for many symbols at once is the expensive case), so this trait must
/// only ever be driven from the background completion worker thread, never
/// from `Completer::complete` on reedline's input thread.
pub(crate) trait KernelBackend: Send + Sync {
    fn load_symbols_for_prefix(&self, prefix: &str) -> Result<Vec<CompletionItem>>;
    fn load_symbol_details(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, CompletionItemDetails>>;
    fn load_options(&self, head: &str) -> Result<Vec<String>>;
}

pub(crate) struct KernelBackendImpl {
    kernel: SharedKernel,
}

impl KernelBackendImpl {
    fn query_lines(&self, code: &str) -> Result<Vec<String>> {
        lock_kernel(&self.kernel)?.query_lines(code)
    }
}

impl KernelBackend for KernelBackendImpl {
    fn load_symbols_for_prefix(&self, prefix: &str) -> Result<Vec<CompletionItem>> {
        let code = symbol_completion_query(prefix);
        let lines = self
            .query_lines(&code)
            .with_context(|| format!("failed to load names for prefix {prefix:?}"))?;
        Ok(parse_completion_items(lines))
    }

    fn load_symbol_details(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, CompletionItemDetails>> {
        let code = symbol_details_batch_query(symbols);
        let lines = self
            .query_lines(&code)
            .context("failed to load symbol usage batch")?;
        Ok(parse_symbol_details_batch(lines))
    }

    fn load_options(&self, head: &str) -> Result<Vec<String>> {
        let code = wolfram_function_call(OPTIONS_QUERY_WL, &[wolfram_string_literal(head)]);
        self.query_lines(&code)
            .with_context(|| format!("failed to load options for {head}"))
    }
}

enum CompletionJob {
    Symbols { prefix: String, epoch: u64 },
    Options { head: String, epoch: u64 },
}

enum CompletionDetailJob {
    Details { symbols: Vec<String>, epoch: u64 },
}

fn spawn_completion_worker(
    backend: Arc<dyn KernelBackend>,
    symbols_cache: AsyncCache<String, Vec<CompletionItem>>,
    options_cache: AsyncCache<String, Vec<String>>,
) -> mpsc::Sender<CompletionJob> {
    let (sender, receiver) = mpsc::channel::<CompletionJob>();
    thread::spawn(move || {
        for job in receiver {
            match job {
                CompletionJob::Symbols { prefix, epoch } => {
                    let start = Instant::now();
                    let items = backend
                        .load_symbols_for_prefix(&prefix)
                        .unwrap_or_else(|err| {
                            eprintln!(
                                "warning: symbol completion disabled for {prefix:?}: {err:#}"
                            );
                            Vec::new()
                        });
                    profile_duration(
                        "worker.symbols",
                        start.elapsed(),
                        format!("prefix={prefix:?} count={}", items.len()),
                    );
                    symbols_cache.insert(prefix, epoch, items);
                }
                CompletionJob::Options { head, epoch } => {
                    let start = Instant::now();
                    let options = backend.load_options(&head).unwrap_or_else(|err| {
                        eprintln!("warning: option completion disabled for {head}: {err:#}");
                        Vec::new()
                    });
                    profile_duration(
                        "worker.options",
                        start.elapsed(),
                        format!("head={head} count={}", options.len()),
                    );
                    options_cache.insert(head, epoch, options);
                }
            }
        }
    });
    sender
}

fn spawn_completion_detail_worker(
    backend: Arc<dyn KernelBackend>,
    details_cache: AsyncCache<String, CompletionItemDetails>,
) -> mpsc::Sender<CompletionDetailJob> {
    let (sender, receiver) = mpsc::channel::<CompletionDetailJob>();
    thread::spawn(move || {
        for job in receiver {
            match job {
                CompletionDetailJob::Details { symbols, epoch } => {
                    let start = Instant::now();
                    let count = symbols.len();
                    match backend.load_symbol_details(&symbols) {
                        Ok(mut details) => {
                            profile_duration(
                                "worker.details",
                                start.elapsed(),
                                format!("count={count} ready={}", details.len()),
                            );
                            for symbol in symbols {
                                let entry =
                                    details.remove(&symbol).unwrap_or(CompletionItemDetails {
                                        context: None,
                                        usage: None,
                                    });
                                details_cache.insert(symbol, epoch, entry);
                            }
                        }
                        Err(err) => {
                            profile_duration(
                                "worker.details.error",
                                start.elapsed(),
                                format!("count={count}"),
                            );
                            eprintln!("warning: symbol details disabled: {err:#}");
                            for symbol in symbols {
                                details_cache.insert(
                                    symbol,
                                    epoch,
                                    CompletionItemDetails {
                                        context: None,
                                        usage: None,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    });
    sender
}

pub(crate) struct CompletionSource {
    pub(crate) epoch: Arc<AtomicU64>,
    pub(crate) user_symbols: Arc<Mutex<HashSet<String>>>,
    job_sender: mpsc::Sender<CompletionJob>,
    detail_job_sender: mpsc::Sender<CompletionDetailJob>,
    pub(crate) symbols_cache: AsyncCache<String, Vec<CompletionItem>>,
    pub(crate) details_cache: AsyncCache<String, CompletionItemDetails>,
    pub(crate) options_cache: AsyncCache<String, Vec<String>>,
}

impl CompletionSource {
    pub(crate) fn new(
        kernel: SharedKernel,
        epoch: Arc<AtomicU64>,
        user_symbols: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self::with_backend(Arc::new(KernelBackendImpl { kernel }), epoch, user_symbols)
    }

    pub(crate) fn with_backend(
        backend: Arc<dyn KernelBackend>,
        epoch: Arc<AtomicU64>,
        user_symbols: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        let symbols_cache = AsyncCache::new();
        let details_cache = AsyncCache::new();
        let options_cache = AsyncCache::new();
        let job_sender = spawn_completion_worker(
            backend.clone(),
            symbols_cache.clone(),
            options_cache.clone(),
        );
        let detail_job_sender =
            spawn_completion_detail_worker(backend.clone(), details_cache.clone());
        Self {
            epoch,
            user_symbols,
            job_sender,
            detail_job_sender,
            symbols_cache,
            details_cache,
            options_cache,
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Never touches the kernel directly. Built-ins resolve locally and
    /// instantly; kernel-sourced names for a not-yet-seen prefix are fetched on
    /// the background worker and simply aren't included until a later call
    /// (typically the next keystroke) finds them cached.
    pub(crate) fn symbols_for_prefix(&self, prefix: &str) -> Vec<CompletionItem> {
        let start = Instant::now();
        if !is_qualified_symbol_name(prefix) {
            return Vec::new();
        }

        if prefix.starts_with("System`") {
            let items = builtin_symbols_for_prefix(prefix);
            profile_duration(
                "source.symbols.builtin",
                start.elapsed(),
                format!("prefix={prefix:?} count={}", items.len()),
            );
            return items;
        }

        let epoch = self.epoch();
        let mut items = self.local_user_symbols_for_prefix(prefix);
        let query_prefix = symbol_query_prefix(prefix);
        let cache_start = Instant::now();
        items.extend(
            match self.symbols_cache.poll_or_claim(&query_prefix, epoch) {
                CachePoll::Ready(items) => items,
                CachePoll::Pending => Vec::new(),
                CachePoll::Spawn => {
                    let _ = self.job_sender.send(CompletionJob::Symbols {
                        prefix: query_prefix.clone(),
                        epoch,
                    });
                    Vec::new()
                }
            },
        );
        profile_duration(
            "source.symbols.kernel_cache",
            cache_start.elapsed(),
            format!(
                "prefix={prefix:?} query_prefix={query_prefix:?} count={}",
                items.len()
            ),
        );

        if !prefix.contains('`') {
            let builtin_start = Instant::now();
            items.extend(builtin_symbols_for_prefix(prefix));
            profile_duration(
                "source.symbols.builtins",
                builtin_start.elapsed(),
                format!("prefix={prefix:?} count={}", items.len()),
            );
        }

        profile_duration(
            "source.symbols",
            start.elapsed(),
            format!("prefix={prefix:?} count={}", items.len()),
        );
        items
    }

    pub(crate) fn local_user_symbols_for_prefix(&self, prefix: &str) -> Vec<CompletionItem> {
        if prefix.contains('`') {
            return Vec::new();
        }

        let symbols = match self.user_symbols.try_lock() {
            Ok(symbols) => symbols,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Vec::new(),
        };

        symbols
            .iter()
            .filter(|symbol| fuzzy_matches(symbol, prefix))
            .map(|symbol| CompletionItem {
                value: symbol.clone(),
                kind: if symbol.ends_with('`') {
                    CompletionKind::Context
                } else {
                    CompletionKind::Symbol
                },
                frequency: None,
                context: Some(if symbol.ends_with('`') {
                    symbol.clone()
                } else {
                    "Global`".to_string()
                }),
            })
            .collect()
    }

    /// Returns whichever of `symbols` already have cached usage/context info;
    /// queues a single batched background fetch for the rest. Keep `symbols`
    /// bounded: it becomes one kernel round trip, and rendering usage messages
    /// is the single most expensive thing this program asks the kernel to do.
    pub(crate) fn usage_details(
        &self,
        symbols: &[String],
    ) -> HashMap<String, CompletionItemDetails> {
        let start = Instant::now();
        let epoch = self.epoch();
        let mut ready = HashMap::new();
        let mut to_spawn = Vec::new();
        for symbol in symbols {
            match self.details_cache.poll_or_claim(symbol, epoch) {
                CachePoll::Ready(details) => {
                    ready.insert(symbol.clone(), details);
                }
                CachePoll::Pending => {}
                CachePoll::Spawn => to_spawn.push(symbol.clone()),
            }
        }

        let spawned = to_spawn.len();
        if !to_spawn.is_empty() {
            let _ = self.detail_job_sender.send(CompletionDetailJob::Details {
                symbols: to_spawn,
                epoch,
            });
        }

        profile_duration(
            "source.usage_details",
            start.elapsed(),
            format!(
                "requested={} ready={} spawned={}",
                symbols.len(),
                ready.len(),
                spawned
            ),
        );
        ready
    }

    pub(crate) fn options_for(&self, head: &str) -> Vec<String> {
        let start = Instant::now();
        if !is_qualified_symbol_name(head) {
            return Vec::new();
        }

        let epoch = self.epoch();
        let options = match self.options_cache.poll_or_claim(&head.to_string(), epoch) {
            CachePoll::Ready(options) => options,
            CachePoll::Pending => Vec::new(),
            CachePoll::Spawn => {
                let _ = self.job_sender.send(CompletionJob::Options {
                    head: head.to_string(),
                    epoch,
                });
                Vec::new()
            }
        };
        profile_duration(
            "source.options",
            start.elapsed(),
            format!("head={head} count={}", options.len()),
        );
        options
    }
}

pub(crate) fn symbol_query_prefix(prefix: &str) -> String {
    if let Some(context_end) = prefix.rfind('`') {
        return prefix[..=context_end].to_string();
    }

    prefix
        .char_indices()
        .nth(2)
        .map_or(prefix, |(idx, _)| &prefix[..idx])
        .to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionItem {
    pub(crate) value: String,
    pub(crate) kind: CompletionKind,
    pub(crate) frequency: Option<usize>,
    pub(crate) context: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    Symbol,
    Context,
}

/// Lists candidate names (and their context) for `prefix`. Deliberately does
/// NOT compute usage messages here: rendering `::usage` text for every match
/// is the expensive part of a completion query (~40ms/symbol for a query
/// spanning many symbols, vs ~1ms/symbol for name+context alone), so it would
/// turn a query matching dozens of names into a multi-second call. Usage text
/// is fetched separately, in small batches, via `symbol_details_batch_query`.
pub(crate) fn symbol_completion_query(prefix: &str) -> String {
    wolfram_function_call(
        SYMBOL_COMPLETION_QUERY_WL,
        &[wolfram_string_literal(prefix)],
    )
}

/// Fetches context + usage for a small, explicit list of symbol names in a
/// single kernel round trip (as opposed to one round trip per symbol).
pub(crate) fn symbol_details_batch_query(symbols: &[String]) -> String {
    let names = symbols
        .iter()
        .map(|symbol| wolfram_string_literal(symbol))
        .collect::<Vec<_>>()
        .join(", ");
    wolfram_function_call(SYMBOL_DETAILS_BATCH_QUERY_WL, &[format!("{{{names}}}")])
}

pub(crate) fn builtin_symbol_names() -> impl Iterator<Item = String> {
    builtin_symbol_cache()
        .iter()
        .map(|item| item.value.clone())
        .collect::<Vec<_>>()
        .into_iter()
}

pub(crate) fn builtin_symbol_cache() -> &'static [CompletionItem] {
    BUILTIN_SYMBOL_CACHE.get_or_init(|| {
        BUILTIN_SYMBOLS
            .lines()
            .filter_map(|line| {
                let (value, frequency) = line.split_once('\t')?;
                let frequency = frequency.parse().ok();
                Some(CompletionItem {
                    value: value.to_string(),
                    kind: CompletionKind::Symbol,
                    frequency,
                    context: Some("System`".to_string()),
                })
            })
            .collect()
    })
}

pub(crate) fn builtin_symbols_for_prefix(prefix: &str) -> Vec<CompletionItem> {
    let short_prefix = short_symbol_name(prefix);
    let mut items: Vec<_> = builtin_symbol_cache()
        .iter()
        .filter(|item| builtin_symbol_matches(&item.value, short_prefix))
        .take(MAX_COMPLETION_SUGGESTIONS)
        .cloned()
        .collect();

    if prefix.starts_with("System`") {
        for item in &mut items {
            item.value = format!("System`{}", item.value);
        }
    }

    items
}

pub(crate) fn builtin_symbol_matches(candidate: &str, prefix: &str) -> bool {
    if prefix.len() < 3 {
        return starts_with_ignore_ascii_case(candidate, prefix);
    }

    fuzzy_matches(candidate, prefix)
}

pub(crate) fn parse_completion_items(lines: Vec<String>) -> Vec<CompletionItem> {
    lines
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let kind = fields.next()?;
            let value = fields.next()?;
            let kind = match kind {
                "symbol" => CompletionKind::Symbol,
                "context" => CompletionKind::Context,
                _ => return None,
            };
            let frequency = fields.next().and_then(|frequency| frequency.parse().ok());
            let context = fields.next().filter(|field| !field.is_empty());
            Some(CompletionItem {
                value: value.to_string(),
                kind,
                frequency,
                context: context.map(str::to_string),
            })
        })
        .collect()
}

pub(crate) fn parse_symbol_details_batch(
    lines: Vec<String>,
) -> HashMap<String, CompletionItemDetails> {
    lines
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_string();
            let context = fields.next().filter(|field| !field.is_empty());
            let usage = fields.next().filter(|field| !field.is_empty());
            Some((
                name,
                CompletionItemDetails {
                    context: context.map(str::to_string),
                    usage: usage.map(str::to_string),
                },
            ))
        })
        .collect()
}

pub(crate) struct WolframCompleter {
    pub(crate) source: CompletionSource,
    pub(crate) theme: ThemeHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionItemDetails {
    pub(crate) context: Option<String>,
    pub(crate) usage: Option<String>,
}

impl WolframCompleter {
    pub(crate) fn new(source: CompletionSource, theme: ThemeHandle) -> Self {
        Self { source, theme }
    }
}

impl Completer for WolframCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let complete_start = Instant::now();
        let styles = self.theme.current().styles();
        if let Some(suggestions) =
            command_completion_suggestions(line, pos, styles, self.theme.registry())
        {
            profile_duration(
                "complete.command",
                complete_start.elapsed(),
                format!(
                    "line_len={} pos={pos} count={}",
                    line.len(),
                    suggestions.len()
                ),
            );
            return suggestions;
        }

        if cursor_is_in_wolfram_string(line, pos) {
            let suggestions = file_completion_suggestions(line, pos, styles);
            profile_duration(
                "complete.string_filesystem",
                complete_start.elapsed(),
                format!(
                    "line_len={} pos={pos} count={}",
                    line.len(),
                    suggestions.len()
                ),
            );
            return suggestions;
        }

        let start = symbol_start(line, pos);
        let prefix = &line[start..pos];
        let short_prefix = short_symbol_name(prefix);
        let option_head = option_context(line, start);

        if short_prefix.is_empty() && !prefix.ends_with('`') {
            profile_duration(
                "complete.empty",
                complete_start.elapsed(),
                format!("line_len={} pos={pos}", line.len()),
            );
            return Vec::new();
        }

        let mut suggestions = Vec::new();

        let symbols_start = Instant::now();
        let symbols = self.source.symbols_for_prefix(prefix);
        profile_duration(
            "complete.load_symbols",
            symbols_start.elapsed(),
            format!("prefix={prefix:?} count={}", symbols.len()),
        );
        let symbol_suggestions_start = Instant::now();
        suggestions.extend(symbol_suggestions(
            &symbols,
            prefix,
            start,
            pos,
            &self.source,
            styles,
        ));
        profile_duration(
            "complete.symbol_suggestions",
            symbol_suggestions_start.elapsed(),
            format!("prefix={prefix:?} total={}", suggestions.len()),
        );

        if let Some(head) = option_head {
            let options_start = Instant::now();
            let options = self.source.options_for(&head);
            profile_duration(
                "complete.load_options",
                options_start.elapsed(),
                format!("head={head} count={}", options.len()),
            );
            suggestions.extend(option_suggestions(
                &options,
                short_prefix,
                start,
                pos,
                &head,
                styles,
            ));
        }

        suggestions.sort_by(|left, right| {
            completion_sort_key(left, short_prefix)
                .cmp(&completion_sort_key(right, short_prefix))
                .then_with(|| left.value.cmp(&right.value))
        });
        suggestions.dedup_by(|left, right| left.value == right.value);
        suggestions.truncate(MAX_COMPLETION_SUGGESTIONS);
        for suggestion in &mut suggestions {
            suggestion.extra = None;
        }
        profile_duration(
            "complete.total",
            complete_start.elapsed(),
            format!(
                "line_len={} pos={pos} prefix={prefix:?} count={}",
                line.len(),
                suggestions.len()
            ),
        );
        suggestions
    }
}

pub(crate) fn file_completion_suggestions(
    line: &str,
    pos: usize,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = env::var_os("HOME").map(PathBuf::from);
    file_completion_suggestions_from(line, pos, &cwd, home.as_deref(), styles)
}

pub(crate) fn file_completion_suggestions_from(
    line: &str,
    pos: usize,
    base_dir: &Path,
    home_dir: Option<&Path>,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    let Some(context) = string_path_completion_context(line, pos) else {
        return Vec::new();
    };
    let Some(raw_fragment) = line.get(context.start..context.end) else {
        return Vec::new();
    };
    let fragment = unescape_wolfram_string_fragment(raw_fragment);
    let Some((query_dir, replacement_prefix, entry_prefix)) =
        file_completion_query_parts(&fragment, base_dir, home_dir)
    else {
        return Vec::new();
    };

    let entries = match fs::read_dir(&query_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let include_hidden = entry_prefix.starts_with('.');
    let mut candidates = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&entry_prefix) || (!include_hidden && name.starts_with('.')) {
                return None;
            }
            let is_dir = entry.path().is_dir();
            Some((name, is_dir))
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0.to_lowercase().cmp(&right.0.to_lowercase()))
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates.truncate(MAX_COMPLETION_SUGGESTIONS);

    candidates
        .into_iter()
        .map(|(name, is_dir)| {
            let completed = format!(
                "{replacement_prefix}{name}{}",
                if is_dir { "/" } else { "" }
            );
            Suggestion {
                value: escape_wolfram_string_fragment(&completed),
                description: Some(if is_dir { "directory" } else { "file" }.to_string()),
                style: Some(if is_dir {
                    styles.completion_directory
                } else {
                    styles.completion_file
                }),
                extra: None,
                span: Span {
                    start: context.start,
                    end: context.end,
                },
                append_whitespace: false,
            }
        })
        .collect()
}

fn file_completion_query_parts(
    fragment: &str,
    base_dir: &Path,
    home_dir: Option<&Path>,
) -> Option<(PathBuf, String, String)> {
    let slash = fragment.rfind('/')?;
    let replacement_prefix = fragment[..=slash].to_string();
    let entry_prefix = fragment[slash + 1..].to_string();
    let query_dir = completion_dir_for_fragment(&replacement_prefix, base_dir, home_dir)?;
    Some((query_dir, replacement_prefix, entry_prefix))
}

fn completion_dir_for_fragment(
    dir_fragment: &str,
    base_dir: &Path,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    if dir_fragment.starts_with("~/") {
        return home_dir.map(|home| home.join(&dir_fragment[2..]));
    }
    if dir_fragment.starts_with('/') {
        return Some(PathBuf::from(dir_fragment));
    }
    Some(base_dir.join(dir_fragment))
}

fn unescape_wolfram_string_fragment(fragment: &str) -> String {
    let mut unescaped = String::new();
    let mut chars = fragment.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            unescaped.push(ch);
            continue;
        }

        match chars.next() {
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            Some('t') => unescaped.push('\t'),
            Some(next) => unescaped.push(next),
            None => unescaped.push('\\'),
        }
    }
    unescaped
}

fn escape_wolfram_string_fragment(fragment: &str) -> String {
    let mut escaped = String::new();
    for ch in fragment.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn command_completion_suggestions(
    line: &str,
    pos: usize,
    styles: ThemeStyles,
    registry: &ThemeRegistry,
) -> Option<Vec<Suggestion>> {
    if !line.starts_with(':') || pos > line.len() {
        return None;
    }

    let before_cursor = &line[..pos];
    let command_line = &before_cursor[1..];
    let command_start = command_line
        .find(|ch: char| !ch.is_whitespace())
        .map_or(pos, |idx| idx + 1);
    let command_and_args = command_line.trim_start();

    if command_and_args.is_empty() || !command_and_args.contains(char::is_whitespace) {
        let prefix = command_and_args;
        return Some(command_name_suggestions(prefix, command_start, pos, styles));
    }

    let mut parts = command_and_args.split_whitespace();
    let command = parts.next().unwrap_or_default().to_lowercase();
    let argument_start = before_cursor
        .rfind(char::is_whitespace)
        .map_or(pos, |idx| idx + 1);
    let argument_prefix = &before_cursor[argument_start..pos];
    let has_trailing_space = before_cursor
        .chars()
        .last()
        .is_some_and(char::is_whitespace);

    match command.as_str() {
        "config" | "conf" if parts.next().is_none() || !has_trailing_space => Some(
            config_arg_suggestions(argument_prefix, argument_start, pos, styles),
        ),
        "theme" if parts.next().is_none() || !has_trailing_space => Some(theme_arg_suggestions(
            argument_prefix,
            argument_start,
            pos,
            styles,
            registry,
        )),
        _ => Some(Vec::new()),
    }
}

pub(crate) fn command_name_suggestions(
    prefix: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    [
        ("clear", "Clear the console"),
        ("config", "Show or edit config file"),
        ("conf", "Show or edit config file"),
        ("help", "Show REPL commands"),
        ("history", "Open the history browser"),
        ("theme", "Change syntax highlighting theme"),
        ("quit", "Quit the REPL"),
    ]
    .into_iter()
    .filter(|(value, _)| command_candidate_matches(value, prefix))
    .map(|(value, description)| command_suggestion(value, description, start, end, styles))
    .collect()
}

pub(crate) fn config_arg_suggestions(
    prefix: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    [("edit", "Open the config file in $EDITOR")]
        .into_iter()
        .filter(|(value, _)| command_candidate_matches(value, prefix))
        .map(|(value, description)| command_suggestion(value, description, start, end, styles))
        .collect()
}

pub(crate) fn theme_arg_suggestions(
    prefix: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
    registry: &ThemeRegistry,
) -> Vec<Suggestion> {
    let mut suggestions = registry
        .themes()
        .iter()
        .filter(|theme| prefix.is_empty() || command_candidate_matches(theme.name(), prefix))
        .map(|theme| {
            command_suggestion(theme.name(), &theme_description(theme), start, end, styles)
        })
        .collect::<Vec<_>>();

    let aliases = [
        (
            "solarized-dark",
            theme_description(&Theme::builtin(BuiltinTheme::Solarized)),
        ),
        (
            "gruvbox-dark",
            theme_description(&Theme::builtin(BuiltinTheme::Gruvbox)),
        ),
        ("none", theme_description(&Theme::plain())),
        ("no-color", theme_description(&Theme::plain())),
        ("nocolor", theme_description(&Theme::plain())),
        ("list", "Browse available themes".to_string()),
        ("ls", "Browse available themes".to_string()),
        ("browse", "Browse available themes".to_string()),
        ("show", "Show the current theme".to_string()),
        ("current", "Show the current theme".to_string()),
    ];

    suggestions.extend(
        aliases
            .into_iter()
            .filter(|(value, _)| {
                if prefix.is_empty() {
                    matches!(*value, "list" | "show")
                } else {
                    command_candidate_matches(value, prefix)
                }
            })
            .map(|(value, description)| {
                command_suggestion(value, &description, start, end, styles)
            }),
    );

    suggestions
}

pub(crate) fn theme_description(theme: &Theme) -> String {
    if theme.is_plain() {
        "Disable syntax highlighting colors".to_string()
    } else {
        format!("Use the {} syntax highlighting theme", theme.name())
    }
}

pub(crate) fn command_candidate_matches(candidate: &str, prefix: &str) -> bool {
    starts_with_ignore_ascii_case(candidate, prefix)
}

pub(crate) fn command_suggestion(
    value: &str,
    description: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Suggestion {
    Suggestion {
        value: value.to_string(),
        description: Some(description.to_string()),
        style: Some(styles.completion_command),
        extra: None,
        span: Span { start, end },
        append_whitespace: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CompletionSourceKind {
    Global,
    Option,
    System,
    OtherSingleNameContext,
    MultiNameContext,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompletionSortMetadata {
    pub(crate) source: CompletionSourceKind,
    pub(crate) frequency: Option<usize>,
}

impl CompletionSortMetadata {
    pub(crate) fn serialize(self) -> String {
        let source = match self.source {
            CompletionSourceKind::Global => "global",
            CompletionSourceKind::Option => "option",
            CompletionSourceKind::System => "system",
            CompletionSourceKind::OtherSingleNameContext => "other-single-name-context",
            CompletionSourceKind::MultiNameContext => "multi-name-context",
            CompletionSourceKind::Other => "other",
        };
        let frequency = self
            .frequency
            .map(|value| value.to_string())
            .unwrap_or_default();
        format!("source={source};frequency={frequency}")
    }

    pub(crate) fn parse(value: &str) -> Self {
        let mut source = CompletionSourceKind::Other;
        let mut frequency = None;

        for part in value.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "source" => {
                    source = match value {
                        "global" | "user" => CompletionSourceKind::Global,
                        "option" => CompletionSourceKind::Option,
                        "system" | "builtin" => CompletionSourceKind::System,
                        "other-single-name-context" => CompletionSourceKind::OtherSingleNameContext,
                        "multi-name-context" => CompletionSourceKind::MultiNameContext,
                        _ => CompletionSourceKind::Other,
                    }
                }
                "frequency" => frequency = value.parse().ok(),
                _ => {}
            }
        }

        Self { source, frequency }
    }
}

pub(crate) fn completion_sort_key(
    suggestion: &Suggestion,
    short_prefix: &str,
) -> (usize, Option<usize>) {
    let metadata = suggestion
        .extra
        .as_ref()
        .and_then(|extra| extra.first())
        .map(|value| CompletionSortMetadata::parse(value))
        .unwrap_or(CompletionSortMetadata {
            source: CompletionSourceKind::Other,
            frequency: None,
        });
    let source_priority = match metadata.source {
        CompletionSourceKind::Global => 0,
        CompletionSourceKind::Option => 1,
        CompletionSourceKind::System => 2,
        CompletionSourceKind::OtherSingleNameContext => 3,
        CompletionSourceKind::MultiNameContext => 4,
        CompletionSourceKind::Other => 5,
    };
    let score =
        completion_score(&suggestion.value, short_prefix, metadata.frequency).unwrap_or(usize::MAX);
    (source_priority, Some(score))
}

/// Keep the result set handed to reedline bounded. The menu displays six rows,
/// but layout still scans the whole returned vector on every repaint.
pub(crate) const MAX_COMPLETION_SUGGESTIONS: usize = 120;

/// How many not-yet-known symbols get an eager background usage lookup per
/// `complete()` call. Usage text is only queued for narrow result sets; broad
/// prefixes should not kick off long WSTP work while the user is still typing.
pub(crate) const USAGE_LOOKAHEAD: usize = 6;
pub(crate) const USAGE_DETAIL_MAX_MATCHES: usize = 60;

pub(crate) fn symbol_suggestions(
    symbols: &[CompletionItem],
    prefix: &str,
    start: usize,
    end: usize,
    source: &CompletionSource,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    let context_prefix = prefix.rfind('`').map(|idx| &prefix[..=idx]);
    let matches: Vec<(&CompletionItem, String)> = symbols
        .iter()
        .filter_map(|candidate| {
            let value =
                if candidate.kind == CompletionKind::Symbol && !candidate.value.contains('`') {
                    context_prefix
                        .map(|context| format!("{context}{}", candidate.value))
                        .unwrap_or_else(|| candidate.value.clone())
                } else {
                    candidate.value.clone()
                };
            let match_pattern = if candidate.kind == CompletionKind::Context {
                prefix
            } else {
                short_symbol_name(prefix)
            };
            let match_value = if candidate.kind == CompletionKind::Context {
                value.as_str()
            } else {
                short_symbol_name(&value)
            };

            fuzzy_matches(match_value, match_pattern).then_some((candidate, value))
        })
        .collect();

    let wanted: Vec<String> = if matches.len() <= USAGE_DETAIL_MAX_MATCHES {
        matches
            .iter()
            .filter(|(candidate, _)| candidate.kind == CompletionKind::Symbol)
            .take(USAGE_LOOKAHEAD)
            .map(|(_, value)| value.clone())
            .collect()
    } else {
        Vec::new()
    };
    let usage = if wanted.is_empty() {
        HashMap::new()
    } else {
        source.usage_details(&wanted)
    };

    matches
        .into_iter()
        .map(|(candidate, value)| {
            let details = match candidate.kind {
                CompletionKind::Symbol => {
                    usage.get(&value).cloned().unwrap_or(CompletionItemDetails {
                        context: candidate.context.clone(),
                        usage: None,
                    })
                }
                CompletionKind::Context => CompletionItemDetails {
                    context: candidate.context.clone(),
                    usage: None,
                },
            };

            let source_kind = completion_source_kind(candidate);
            let (description, style) = match candidate.kind {
                CompletionKind::Symbol => (
                    symbol_completion_description(&details),
                    symbol_completion_style(source_kind, styles),
                ),
                CompletionKind::Context => (
                    context_completion_description(&details),
                    styles.completion_context,
                ),
            };

            Suggestion {
                value,
                description: Some(description),
                style: Some(style),
                extra: Some(vec![
                    CompletionSortMetadata {
                        source: source_kind,
                        frequency: candidate.frequency,
                    }
                    .serialize(),
                ]),
                span: Span { start, end },
                append_whitespace: false,
            }
        })
        .collect()
}

pub(crate) fn symbol_completion_style(
    source_kind: CompletionSourceKind,
    styles: ThemeStyles,
) -> nu_ansi_term::Style {
    match source_kind {
        CompletionSourceKind::Global => styles.completion_global_symbol,
        CompletionSourceKind::System => styles.completion_symbol,
        CompletionSourceKind::OtherSingleNameContext
        | CompletionSourceKind::MultiNameContext
        | CompletionSourceKind::Other
        | CompletionSourceKind::Option => styles.completion_user_symbol,
    }
}

pub(crate) fn completion_source_kind(candidate: &CompletionItem) -> CompletionSourceKind {
    completion_context_source_kind(candidate.context.as_deref())
}

pub(crate) fn completion_context_source_kind(context: Option<&str>) -> CompletionSourceKind {
    match context {
        Some("Global`") => CompletionSourceKind::Global,
        Some("System`") => CompletionSourceKind::System,
        Some(context) if is_single_name_context(context) => {
            CompletionSourceKind::OtherSingleNameContext
        }
        Some(_) => CompletionSourceKind::MultiNameContext,
        None => CompletionSourceKind::Other,
    }
}

pub(crate) fn is_single_name_context(context: &str) -> bool {
    let mut segments = context
        .trim_end_matches('`')
        .split('`')
        .filter(|segment| !segment.is_empty());
    segments.next().is_some() && segments.next().is_none()
}

pub(crate) fn symbol_completion_description(details: &CompletionItemDetails) -> String {
    let mut parts = vec!["symbol".to_string()];

    if let Some(context) = &details.context {
        parts.push(format!("Context: {context}"));
    }

    if let Some(usage) = &details.usage {
        parts.push(format!("Usage: {usage}"));
    }

    parts.join("\n")
}

pub(crate) fn context_completion_description(details: &CompletionItemDetails) -> String {
    details
        .context
        .as_ref()
        .map(|context| format!("context\nContext: {context}"))
        .unwrap_or_else(|| "context".to_string())
}

pub(crate) fn option_suggestions(
    options: &[String],
    prefix: &str,
    start: usize,
    end: usize,
    head: &str,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    options
        .iter()
        .filter(|candidate| fuzzy_matches(candidate, prefix))
        .map(|candidate| Suggestion {
            value: candidate.clone(),
            description: Some(format!("option for {head}")),
            style: Some(styles.completion_option),
            extra: Some(vec![
                CompletionSortMetadata {
                    source: CompletionSourceKind::Option,
                    frequency: None,
                }
                .serialize(),
            ]),
            span: Span { start, end },
            append_whitespace: false,
        })
        .collect()
}

pub(crate) fn fuzzy_matches(candidate: &str, pattern: &str) -> bool {
    completion_score(candidate, pattern, None).is_some()
}

pub(crate) fn completion_score(
    candidate: &str,
    pattern: &str,
    frequency: Option<usize>,
) -> Option<usize> {
    let frequency_bonus = frequency.unwrap_or(0);
    let weigh = |score: usize| score.saturating_sub(frequency_bonus);

    if pattern.is_empty() {
        return Some(weigh(100));
    }

    if starts_with_ignore_ascii_case(candidate, pattern) {
        return Some(weigh(100));
    }

    if acronym_matches(candidate, pattern) {
        return Some(weigh(200 + candidate.chars().count()));
    }

    if prefix_plus_word_initials_matches(candidate, pattern) {
        return Some(weigh(250 + candidate.chars().count()));
    }

    if pattern.chars().count() < 3 {
        return None;
    }

    fuzzy_subsequence_score(candidate, pattern).map(|score| weigh(300 + score))
}

pub(crate) fn starts_with_ignore_ascii_case(candidate: &str, prefix: &str) -> bool {
    candidate
        .get(..prefix.len())
        .is_some_and(|candidate_prefix| candidate_prefix.eq_ignore_ascii_case(prefix))
}

fn fuzzy_subsequence_score(candidate: &str, pattern: &str) -> Option<usize> {
    if candidate.is_ascii() && pattern.is_ascii() {
        return fuzzy_ascii_subsequence_score(candidate.as_bytes(), pattern.as_bytes());
    }

    let candidate: Vec<char> = candidate.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut last_match: Option<usize> = None;
    let mut search_from = 0;
    let mut skipped = 0;

    for wanted in &pattern {
        let found = candidate
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(idx, ch)| ch.eq_ignore_ascii_case(wanted).then_some(idx))?;

        if let Some(last) = last_match {
            skipped += found.saturating_sub(last + 1);
        }

        last_match = Some(found);
        search_from = found + 1;
    }

    let end = last_match?;
    skipped += candidate.len().saturating_sub(end + 1);

    if skipped > pattern.len() {
        return None;
    };

    Some(skipped)
}

fn fuzzy_ascii_subsequence_score(candidate: &[u8], pattern: &[u8]) -> Option<usize> {
    let mut last_match: Option<usize> = None;
    let mut search_from = 0;
    let mut skipped = 0;

    for wanted in pattern {
        let found = candidate
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(idx, ch)| ch.eq_ignore_ascii_case(wanted).then_some(idx))?;

        if let Some(last) = last_match {
            skipped += found.saturating_sub(last + 1);
        }

        last_match = Some(found);
        search_from = found + 1;
    }

    let end = last_match?;
    skipped += candidate.len().saturating_sub(end + 1);

    if skipped > pattern.len() {
        return None;
    }

    Some(skipped)
}

fn acronym_matches(candidate: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }

    let mut pattern = pattern.chars();
    let mut saw_initial = false;

    for initial in candidate.chars().filter(|ch| ch.is_uppercase()) {
        saw_initial = true;
        let Some(wanted) = pattern.next() else {
            return false;
        };
        if !initial.eq_ignore_ascii_case(&wanted) {
            return false;
        }
    }

    saw_initial && pattern.next().is_none()
}

fn prefix_plus_word_initials_matches(candidate: &str, pattern: &str) -> bool {
    let mut candidate_chars = candidate.chars().peekable();
    let mut pattern_chars = pattern.chars().peekable();

    while let (Some(candidate_char), Some(pattern_char)) =
        (candidate_chars.peek(), pattern_chars.peek())
    {
        if !candidate_char.eq_ignore_ascii_case(pattern_char) {
            break;
        }
        candidate_chars.next();
        pattern_chars.next();
    }

    if pattern_chars.peek().is_none() {
        return true;
    }

    let mut saw_initial = false;
    for initial in candidate_chars.filter(|ch| ch.is_uppercase()) {
        saw_initial = true;
        let Some(wanted) = pattern_chars.next() else {
            return true;
        };
        if !initial.eq_ignore_ascii_case(&wanted) {
            return false;
        }
        if pattern_chars.peek().is_none() {
            return true;
        }
    }

    saw_initial && pattern_chars.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol_in_context(context: &str) -> CompletionItem {
        CompletionItem {
            value: "Alpha".to_string(),
            kind: CompletionKind::Symbol,
            frequency: None,
            context: Some(context.to_string()),
        }
    }

    fn suggestion(value: &str, source: CompletionSourceKind) -> Suggestion {
        Suggestion {
            value: value.to_string(),
            description: None,
            style: None,
            extra: Some(vec![
                CompletionSortMetadata {
                    source,
                    frequency: None,
                }
                .serialize(),
            ]),
            span: Span { start: 0, end: 1 },
            append_whitespace: false,
        }
    }

    #[test]
    fn classifies_symbol_contexts_for_completion_precedence() {
        assert_eq!(
            completion_source_kind(&symbol_in_context("Global`")),
            CompletionSourceKind::Global
        );
        assert_eq!(
            completion_source_kind(&symbol_in_context("System`")),
            CompletionSourceKind::System
        );
        assert_eq!(
            completion_source_kind(&symbol_in_context("DataPaclets`")),
            CompletionSourceKind::OtherSingleNameContext
        );
        assert_eq!(
            completion_source_kind(&symbol_in_context("Developer`PackedArrayDump`")),
            CompletionSourceKind::MultiNameContext
        );
    }

    #[test]
    fn sorts_symbols_by_context_precedence_before_match_score() {
        let mut suggestions = vec![
            suggestion(
                "AlphaFromMultiNameContext",
                CompletionSourceKind::MultiNameContext,
            ),
            suggestion(
                "AlphaFromSingleNameContext",
                CompletionSourceKind::OtherSingleNameContext,
            ),
            suggestion("AlphaFromSystem", CompletionSourceKind::System),
            suggestion("AlphaFromGlobal", CompletionSourceKind::Global),
        ];

        suggestions.sort_by(|left, right| {
            completion_sort_key(left, "Alpha")
                .cmp(&completion_sort_key(right, "Alpha"))
                .then_with(|| left.value.cmp(&right.value))
        });

        let values = suggestions
            .into_iter()
            .map(|suggestion| suggestion.value)
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![
                "AlphaFromGlobal",
                "AlphaFromSystem",
                "AlphaFromSingleNameContext",
                "AlphaFromMultiNameContext",
            ]
        );
    }
}
