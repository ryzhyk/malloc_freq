//! This malloc frequency profiler helps detect program hotspots that perform a large number of
//! memory allocations.  Many small allocations can slow down your application
//! and lead to memory fragmentation.  Typical symptoms include seeing `malloc` at the top of the
//! application's CPU profile, as well as observing that the resident set size (RSS) of the
//! appliciation significantly exceeds the amount of memory it allocates from the heap.
//!
//! Note: `malloc_freq` is different from heap profilers like `valgrind`'s `massif`, which track
//! program's heap usage at any give point in time, in two ways.  First, heap profilers ignore
//! short-lived allocations that do not affect the heap size significantly.  Second, heap profilers
//! track the amount of allocated memory, whereas `malloc_freq` focuses on the number of calls
//! to `malloc`.
//!
//! `malloc_freq` can be used to profile programs in Rust or any other compiled language.
//!
//! ## Enabling `malloc_freq` in a Rust program
//!
//! To enable `malloc_freq` in a Rust program, configure it as a global allocator instead of
//! [`std::alloc::System`]:
//!
//! ```
//! use malloc_freq::ProfAllocator;
//!
//! #[global_allocator]
//! static GLOBAL: ProfAllocator = ProfAllocator;
//!
//! fn main() {}
//! ```
//!
//! ## Using `malloc_freq` via `LD_PRELOAD`
//!
//! For programs in other languages, including a mix of Rust, C, etc., use the companion
//! [`lib_malloc_freq`] crate, which produces a dynamic library that can be used via `LD_PRELOAD`
//! to intercept all `malloc` calls issued by the program:
//!
//! ```bash
//! LD_PRELOAD=libmalloc_freq.so ./my_program
//! ```
//!
//! ## Viewing `malloc_freq` profiles
//!
//! Upon **successful** program termination, `malloc_freq` stores the malloc profile in
//! the `malloc_freq.<pid>` directory.  The directory will contain a file for each thread in the
//! program.  To view the profile, use the `mf_print` tool from this crate, e.g.:
//!
//! ```bash
//! mf_print --dir malloc_freq.<pid> --threshold 0.2
//! ```
//!
//! where the `--threshold` switch specifies the significance threshold of profile entries in
//! percents (profile entries with fewer than `threshold`% `malloc` calls are skipped in the output).
//! `mf_print` outputs a human-readable profile that lists program callstacks that perform `malloc`
//! calls, annotated with the number of `malloc` calls and the total number of bytes allocated by each
//! stack trace.

#![allow(clippy::ptr_arg)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    array::IntoIter,
    cell::RefCell,
    collections::{hash_map::Entry, HashMap},
    fmt,
    fmt::{Display, Formatter},
    fs,
    path::Path,
    process, thread_local,
};

use backtrace::SymbolName;
use libc::{c_char, c_void, dlsym, pthread_self, RTLD_NEXT};
use num_format::{Locale, ToFormattedString};
use once_cell::sync::Lazy;
use radix_trie::{iter::Children, SubTrie, Trie, TrieCommon, TrieKey};
use serde::{Deserialize, Serialize};

const MAX_BACKTRACE: usize = 128;

// Call stack returned by `backtrace::trace()`.
type CallStack = Vec<usize>;

// For each call stack, track (allocation size -> number of allocations map)
type AllocCounts = HashMap<usize, usize>;

#[derive(Clone, Debug)]
enum CallStackStats {
    Detailed(AllocCounts),
    // Aggregate number of invocations/total bytes allocated.
    Summary(usize, usize),
}

impl CallStackStats {
    fn new() -> Self {
        CallStackStats::Summary(0, 0)
    }

    fn summarize(&self) -> CallStackStats {
        match self {
            CallStackStats::Summary(..) => self.clone(),
            CallStackStats::Detailed(counts) => {
                let mut calls = 0;
                let mut bytes = 0;
                for (b, c) in counts.iter() {
                    calls += c;
                    bytes += c * b;
                }
                CallStackStats::Summary(calls, bytes)
            }
        }
    }

    fn merge(&mut self, other: &CallStackStats) {
        if let CallStackStats::Summary(self_calls, self_bytes) = self.summarize() {
            if let CallStackStats::Summary(other_calls, other_bytes) = other.summarize() {
                *self = CallStackStats::Summary(self_calls + other_calls, self_bytes + other_bytes);
            }
        }
    }

