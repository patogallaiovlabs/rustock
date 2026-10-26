//! Blocks mined by rskj on regtest import through `BlockImporter` with every
//! check passing, and a tampered block stops the import at the right check.
//!
//! `data/rskj-regtest-blocks-1-3.txt` holds blocks 1 to 3 of an rskj regtest
//! chain (regtest.conf with rskip144 = -1 and rskip97 = 0, the genesis in
//! rustock-execution's tests/data), in rskj's ExportBlocks format.

use std::sync::Arc;

use alloy_primitives::B256;
use rustock_core::config::{ActivationHeights, ChainConfig};
use rustock_core::rskj_config::{GenesisSpec, RskjActivations};
use rustock_core::validation::HeaderVerifier;
use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, CachedTrieStore};
use rustock_sync::import::{parse_export_line, BlockImporter, ImportError};
use rustock_trie::TrieStore;

const BLOCKS: &str = include_str!("data/rskj-regtest-blocks-1-3.txt");
const GENESIS: &str = include_str!("../../execution/tests/data/rskj-regtest-genesis.json");

fn chain() -> ChainConfig {
    let mut activations = RskjActivations::regtest();
    activations.apply_override("rskip144=-1").unwrap();
    activations.apply_override("rskip97=0").unwrap();
    let spec = GenesisSpec::from_json(GENESIS).unwrap();
    let mut config = ChainConfig::regtest_with(activations, Some(spec)).unwrap();
    // The chain was mined with rskj's miner.server.skipPowValidation.
    config.skip_pow_validation = true;
    config
}

fn blocks() -> Vec<Block> {
    // Header layout (RSKIP351 v2) comes from the process-wide heights.
    ActivationHeights::regtest().install();
    BLOCKS
        .lines()
        .map(|l| parse_export_line(l).unwrap().unwrap().block)
        .collect()
}

/// An importer on a fresh store holding the genesis, like `import-blocks`.
fn importer(dir: &std::path::Path) -> BlockImporter {
    let mut config = chain();
    let hardfork_cfg = RskHardforkConfig::for_chain(&config);
    let store = Arc::new(BlockStore::open(dir).unwrap());
    let trie_store: Arc<dyn TrieStore> =
        Arc::new(CachedTrieStore::with_defaults(store.db().clone()));
    let spec = config.genesis_spec.as_mut().unwrap();
    let root =
        rustock_execution::genesis::build_genesis_state(spec, &hardfork_cfg, trie_store.as_ref());
    spec.state_root = Some(root.compute_hash(trie_store.as_ref()));
    let genesis = config.genesis_header();
    let hash = config.known_genesis_hash().unwrap();
    store.put_header_with_hash(hash, &genesis).unwrap();
    store
        .put_total_difficulty(hash, genesis.difficulty)
        .unwrap();
    store.put_canonical_hash(0, hash).unwrap();
    store.set_head(hash).unwrap();

    let config = Arc::new(config);
    let processor = BlockProcessor::new(hardfork_cfg, store.clone());
    BlockImporter::new(
        store,
        HeaderVerifier::default_rsk(config),
        processor,
        trie_store,
        root,
        2,
    )
    .unwrap()
}

#[test]
fn imports_rskj_regtest_blocks() {
    let blocks = blocks();
    let expected: Vec<B256> = BLOCKS
        .lines()
        .map(|l| l.split(',').nth(1).unwrap().parse().unwrap())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let mut importer = importer(dir.path());
    for (block, hash) in blocks.into_iter().zip(expected) {
        let number = block.header.number;
        let imported = importer.import(block).unwrap();
        assert_eq!((imported.number, imported.hash), (number, hash));
        // flush_every = 2: blocks 2, 4, ...
        assert_eq!(imported.flushed, number % 2 == 0);
    }
    assert_eq!(importer.best().0, 3);
    importer.finish().unwrap();
}

#[test]
fn stops_at_a_block_that_is_not_the_next_one() {
    let blocks = blocks();
    let dir = tempfile::tempdir().unwrap();
    let mut importer = importer(dir.path());
    let err = importer.import(blocks[1].clone()).unwrap_err();
    assert!(
        matches!(err, ImportError::NotNextBlock { number: 2, .. }),
        "{err}"
    );
    assert_eq!(err.check(), "parent");
}

#[test]
fn stops_at_a_block_whose_state_root_differs() {
    let mut blocks = blocks();
    let dir = tempfile::tempdir().unwrap();
    let mut importer = importer(dir.path());
    let mut block = blocks.remove(0);
    block.header.state_root = B256::repeat_byte(0x11);
    let err = importer.import(block).unwrap_err();
    assert_eq!(err.check(), "stateRoot", "{err}");
}
