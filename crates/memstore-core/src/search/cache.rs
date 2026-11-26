//! The index cache every index dataset shares (see [`super::session`]): Lance's own (`quick_cache`), with room for
//! what was just read. Lance keeps 97% of it for entries read more than once and takes a new one into the other 3%,
//! which the next reads overwrite before it is read again; a key seen twice goes to the 97% on its next load. With
//! the cache full, as a busy server's is, a keyword search loaded its postings from object storage twice before
//! keeping them (a production facts index: a 4-term search took 16 s, 5 s, then 50 ms), and again for each new file an
//! index update or merge wrote. A keyword search loads a term's postings one part at a time, so a long query paid
//! one storage round trip per term, field and part: 10 to 30 s searches. `AMPUP_MEMORY_INDEX_CACHE_HOT` (percent,
//! default 50) is the share kept for entries read again; the rest holds new ones until their second read.
//!
//! An entry weighs what Lance says it holds plus what it holds that Lance doesn't count ([`unseen_bytes`]).

use std::pin::Pin;

use arrow_array::{RecordBatch, UInt8Array, UInt64Array};
use async_trait::async_trait;
use futures::Future;
use lance::cache::{CacheBackend, CacheCodec, CacheEntry, Context, InternalCacheKey};
use lance::index::vector::ivf::v2::PartitionEntry;
use lance_index::vector::flat::index::FlatIndex;
use lance_index::vector::sq::ScalarQuantizer;

#[derive(Clone)]
struct Entry {
    entry: CacheEntry,
    size_bytes: usize,
}

/// An entry's weight: its declared size and its key, as Lance's own backends count it.
#[derive(Clone)]
struct Weigh;

fn key_bytes() -> usize {
    std::mem::size_of::<InternalCacheKey>()
}

impl quick_cache::Weighter<InternalCacheKey, Entry> for Weigh {
    fn weight(&self, _key: &InternalCacheKey, v: &Entry) -> u64 {
        // Never 0: quick_cache never evicts an entry of no weight.
        key_bytes().saturating_add(v.size_bytes).max(1) as u64
    }
}

/// What a vector index's partition (IVF_SQ, as every vector index here is) holds beyond the size Lance gives it:
/// its quantized rows are kept in chunks, with room made for 1,024 of them (and their offsets) however few rows
/// the partition has, and Lance counts only the chunks. That is about 250 KB a partition, so about 100 MB for an
/// index of 170k rows (412 partitions) and 300 MB for one of 1.4M (1,186), whether its segment holds every row or
/// a delta of a dozen. Uncounted, a delta's partitions weighed next to nothing: the cache kept every delta an
/// update had written since its last eviction, each 100 to 300 MB of memory nobody saw (a local store of 170k
/// passages: 100 MB more per index update, 400 MB after four, with the cache's own size unchanged).
fn unseen_bytes(entry: &CacheEntry) -> usize {
    // The chunk Lance keeps (`SQStorageChunk`, private): the batch, the dimension, the row ids and the codes.
    const CHUNK: usize = std::mem::size_of::<RecordBatch>()
        + std::mem::size_of::<usize>()
        + std::mem::size_of::<UInt64Array>()
        + std::mem::size_of::<UInt8Array>();
    const CHUNKS: usize = 1024;
    if entry
        .downcast_ref::<PartitionEntry<FlatIndex, ScalarQuantizer>>()
        .is_some()
    {
        CHUNKS * CHUNK + (CHUNKS + 1) * std::mem::size_of::<u32>()
    } else {
        0
    }
}

pub(crate) struct IndexCache {
    cache: quick_cache::sync::Cache<InternalCacheKey, Entry, Weigh>,
    capacity: usize,
}

impl std::fmt::Debug for IndexCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexCache")
            .field("entries", &self.cache.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl IndexCache {
    /// Up to `capacity` bytes, `hot` of it (0 to 1) kept for entries read more than once; one shard, as Lance
    /// sizes its index cache.
    pub(crate) fn new(capacity: usize, hot: f64) -> Self {
        let options = quick_cache::OptionsBuilder::new()
            .estimated_items_capacity((capacity / (64 << 10)).clamp(32, 1_000_000))
            .weight_capacity(capacity as u64)
            .shards(1)
            .hot_allocation(hot.clamp(0.0, 1.0))
            .build()
            .expect("the cache's capacities are set");
        IndexCache {
            cache: quick_cache::sync::Cache::with_options(
                options,
                Weigh,
                Default::default(),
                Default::default(),
            ),
            capacity,
        }
    }
}

#[async_trait]
impl CacheBackend for IndexCache {
    async fn get(&self, key: &InternalCacheKey, _codec: Option<CacheCodec>) -> Option<CacheEntry> {
        self.cache.get(key).map(|v| v.entry)
    }

    async fn insert(
        &self,
        key: &InternalCacheKey,
        entry: CacheEntry,
        size_bytes: usize,
        _codec: Option<CacheCodec>,
    ) {
        let size_bytes = size_bytes + unseen_bytes(&entry);
        self.cache.insert(*key, Entry { entry, size_bytes });
    }

    async fn get_or_insert<'a>(
        &self,
        key: &InternalCacheKey,
        loader: Pin<Box<dyn Future<Output = lance::Result<(CacheEntry, usize)>> + Send + 'a>>,
        _codec: Option<CacheCodec>,
    ) -> lance::Result<(CacheEntry, bool)> {
        match self.cache.get_value_or_guard_async(key).await {
            Ok(v) => Ok((v.entry, true)),
            Err(guard) => {
                let (entry, size_bytes) = loader.await?;
                let _ = guard.insert(Entry {
                    size_bytes: size_bytes + unseen_bytes(&entry),
                    entry: entry.clone(),
                });
                Ok((entry, false))
            }
        }
    }

    async fn clear(&self) {
        self.cache.clear();
    }

    async fn num_entries(&self) -> usize {
        self.cache.len()
    }

    async fn size_bytes(&self) -> usize {
        self.cache.weight() as usize
    }

    fn approx_num_entries(&self) -> usize {
        self.cache.len()
    }

    fn approx_size_bytes(&self) -> usize {
        self.cache.weight() as usize
    }

    fn capacity_bytes(&self) -> Option<usize> {
        Some(self.capacity)
    }

    fn deep_size_of_entries(
        &self,
        context: &mut Context,
        size_of_entry: &dyn Fn(&CacheEntry, &mut Context) -> Option<usize>,
    ) -> Option<usize> {
        Some(
            self.cache
                .iter()
                .map(|(_, v)| {
                    key_bytes()
                        + size_of_entry(&v.entry, context)
                            .map_or(v.size_bytes, |n| n + unseen_bytes(&v.entry))
                })
                .sum(),
        )
    }
}