    fn format_totals<W: fmt::Write>(&self, f: &mut W) -> Result<(), fmt::Error> {
        match self {
            CallStackStats::Summary(self_calls, self_bytes) => f.write_str(
                format!(
                    "{} calls, {}B",
                    self_calls.to_formatted_string(&Locale::en),
                    self_bytes.to_formatted_string(&Locale::en)
                )
                .as_str(),
            ),
            CallStackStats::Detailed(_) => self.summarize().format_totals(f),
        }
    }

    fn num_calls(&self) -> usize {
        match self {
            CallStackStats::Summary(calls, _) => *calls,
            CallStackStats::Detailed(_) => self.summarize().num_calls(),
        }
    }
}

/// Malloc frequency profile generated by `malloc_freq`.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Profile {
    callstacks: Trie<CallStack, AllocCounts>,
    symbols: HashMap<usize, String>,
}

impl Display for Profile {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        self.fmt_with_threshold(0.0, f)
    }
}

// Invoked when a thread terminates.  Dump thread's memory profile to file.
impl Drop for Profile {
    fn drop(&mut self) {
        if !self.symbols.is_empty() {
            return;
        }
        for callstack in self.callstacks.keys() {
            for frame in callstack {
                if let Entry::Vacant(ve) = self.symbols.entry(*frame) {
                    ve.insert(unsafe { Self::resolve(*frame) });
                }
            }
        }

        let profile_string = serde_yaml::to_string(self)
            .unwrap_or_else(|e| format!("failed to convert malloc profile to YAML: {}", e));
        let profile_dir = format!("malloc_freq.{}", process::id());
        let profile_file = format!("{}/malloc_freq.{}", profile_dir, unsafe { pthread_self() });
        let _ = fs::create_dir(profile_dir);
        fs::write(profile_file, profile_string)
            .unwrap_or_else(|e| eprintln!("failed to write malloc profile: {}", e));
    }
}

impl Profile {
    pub fn new() -> Self {
        Self::default()
    }

    // Record a memory allocation of size `size` in the profile.
    unsafe fn record_allocation(callstack: &Vec<usize>, size: usize) {
        // Ignore errors accessing the TLS when the thread is being destroyed.
        let _ = PROFILE.try_with(|profile| {
            let mut profile = profile.borrow_mut();
            match profile.callstacks.get_mut(callstack) {
                Some(counts) => match counts.entry(size) {
                    Entry::Occupied(mut oe) => {
                        *oe.get_mut() += 1;
                    }
                    Entry::Vacant(ve) => {
                        ve.insert(1);
                    }
                },
                None => {
                    profile
                        .callstacks
                        .insert(callstack.clone(), IntoIter::new([(size, 1)]).collect());
                }
            }
        });
    }

    // Convert frame address into string.
    // Must be called while running in the context of the target program.
    unsafe fn resolve(frame: usize) -> String {
        let mut sym = frame.to_string();
        backtrace::resolve(frame as *mut c_void, |s| {
            sym = format!(
                "{} (in {},{}:{})",
                s.name().unwrap_or_else(|| SymbolName::new(&[])),
                s.filename().unwrap_or_else(|| Path::new("")).display(),
                s.lineno().unwrap_or(0),
                s.colno().unwrap_or(0)
            )
        });
        sym
    }

    // Use self.symbols to resolve a single frame in a stack trace.
    fn resolve_symbol(&self, frame: usize) -> String {
        self.symbols
            .get(&frame)
            .cloned()
            .unwrap_or_else(|| frame.to_string())
    }

    // Merge two profiles.
    pub fn merge(&mut self, other: &Profile) {
        for (callstack, counts) in other.callstacks.iter() {
            match self.callstacks.get_mut(callstack) {
                None => {
                    self.callstacks.insert(callstack.clone(), counts.clone());
                }
                Some(old_counts) => {
                    Self::merge_counts(old_counts, counts);
                }
            }
        }

        for (frame, symbol) in other.symbols.iter() {
            if let Entry::Vacant(ve) = self.symbols.entry(*frame) {
                ve.insert(symbol.clone());
            }
        }
    }

    fn merge_counts(this: &mut AllocCounts, other: &AllocCounts) {
        for (size, cnt) in other.iter() {
            match this.entry(*size) {
                Entry::Occupied(mut oe) => {
                    *oe.get_mut() += *cnt;
                }
                Entry::Vacant(ve) => {
                    ve.insert(*cnt);
                }
            }
        }
    }

