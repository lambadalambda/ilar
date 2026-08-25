//! Disposable replay acceleration for the append-only session log.
//!
//! Nothing here is authoritative: the JSONL log is. This module holds
//! two caches that let a load skip re-parsing history, plus the
//! integrity machinery that makes trusting them safe.
//!
//! * [`ReplayCheckpoint`] — a sealed snapshot of the active replay
//!   window (events, derived state, byte offset into the log, physical
//!   line count) guarded by a self checksum and a [`FileStamp`] of the
//!   log at seal time. A load replays only the bytes past
//!   `replay_offset`; any mismatch means the cache is stale and the
//!   caller falls back to a full canonical parse.
//! * [`ReplayIdIndex`] — a sorted, paged file of 33-byte id records
//!   (namespace byte + SHA-256 of the id) covering *all* history the
//!   checkpoint elided, over a Merkle tree of page digests. Only the
//!   root lives in the checkpoint, so a page is trusted only after its
//!   digest chains to that root; lookups are a binary search that
//!   verifies each page it touches. This is what lets duplicate-id
//!   detection keep working without holding compacted-away events in
//!   memory.
//!
//! The module has no session semantics beyond reading ids out of
//! events; it never decides what a valid session is.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::event::SessionEvent;
use super::model::ContentBlock;

/// Bumped to 3 when `physical_line_count` stopped being the folded
/// event count: a version-2 file written after a rewind carries a wrong
/// value that still checksums, and the cache is disposable, so the
/// cheapest repair is to refuse to read the old shape at all.
pub(super) const REPLAY_INDEX_VERSION: u32 = 3;
const REPLAY_IDS_MAGIC: &[u8; 8] = b"ILARIDS1";
const REPLAY_IDS_HEADER_LEN: u64 = 32;
const REPLAY_ID_RECORD_LEN: u64 = 33;
const REPLAY_ID_PAGE_RECORDS: usize = 256;

/// One id record: a namespace byte followed by the SHA-256 of the id.
pub(super) type IdRecord = [u8; REPLAY_ID_RECORD_LEN as usize];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FileStamp {
    pub(super) len: u64,
    modified_nanos: u64,
    #[serde(default)]
    device: u64,
    #[serde(default)]
    inode: u64,
    #[serde(default)]
    changed_seconds: i64,
    #[serde(default)]
    changed_nanos: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ReplayCheckpoint {
    pub(super) version: u32,
    pub(super) generation: String,
    pub(super) session_id: String,
    pub(super) replay_offset: u64,
    pub(super) canonical_event_count: usize,
    pub(super) physical_line_count: usize,
    pub(super) active_start: usize,
    pub(super) events: Vec<SessionEvent>,
    pub(super) effective_model: String,
    #[serde(default)]
    pub(super) effective_variant: Option<String>,
    pub(super) todo_list: Option<crate::todo::TodoList>,
    #[serde(default)]
    pub(super) topic: Option<String>,
    pub(super) id_root: String,
    pub(super) observed: FileStamp,
    pub(super) checksum: String,
}

pub(super) fn file_stamp(metadata: &std::fs::Metadata) -> std::io::Result<FileStamp> {
    let modified_nanos = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileStamp {
            len: metadata.len(),
            modified_nanos,
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_seconds: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileStamp {
            len: metadata.len(),
            modified_nanos,
            device: 0,
            inode: 0,
            changed_seconds: 0,
            changed_nanos: 0,
        })
    }
}

pub(super) fn checkpoint_checksum(checkpoint: &ReplayCheckpoint) -> std::io::Result<String> {
    let payload = serde_json::to_vec(&(
        checkpoint.version,
        &checkpoint.generation,
        &checkpoint.session_id,
        checkpoint.replay_offset,
        checkpoint.canonical_event_count,
        checkpoint.physical_line_count,
        checkpoint.active_start,
        &checkpoint.events,
        &checkpoint.effective_model,
        &checkpoint.effective_variant,
        &checkpoint.todo_list,
        &checkpoint.topic,
        &checkpoint.id_root,
        &checkpoint.observed,
    ))
    .map_err(std::io::Error::other)?;
    Ok(digest_hex(&payload))
}

pub(super) fn write_checkpoint(
    path: &std::path::Path,
    checkpoint: &ReplayCheckpoint,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(checkpoint).map_err(std::io::Error::other)?;
    crate::atomic_file::replace(path, &bytes, crate::atomic_file::Mode::Force(0o600))
}

