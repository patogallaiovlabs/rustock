//! A regtest chain loaded from an rskj genesis file has rskj's genesis hash.
//!
//! `data/rskj-regtest-genesis.json` is a regtest genesis with fifteen funded
//! accounts and the Bridge holding 21M RBTC. rskj (regtest.conf, this file as
//! `genesis`) gives it the hash below: block 1 of chains it mined from this
//! genesis names it as parent.

use alloy_primitives::{b256, U256};
use rustock_core::config::ChainConfig;
use rustock_core::rskj_config::{GenesisSpec, RskjActivations};
use rustock_execution::genesis::build_genesis_state;
use rustock_execution::RskHardforkConfig;
use rustock_trie::MemoryTrieStore;

#[test]
fn rskj_regtest_genesis_hash() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/rskj-regtest-genesis.json"
    );
    let spec = GenesisSpec::from_file(std::path::Path::new(path)).unwrap();
    let mut config = ChainConfig::regtest_with(RskjActivations::regtest(), Some(spec)).unwrap();
    let hardfork_cfg = RskHardforkConfig::for_chain(&config);

    let store = MemoryTrieStore::new();
    let spec = config.genesis_spec.as_mut().unwrap();
    let root = build_genesis_state(spec, &hardfork_cfg, &store);
    spec.state_root = Some(root.compute_hash(&store));

    assert_eq!(
        config.known_genesis_hash(),
        Some(b256!(
            "227d8e8d835f5e7290e333f4177e551ef1222dde4704f677ba4a120051c31cc2"
        ))
    );
    let header = config.genesis_header();
    assert_eq!(header.gas_limit, U256::from(25_000_000));
    assert_eq!(header.difficulty, U256::from(0x1000_0000u64));
}
