//! Import of blocks from a file into an empty node, through the same
//! validation and execution path follow and batch sync use:
//!
//! 1. header validation with the node's [`HeaderVerifier`] against the parent
//!    ([`SyncManager::handle_headers_response`](crate::SyncManager));
//! 2. header, total difficulty and canonical chain stored with
//!    [`BlockStore::store_headers_batch`], body with [`BlockStore::put_body`];
//! 3. [`BlockProcessor::process_and_commit`]: transactions root, ommers hash,
//!    execution, then gas used, paid fees, state root, receipts root and logs
//!    bloom against the header, and the receipts stored;
//! 4. the trie store flushed and the executed head advanced every
//!    `flush_every` blocks, as batch sync does every 100.
//!
//! Unlike sync, a block that is not the child of the current best block, or
//! that fails any check, stops the import: the file is meant to be one chain.

use alloy_primitives::{B256, U256};
use alloy_rlp::Decodable;
use rustock_core::validation::{HeaderVerifier, ValidationError};
use rustock_core::{Block, Header, Transaction};
use rustock_execution::{BlockProcessor, ProcessError, ProcessTimings};
use rustock_storage::BlockStore;
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Why a block was not imported.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("not the next block: parent {parent}, number {number}, but the best block is #{best_number} {best_hash}")]
    NotNextBlock {
        number: u64,
        parent: B256,
        best_number: u64,
        best_hash: B256,
    },
    #[error("header validation failed: {0}")]
    Header(#[from] ValidationError),
    #[error("{0}")]
    Process(#[from] ProcessError),
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

impl ImportError {
    /// Short name of the check that failed (for logs and exit messages).
    pub fn check(&self) -> &'static str {
        match self {
            ImportError::NotNextBlock { .. } => "parent",
            ImportError::Header(_) => "header",
            ImportError::Process(e) => match e {
                ProcessError::SenderRecovery { .. } => "senderRecovery",
                ProcessError::Execution(_) => "execution",
                ProcessError::GasUsedMismatch { .. } => "gasUsed",
                ProcessError::PaidFeesMismatch { .. } => "paidFees",
                ProcessError::StateRootMismatch { .. } => "stateRoot",
                ProcessError::OrchidStateRootMismatch { .. } => "orchidStateRoot",
                ProcessError::ReceiptsRootMismatch { .. } => "receiptsRoot",
                ProcessError::TransactionsRootMismatch { .. } => "transactionsRoot",
                ProcessError::OmmersHashMismatch { .. } => "ommersHash",
                ProcessError::LogsBloomMismatch => "logsBloom",
                ProcessError::Storage(_) => "storage",
            },
            ImportError::Storage(_) => "storage",
        }
    }
}

/// A block that was imported as the new best block.
#[derive(Debug, Clone)]
pub struct ImportedBlock {
    pub number: u64,
    pub hash: B256,
    pub tx_count: usize,
    pub gas_used: u64,
    /// Whether this block's import ended with a flush of the trie store.
    pub flushed: bool,
    /// Where the time went, when the importer measures it
    /// ([`BlockImporter::measure`]).
    pub timings: Option<ImportTimings>,
}

/// Wall time of each step of [`BlockImporter::import`], from a monotonic
/// clock. `total` covers the whole call; the steps run in this order.
#[derive(Debug, Default, Clone)]
pub struct ImportTimings {
    pub total: Duration,
    /// Checking that the block extends the best block.
    pub preamble: Duration,
    /// `HeaderVerifier::verify` against the parent.
    pub header_validation: Duration,
    /// Header, total difficulty, canonical index and body stored.
    pub store_block: Duration,
    /// `BlockProcessor::process_and_commit`, by stage.
    pub process: ProcessTimings,
    /// Trie store flushed to disk (zero on blocks without a flush).
    pub flush: Duration,
    /// Executed head recorded (with the flush).
    pub exec_head: Duration,
}

/// Measures consecutive steps; does not read the clock when disabled.
struct Laps {
    start: Option<Instant>,
    last: Option<Instant>,
}

impl Laps {
    fn new(enabled: bool) -> Self {
        let now = enabled.then(Instant::now);
        Self {
            start: now,
            last: now,
        }
    }

    fn lap(&mut self) -> Duration {
        match self.last {
            Some(last) => {
                let now = Instant::now();
                self.last = Some(now);
                now - last
            }
            None => Duration::ZERO,
        }
    }

    fn total(&self) -> Duration {
        self.start.map(|s| s.elapsed()).unwrap_or_default()
    }
}

/// Imports a chain block by block on top of the store's best block.
pub struct BlockImporter {
    store: Arc<BlockStore>,
    verifier: HeaderVerifier,
    processor: BlockProcessor,
    trie_store: Arc<dyn TrieStore>,
    state_root: TrieNode,
    best: Header,
    best_hash: B256,
    best_td: U256,
    flush_every: u64,
    imported: u64,
    last_executed: Option<(B256, B256)>,
    measure: bool,
}

impl BlockImporter {
    /// An importer continuing from the store's head, whose state is
    /// `state_root`. `flush_every` must be at least 1.
    pub fn new(
        store: Arc<BlockStore>,
        verifier: HeaderVerifier,
        processor: BlockProcessor,
        trie_store: Arc<dyn TrieStore>,
        state_root: TrieNode,
        flush_every: u64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(flush_every >= 1, "flush_every must be at least 1");
        let best_hash = store
            .head()?
            .ok_or_else(|| anyhow::anyhow!("the store has no head block"))?;
        let best = store
            .header(best_hash)?
            .ok_or_else(|| anyhow::anyhow!("head header {best_hash} missing"))?;
        let best_td = store.total_difficulty(best_hash)?.unwrap_or_default();
        Ok(Self {
            store,
            verifier,
            processor,
            trie_store,
            state_root,
            best,
            best_hash,
            best_td,
            flush_every,
            imported: 0,
            last_executed: None,
            measure: false,
        })
    }

    /// Measure each import ([`ImportedBlock::timings`]). Off by default; it
    /// only reads a monotonic clock between steps.
    pub fn measure(mut self, measure: bool) -> Self {
        self.measure = measure;
        self
    }

    /// Hash and number of the best block.
    pub fn best(&self) -> (u64, B256) {
        (self.best.number, self.best_hash)
    }

    /// Validate, execute and store `block` as the new best block.
    pub fn import(&mut self, block: Block) -> Result<ImportedBlock, ImportError> {
        let mut laps = Laps::new(self.measure);
        let mut t = ImportTimings::default();
        let header = &block.header;
        let hash = header.hash();
        if header.parent_hash != self.best_hash || header.number != self.best.number + 1 {
            return Err(ImportError::NotNextBlock {
                number: header.number,
                parent: header.parent_hash,
                best_number: self.best.number,
                best_hash: self.best_hash,
            });
        }
        t.preamble = laps.lap();

        self.verifier.verify(header, Some(&self.best))?;
        t.header_validation = laps.lap();

        let td = self.best_td + header.difficulty;
        self.store
            .store_headers_batch(&[(header, td)], Some(self.best_hash), self.best_td)?;
        self.store
            .put_body(hash, &block.transactions, &block.ommers)?;
        t.store_block = laps.lap();

        let state_root = &self.state_root;
        let trie_store = self.trie_store.clone();
        let result = if self.measure {
            self.processor.process_and_commit_timed(
                &block,
                state_root,
                trie_store,
                &mut t.process,
            )?
        } else {
            self.processor
                .process_and_commit(&block, state_root, trie_store)?
        };
        laps.lap();
        self.state_root = result.new_state_root;
        self.last_executed = Some((hash, result.state_root_hash));

        self.imported += 1;
        let flushed = self.imported.is_multiple_of(self.flush_every);
        if flushed {
            self.trie_store.flush();
            t.flush = laps.lap();
            self.record_exec_head()?;
            t.exec_head = laps.lap();
        }
        t.total = laps.total();

        let imported = ImportedBlock {
            number: header.number,
            hash,
            tx_count: block.transactions.len(),
            gas_used: header.gas_used,
            flushed,
            timings: self.measure.then_some(t),
        };
        self.best_hash = hash;
        self.best_td = td;
        self.best = block.header;
        Ok(imported)
    }

    /// Record the executed head (batch sync does it at each flush).
    fn record_exec_head(&mut self) -> anyhow::Result<()> {
        if let Some((hash, root)) = self.last_executed.take() {
            self.store.set_exec_head(hash, root)?;
        }
        Ok(())
    }

    /// Flush whatever the last blocks left in memory.
    pub fn finish(mut self) -> anyhow::Result<()> {
        self.trie_store.flush();
        self.record_exec_head()
    }
}

/// One line of rskj's ExportBlocks format:
/// `number,hashHex,totalDifficultyHex,blockRlpHex`.
#[derive(Debug, Clone)]
pub struct ExportedBlock {
    pub number: u64,
    pub hash: B256,
    pub block: Block,
}

/// Decode a block in rskj's encoding: `RLP([header, transactions, uncles])`,
/// with the header hash taken as rskj computes it.
pub fn decode_block(mut rlp: &[u8]) -> anyhow::Result<Block> {
    let buf = &mut rlp;
    let list = alloy_rlp::Header::decode(buf)?;
    anyhow::ensure!(list.list, "block is not an RLP list");
    let mut body = &buf[..list.payload_length];
    let header = Header::decode_with_hash(&mut body)?;
    let transactions = Vec::<Transaction>::decode(&mut body)?;
    let uncles_list = alloy_rlp::Header::decode(&mut body)?;
    anyhow::ensure!(uncles_list.list, "uncles are not an RLP list");
    let (mut uncles_rlp, rest) = body.split_at(uncles_list.payload_length);
    let mut ommers = Vec::new();
    while !uncles_rlp.is_empty() {
        ommers.push(Header::decode_with_hash(&mut uncles_rlp)?);
    }
    anyhow::ensure!(rest.is_empty(), "unexpected fields after the uncles");
    Ok(Block {
        header,
        transactions,
        ommers,
    })
}

/// Parse one ExportBlocks line. `Ok(None)` for lines rskj's ConnectBlocks
/// skips (fewer than four fields). The decoded block must have the number and
/// hash the line states.
pub fn parse_export_line(line: &str) -> anyhow::Result<Option<ExportedBlock>> {
    let parts: Vec<&str> = line.trim().split(',').collect();
    if parts.len() < 4 {
        return Ok(None);
    }
    let number: u64 = parts[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("block number '{}': {e}", parts[0]))?;
    let hash: B256 = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("block hash '{}': {e}", parts[1]))?;
    let rlp = alloy_primitives::hex::decode(parts[3])?;
    let block = decode_block(&rlp)?;
    anyhow::ensure!(
        block.header.number == number,
        "line says block {number}, the block is #{}",
        block.header.number
    );
    anyhow::ensure!(
        block.header.hash() == hash,
        "block #{number}: the line's hash is {hash}, rustock computes {}",
        block.header.hash()
    );
    Ok(Some(ExportedBlock {
        number,
        hash,
        block,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_are_skipped_like_rskj_connect_blocks() {
        assert!(parse_export_line("").unwrap().is_none());
        assert!(parse_export_line("1,abc,01").unwrap().is_none());
    }

    #[test]
    fn malformed_lines_are_errors() {
        let hash = "00".repeat(32);
        assert!(parse_export_line(&format!("x,{hash},01,c0")).is_err());
        assert!(parse_export_line(&format!("1,{hash},01,zz")).is_err());
        // An empty list is not a block.
        assert!(parse_export_line(&format!("1,{hash},01,c0")).is_err());
    }
}
