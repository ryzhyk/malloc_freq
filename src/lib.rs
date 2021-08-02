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
use libc::{c_void, pthread_self};
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
                for (c, b) in counts.iter() {
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

    fn format_totals(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            CallStackStats::Summary(self_calls, self_bytes) => {
                f.write_str(format!("{}calls, {}B", *self_calls, *self_bytes).as_str())
            }
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

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Profile {
    callstacks: Trie<CallStack, AllocCounts>,
    symbols: HashMap<usize, String>,
}

impl Display for Profile {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        let summary = self.summarize();
        self.format_summary(&summary, "", f)
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

    // Use self.symbols to resolve frames in a stack trace into symbols.
    /*fn resolve_callstack(&self, callstack: CallStack) -> Vec<String> {
        callstack
            .iter()
            .map(|frame| self.resolve_symbol(*frame))
            .collect()
    }*/

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

    fn format_summary<'a, T: Clone + TrieCommon<'a, CallStack, CallStackStats>>(
        &self,
        stats: T,
        prefix: &str,
        f: &mut Formatter<'_>,
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

        for (idx, child) in children_sorted.iter().enumerate() {
            if idx == nchildren - 1 {
                self.format_summary(child, format!("{} ", prefix).as_str(), f)?;
            } else {
                self.format_summary(child, format!("{}|", prefix).as_str(), f)?;
            }
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
    pub static PROFILE: RefCell<Profile> = RefCell::new(Profile::new());

    pub static CALL_STACK: RefCell<Vec<usize>> = {
        let callstack = vec![0;MAX_BACKTRACE];
        RefCell::new(callstack)
    };

    // Flag used to detect nested calls to the allocator.
    pub static NESTED: RefCell<bool> = RefCell::new(false);
}

/// Allocator that collects statistics about allocations performed
/// by the program and dumps this statistics to the disk on exit.
/// Use the [`std::alloc::global_allocator`]
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
        // Ignore errors accessing the TLS when the thread is being destroyed.
        let _ = CALL_STACK.try_with(|callstack| {
            let mut callstack = callstack.borrow_mut();
            callstack.set_len(0);
            backtrace::trace(|frame| {
                callstack.push(frame.ip() as usize);
                callstack.len() < MAX_BACKTRACE
            });
            Profile::record_allocation(&callstack, layout.size());
        });
    }
}

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