pub(super) fn id_records(events: &[SessionEvent]) -> Vec<IdRecord> {
    let mut records = Vec::new();
    for event in events {
        let event_id = match event {
            SessionEvent::Meta { .. } => None,
            SessionEvent::UserMessage { id, .. }
            | SessionEvent::SubagentInvocation { id, .. }
            | SessionEvent::AssistantMessage { id, .. }
            | SessionEvent::ToolResult { id, .. }
            | SessionEvent::Checkpoint { id, .. }
            | SessionEvent::ModelChange { id, .. }
            | SessionEvent::Compaction { id, .. }
            | SessionEvent::Topic { id, .. }
            | SessionEvent::Rewind { id, .. } => Some(id.as_str()),
        };
        if let Some(id) = event_id {
            records.push(id_record(0, id));
        }
        if let SessionEvent::AssistantMessage { content, .. } = event {
            records.extend(content.iter().filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(id_record(1, id)),
                _ => None,
            }));
        }
    }
    records
}

pub(super) fn id_record(namespace: u8, id: &str) -> IdRecord {
    let mut record = [0; REPLAY_ID_RECORD_LEN as usize];
    record[0] = namespace;
    record[1..].copy_from_slice(&Sha256::digest(id.as_bytes()));
    record
}

pub(super) struct ReplayIdIndex {
    file: File,
    count: usize,
    level_counts: Vec<usize>,
    level_offsets: Vec<u64>,
    root: [u8; 32],
    verified_pages: HashMap<usize, Vec<IdRecord>>,
}

impl ReplayIdIndex {
    pub(super) fn open(
        path: &std::path::Path,
        generation: &str,
        root: &str,
    ) -> std::io::Result<Self> {
        let generation = uuid::Uuid::parse_str(generation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut file = File::open(path)?;
        let mut header = [0u8; REPLAY_IDS_HEADER_LEN as usize];
        file.read_exact(&mut header)?;
        if &header[..8] != REPLAY_IDS_MAGIC || header[8..24] != *generation.as_bytes() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "replay id index generation mismatch",
            ));
        }
        let count = usize::try_from(u64::from_le_bytes(header[24..32].try_into().unwrap()))
            .map_err(|_| invalid_data("replay id count does not fit this platform"))?;
        let level_counts = merkle_level_counts(count);
        let records_len = (count as u64)
            .checked_mul(REPLAY_ID_RECORD_LEN)
            .ok_or_else(|| invalid_data("replay id length overflow"))?;
        let mut offset = REPLAY_IDS_HEADER_LEN
            .checked_add(records_len)
            .ok_or_else(|| invalid_data("replay id length overflow"))?;
        let mut level_offsets = Vec::with_capacity(level_counts.len());
        for level_count in &level_counts {
            level_offsets.push(offset);
            offset = offset
                .checked_add(
                    (*level_count as u64)
                        .checked_mul(32)
                        .ok_or_else(|| invalid_data("replay id tree length overflow"))?,
                )
                .ok_or_else(|| invalid_data("replay id tree length overflow"))?;
        }
        if file.metadata()?.len() != offset {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid replay id index length",
            ));
        }
        Ok(Self {
            file,
            count,
            level_counts,
            level_offsets,
            root: parse_digest(root)?,
            verified_pages: HashMap::new(),
        })
    }

    pub(super) fn contains(&mut self, target: &IdRecord) -> std::io::Result<bool> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            match self.record_at(middle)?.cmp(target) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(true),
            }
        }
        Ok(false)
    }

    fn record_at(&mut self, index: usize) -> std::io::Result<IdRecord> {
        let page = index / REPLAY_ID_PAGE_RECORDS;
        let within = index % REPLAY_ID_PAGE_RECORDS;
        Ok(self.read_page(page)?[within])
    }

    fn read_page(&mut self, page: usize) -> std::io::Result<Vec<IdRecord>> {
        if let Some(records) = self.verified_pages.get(&page) {
            return Ok(records.clone());
        }
        let first_record = page.saturating_mul(REPLAY_ID_PAGE_RECORDS);
        let count = (self.count - first_record).min(REPLAY_ID_PAGE_RECORDS);
        let mut bytes = vec![0; count * REPLAY_ID_RECORD_LEN as usize];
        self.file.seek(std::io::SeekFrom::Start(
            REPLAY_IDS_HEADER_LEN + first_record as u64 * REPLAY_ID_RECORD_LEN,
        ))?;
        self.file.read_exact(&mut bytes)?;
        let mut hash = digest(&bytes);
        let mut node = page;
        for level in 0..self.level_counts.len().saturating_sub(1) {
            let sibling = if node.is_multiple_of(2) {
                node + 1
            } else {
                node - 1
            };
            let sibling_hash = if sibling < self.level_counts[level] {
                self.read_tree_hash(level, sibling)?
            } else {
                hash
            };
            hash = if node.is_multiple_of(2) {
                digest_pair(&hash, &sibling_hash)
            } else {
                digest_pair(&sibling_hash, &hash)
            };
            node /= 2;
        }
        if hash != self.root {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "replay id Merkle proof mismatch",
            ));
        }
        let records = bytes
            .chunks_exact(REPLAY_ID_RECORD_LEN as usize)
            .map(|bytes| bytes.try_into().map_err(std::io::Error::other))
            .collect::<std::io::Result<Vec<_>>>()?;
        self.verified_pages.insert(page, records.clone());
        Ok(records)
    }

    fn read_tree_hash(&mut self, level: usize, node: usize) -> std::io::Result<[u8; 32]> {
        let offset = self.level_offsets[level]
            .checked_add(
                (node as u64)
                    .checked_mul(32)
                    .ok_or_else(|| invalid_data("replay id tree offset overflow"))?,
            )
            .ok_or_else(|| invalid_data("replay id tree offset overflow"))?;
        self.file.seek(std::io::SeekFrom::Start(offset))?;
        let mut hash = [0; 32];
        self.file.read_exact(&mut hash)?;
        Ok(hash)
    }
}