    fn summarize(&self) -> Trie<CallStack, CallStackStats> {
        let mut all_stacks = Trie::new();
        for (callstack, stats) in self.callstacks.iter() {
            all_stacks.insert(callstack.clone(), CallStackStats::Detailed(stats.clone()));
            for range in 1..(callstack.len() - 1) {
                let prefix = Vec::from(&callstack.as_slice()[0..range]);
                if all_stacks.get(&prefix).is_none() {
                    all_stacks.insert(prefix, CallStackStats::new());
                }
            }
        }
        let mut summary = Trie::new();
        Self::aggregate_stats(&mut summary, &all_stacks);
        summary
    }

    // Scan the trie, store aggregate allocation counts in each node
    fn aggregate_stats<'a, T: Clone + TrieCommon<'a, CallStack, CallStackStats>>(
        trie: &mut Trie<CallStack, CallStackStats>,
        node: T,
    ) -> CallStackStats {
        let mut stats = node
            .clone()
            .value()
            .cloned()
            .unwrap_or_else(CallStackStats::new);
        for child in node.clone().children() {
            let child_stats = Self::aggregate_stats(trie, &child);
            stats.merge(&child_stats);
        }
        if node.clone().value().is_some() {
            trie.insert(node.key().unwrap().clone(), stats.clone());
        }
        stats
    }

    pub fn fmt_with_threshold<W: fmt::Write>(
        &self,
        threshold: f64,
        f: &mut W,
    ) -> Result<(), fmt::Error> {
        let summary = self.summarize();
        let total_calls = trie_children_with_keys(&summary)
            .next()
            .unwrap()
            .value()
            .unwrap()
            .num_calls();
        self.format_summary(&summary, total_calls, threshold, "", f)
    }

    fn format_summary<'a, T: Clone + TrieCommon<'a, CallStack, CallStackStats>, W: fmt::Write>(
        &self,
        stats: T,
        total_calls: usize,
        threshold: f64,
        prefix: &str,
        f: &mut W,
    ) -> Result<(), fmt::Error> {
        let has_key = stats.clone().key().is_some();
        if has_key {
            f.write_str("\n")?;
            f.write_str(prefix)?;
            f.write_str("->")?;
            stats.clone().value().unwrap().format_totals(f)?;
            f.write_str(": ")?;
            let sym = self.resolve_symbol(*stats.clone().key().unwrap().last().unwrap());
            f.write_str(sym.as_str())?;
            //if let CallStackStats::Detailed(counts) = self.value().unwrap() { };
        }

        let nchildren = trie_children_with_keys(stats.clone()).count();
        let mut children_sorted: Vec<_> = trie_children_with_keys(stats).collect();
        children_sorted.as_mut_slice().sort_by(|c1, c2| {
            c2.value()
                .unwrap()
                .num_calls()
                .cmp(&c1.value().unwrap().num_calls())
        });

        let mut below_threshold = 0;
        for (idx, child) in children_sorted.iter().enumerate() {
            if 100.0 * (child.value().unwrap().num_calls() as f64) / (total_calls as f64)
                < threshold
            {
                below_threshold += child.value().unwrap().num_calls();
                continue;
            }
            if idx == nchildren - 1 {
                self.format_summary(
                    child,
                    total_calls,
                    threshold,
                    format!("{}  ", prefix).as_str(),
                    f,
                )?;
            } else {
                self.format_summary(
                    child,
                    total_calls,
                    threshold,
                    format!("{} |", prefix).as_str(),
                    f,
                )?;
            }
        }

        if below_threshold > 0 {
            f.write_str(
                format!(
                    "\n{}  ->{} calls in places below mf_print threshold ({}%)",
                    prefix, below_threshold, threshold
                )
                .as_str(),
            )?;
        }

        Ok(())
    }
}

// Iterate through the nearest descendants that have keys.
struct ChildrenWithKey<'a, K, V> {
    stack: Vec<Children<'a, K, V>>,
}

impl<'a, K, V> ChildrenWithKey<'a, K, V> {
    fn new<T>(trie: T) -> Self
    where
        T: TrieCommon<'a, K, V>,
        K: TrieKey,
    {
        ChildrenWithKey {
            stack: vec![trie.children()],
        }
    }
}

