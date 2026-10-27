//! Storage for the tree that grows in fixed-size chunks instead of
//! reallocating. A growing `Vec` copies itself on every doubling, which for a
//! million-key tree means multi-megabyte copies while the scan holds the tree
//! lock, and on macOS the freed copies stay resident in the allocator's
//! cache (they made up about half of the tree's RSS). Chunks are allocated
//! once and never move. Their untouched tail is not resident, so the first
//! chunk of a small tree costs address space, not memory.

use std::ops::{Index, IndexMut};

const CHUNK_BITS: u32 = 14;
const CHUNK: usize = 1 << CHUNK_BITS;

/// A `Vec<T>` addressed by `u32` that grows in chunks of 16,384 elements.
pub(super) struct Chunked<T> {
    chunks: Vec<Vec<T>>,
}

impl<T> Default for Chunked<T> {
    fn default() -> Self {
        Self { chunks: Vec::new() }
    }
}

impl<T> Chunked<T> {
    pub(super) fn len(&self) -> usize {
        self.chunks
            .last()
            .map_or(0, |last| (self.chunks.len() - 1) * CHUNK + last.len())
    }

    /// Appends `value` and returns its index. Callers keep the length within
    /// `u32` (the tree caps its node count far below that).
    pub(super) fn push(&mut self, value: T) -> u32 {
        if self.chunks.last().is_none_or(|last| last.len() == CHUNK) {
            self.chunks.push(Vec::with_capacity(CHUNK));
        }
        let index = self.len() as u32;
        if let Some(last) = self.chunks.last_mut() {
            last.push(value);
        }
        index
    }

    pub(super) fn get(&self, index: u32) -> Option<&T> {
        self.chunks
            .get((index >> CHUNK_BITS) as usize)?
            .get(index as usize & (CHUNK - 1))
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks.iter().flatten()
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.chunks.iter_mut().flatten()
    }

    pub(super) fn heap_bytes(&self) -> usize {
        self.chunks.capacity() * size_of::<Vec<T>>()
            + self
                .chunks
                .iter()
                .map(|chunk| chunk.capacity() * size_of::<T>())
                .sum::<usize>()
    }
}

impl<T> Index<u32> for Chunked<T> {
    type Output = T;

    fn index(&self, index: u32) -> &T {
        &self.chunks[(index >> CHUNK_BITS) as usize][index as usize & (CHUNK - 1)]
    }
}

impl<T> IndexMut<u32> for Chunked<T> {
    fn index_mut(&mut self, index: u32) -> &mut T {
        &mut self.chunks[(index >> CHUNK_BITS) as usize][index as usize & (CHUNK - 1)]
    }
}

const NAME_BITS: u32 = 18;
/// Bytes per name chunk (256 KiB). A longer name gets a chunk of its own.
pub(super) const NAME_CHUNK: usize = 1 << NAME_BITS;
const MAX_NAME_CHUNKS: usize = 1 << (32 - NAME_BITS);

/// Segment names, appended in node order. A name is addressed by a `u32`
/// start (chunk index in the high bits, offset in the low ones) and never
/// spans chunks. Its end is not stored: it is where the next name starts if
/// that one is in the same chunk, else the end of the chunk, since a chunk
/// is never appended to again once a later one exists.
pub(super) struct Names {
    chunks: Vec<Vec<u8>>,
}

impl Names {
    /// Starts with one empty chunk, so start `0` is the empty name.
    pub(super) fn new() -> Self {
        Self {
            chunks: vec![Vec::with_capacity(NAME_CHUNK)],
        }
    }

    /// Appends `name` and returns where it starts, or `None` once the 4 GiB
    /// of addressable name space are used up.
    pub(super) fn push(&mut self, name: &[u8]) -> Option<u32> {
        // The offset must stay below NAME_CHUNK to fit its bits, even for an
        // empty name at the very end of a full chunk.
        let fits = self.chunks.last().is_some_and(|chunk| {
            chunk.len() < NAME_CHUNK && chunk.len() + name.len() <= NAME_CHUNK
        });
        if !fits {
            if self.chunks.len() == MAX_NAME_CHUNKS {
                return None;
            }
            self.chunks
                .push(Vec::with_capacity(name.len().max(NAME_CHUNK)));
        }
        let index = self.chunks.len() - 1;
        let chunk = &mut self.chunks[index];
        let start = ((index << NAME_BITS) | chunk.len()) as u32;
        chunk.extend_from_slice(name);
        Some(start)
    }

    /// The name starting at `start`, given where the next name starts (if
    /// there is a next one).
    pub(super) fn get(&self, start: u32, next: Option<u32>) -> &[u8] {
        let chunk = &self.chunks[(start >> NAME_BITS) as usize];
        let from = (start as usize) & (NAME_CHUNK - 1);
        let end = match next {
            Some(next) if next >> NAME_BITS == start >> NAME_BITS => {
                (next as usize) & (NAME_CHUNK - 1)
            }
            _ => chunk.len(),
        };
        &chunk[from..end]
    }

    /// The `len` bytes starting at `start`, for a caller that has looked up
    /// a name's length before.
    pub(super) fn slice(&self, start: u32, len: u32) -> &[u8] {
        let chunk = &self.chunks[(start >> NAME_BITS) as usize];
        let from = (start as usize) & (NAME_CHUNK - 1);
        &chunk[from..from + len as usize]
    }

    pub(super) fn heap_bytes(&self) -> usize {
        self.chunks.capacity() * size_of::<Vec<u8>>()
            + self.chunks.iter().map(Vec::capacity).sum::<usize>()
    }
}