pub(super) fn read_all_id_records(
    path: &std::path::Path,
    generation: &str,
    root: &str,
) -> std::io::Result<Vec<IdRecord>> {
    let mut index = ReplayIdIndex::open(path, generation, root)?;
    let mut records = Vec::with_capacity(index.count);
    for page in 0..index.level_counts.first().copied().unwrap_or(0) {
        records.extend(index.read_page(page)?);
    }
    Ok(records)
}

pub(super) fn write_id_records(
    path: &std::path::Path,
    generation: &str,
    records: &[IdRecord],
) -> std::io::Result<String> {
    let generation = uuid::Uuid::parse_str(generation)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut bytes = Vec::with_capacity(
        REPLAY_IDS_HEADER_LEN as usize + records.len() * REPLAY_ID_RECORD_LEN as usize,
    );
    bytes.extend_from_slice(REPLAY_IDS_MAGIC);
    bytes.extend_from_slice(generation.as_bytes());
    bytes.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for record in records {
        bytes.extend_from_slice(record);
    }
    let mut levels = vec![
        records
            .chunks(REPLAY_ID_PAGE_RECORDS)
            .map(|page| {
                digest(
                    &page
                        .iter()
                        .flat_map(|record| record.iter().copied())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    ];
    while levels.last().is_some_and(|level| level.len() > 1) {
        let previous = levels.last().unwrap();
        levels.push(
            previous
                .chunks(2)
                .map(|pair| digest_pair(&pair[0], pair.get(1).unwrap_or(&pair[0])))
                .collect(),
        );
    }
    for level in &levels {
        for hash in level {
            bytes.extend_from_slice(hash);
        }
    }
    let root = levels
        .last()
        .and_then(|level| level.first())
        .copied()
        .unwrap_or_else(|| digest(&[]));
    crate::atomic_file::replace(path, &bytes, crate::atomic_file::Mode::Force(0o600))?;
    Ok(digest_to_hex(&root))
}

pub(super) fn replay_ids_path(
    replay_index_path: &std::path::Path,
    id: &str,
    generation: &str,
) -> PathBuf {
    replay_index_path.with_file_name(format!("{id}.replay.{generation}.ids"))
}

fn merkle_level_counts(record_count: usize) -> Vec<usize> {
    let mut count = record_count.div_ceil(REPLAY_ID_PAGE_RECORDS);
    let mut levels = Vec::new();
    while count > 0 {
        levels.push(count);
        if count == 1 {
            break;
        }
        count = count.div_ceil(2);
    }
    levels
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn digest_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(left);
    bytes[32..].copy_from_slice(right);
    digest(&bytes)
}

fn digest_to_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_hex(bytes: &[u8]) -> String {
    digest_to_hex(&digest(bytes))
}

fn parse_digest(value: &str) -> std::io::Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(invalid_data("invalid replay digest length"));
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid_data("invalid replay digest"))?;
    }
    Ok(digest)
}

pub(super) fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Lines in a newline-terminated byte range. Callers pass a *committed*
/// region — one that is empty or ends in `\n` — so that the count is
/// also the offset the next line number continues from.
pub(super) fn committed_line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}