impl<'a, K, V> Iterator for ChildrenWithKey<'a, K, V>
where
    K: TrieKey,
{
    type Item = SubTrie<'a, K, V>;

    fn next(&mut self) -> Option<SubTrie<'a, K, V>> {
        if self.stack.is_empty() {
            return None;
        }

        match self.stack.last_mut().unwrap().next() {
            None => {
                self.stack.pop();
                self.next()
            }
            Some(child) => match child.key() {
                Some(_) => Some(child),
                None => {
                    self.stack.push(child.children());
                    self.next()
                }
            },
        }
    }
}

fn trie_children_with_keys<'a, K, V, T>(trie: T) -> ChildrenWithKey<'a, K, V>
where
    T: Clone + TrieCommon<'a, K, V>,
    K: TrieKey,
{
    ChildrenWithKey::new(trie)
}

thread_local! {
    // Per-thread memory profile.
    pub(crate) static PROFILE: RefCell<Profile> = RefCell::new(Profile::new());

    pub(crate) static CALL_STACK: RefCell<Vec<usize>> = {
        let callstack = vec![0;MAX_BACKTRACE];
        RefCell::new(callstack)
    };

    // Flag used to detect nested calls to the allocator.
    pub(crate) static NESTED: RefCell<bool> = RefCell::new(false);
}

/// Allocator that collects statistics about allocations performed
/// by the program and dumps this statistics to the disk on exit.
/// Use the `global_allocator`
/// attribute to enable this allocator in your program:
///
/// ```
/// use malloc_freq::ProfAllocator;
///
/// #[global_allocator]
/// static GLOBAL: ProfAllocator = ProfAllocator;
///
/// fn main() {}
/// ```
///
pub struct ProfAllocator;

impl ProfAllocator {
    unsafe fn alloc_record(layout: Layout) {
        Self::malloc_record(layout.size() as libc::size_t);
    }

    unsafe fn malloc_record(size: libc::size_t) {
        // Ignore errors accessing the TLS when the thread is being destroyed.
        let _ = CALL_STACK.try_with(|callstack| {
            let mut callstack = callstack.borrow_mut();
            callstack.set_len(0);
            backtrace::trace(|frame| {
                callstack.push(frame.ip() as usize);
                callstack.len() < MAX_BACKTRACE
            });
            Profile::record_allocation(&callstack, size);
        });
    }

    /// A replacement `malloc()` implementation that records each `malloc` call in the profile.
    /// When loaded via `LD_PRELOAD`, [`lib_malloc_freq`] redirects `malloc` calls to this method.
    pub unsafe fn malloc(size: libc::size_t) -> *mut c_void {
        let real_malloc: MallocFunc = std::mem::transmute(*REAL_MALLOC);
        let res = NESTED.try_with(|nested| {
            if *nested.borrow() {
                real_malloc(size)
            } else {
                *nested.borrow_mut() = true;
                Self::malloc_record(size);
                let res = real_malloc(size);
                *nested.borrow_mut() = false;
                res
            }
        });
        match res {
            Ok(res) => res,
            // alloc called during thread destruction.
            Err(..) => real_malloc(size),
        }
    }
}

static REAL_MALLOC: Lazy<usize> = Lazy::new(|| {
    let real_malloc = unsafe { dlsym(RTLD_NEXT, b"malloc\0".as_ptr() as *const c_char) };
    if real_malloc.is_null() {
        panic!("malloc_freq: couldn't find original malloc");
    };
    real_malloc as usize
});

type MallocFunc = unsafe extern "C" fn(size: libc::size_t) -> *mut c_void;

unsafe impl GlobalAlloc for ProfAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let res = NESTED.try_with(|nested| {
            if *nested.borrow() {
                System.alloc(layout)
            } else {
                *nested.borrow_mut() = true;
                //eprintln!("alloc {}", layout.size());
                Self::alloc_record(layout);
                let res = System.alloc(layout);
                *nested.borrow_mut() = false;
                res
            }
        });
        match res {
            Ok(res) => res,
            // alloc called during thread destruction.
            Err(..) => System.alloc(layout),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[cfg(test)]
mod tests {
    use crate::ProfAllocator;
    use std::thread::spawn;

    #[global_allocator]
    static GLOBAL: ProfAllocator = ProfAllocator;

    #[test]
    fn alloc_vectors() {
        let mut thread_handles = vec![];
        for _ in 0..10 {
            let thread = spawn(|| {
                let _v = vec![1, 2, 3];
                let _b = Box::new(true);
            });
            thread_handles.push(thread);
        }
        for t in thread_handles.drain(..) {
            t.join().unwrap();
        }
    }
}
