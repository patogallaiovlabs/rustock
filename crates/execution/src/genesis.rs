//! Genesis state of a chain loaded from an rskj genesis file, built the way
//! rskj `GenesisLoaderImpl.loadGenesisTrie` builds it.

use alloy_primitives::{Address, B256, U256};
use rustock_core::rskj_config::GenesisSpec;
use rustock_trie::{
    account_key, code_key, storage_key, storage_prefix_key, AccountState, TrieKeySlice, TrieNode,
    TrieStore,
};

use crate::hardfork::{RskHardforkConfig, RskNetworkUpgrade};
use crate::precompiles::{CONSENSUS_ENABLED_PRECOMPILES, GENESIS_PRECOMPILES};

fn put(root: TrieNode, key: &[u8], value: &[u8], store: &dyn TrieStore) -> TrieNode {
    root.put(&TrieKeySlice::from_key(key), value, store)
}

fn put_account(
    root: TrieNode,
    addr: &Address,
    acct: &AccountState,
    store: &dyn TrieStore,
) -> TrieNode {
    put(root, &account_key(addr), &acct.encode(), store)
}

/// rskj `Repository.setupContract`: the storage-prefix marker.
fn setup_contract(root: TrieNode, addr: &Address, store: &dyn TrieStore) -> TrieNode {
    put(root, &storage_prefix_key(addr), &[1], store)
}

/// Build the genesis state trie and save it to `store`; returns its root.
///
/// rskj order (`loadGenesisInitalState`, then
/// `setupPrecompiledContractsStorage`):
/// 1. every alloc account is created; accounts with a contract get the
///    storage-prefix marker, their code and their storage;
/// 2. every alloc account gets its nonce and balance;
/// 3. if RSKIP126 (wasabi100) activates at block 0, rskj
///    `BlockExecutor.maintainPrecompiledContractStorageRoots` runs for block 0:
///    the genesis precompiles are created if absent and marked, and every
///    precompile whose upgrade activates at block 0 is re-created (nonce and
///    balance reset) and marked.
pub fn build_genesis_state(
    spec: &GenesisSpec,
    hardfork_cfg: &RskHardforkConfig,
    store: &dyn TrieStore,
) -> TrieNode {
    let mut root = TrieNode::empty();

    for account in &spec.accounts {
        root = put_account(root, &account.address, &AccountState::default(), store);
        if let Some(code) = &account.code {
            root = setup_contract(root, &account.address, store);
            if !code.is_empty() {
                root = put(root, &code_key(&account.address), code, store);
            }
            for (slot, value) in &account.storage {
                // rskj addStorageBytes: an empty value deletes the cell.
                if !value.is_empty() {
                    let key = storage_key(&account.address, &B256::from(*slot));
                    root = put(root, &key, value, store);
                }
            }
        }
    }
    for account in &spec.accounts {
        let state = AccountState::new(account.nonce, account.balance);
        root = put_account(root, &account.address, &state, store);
    }

    if hardfork_cfg.is_activating(RskNetworkUpgrade::Wasabi100, 0) {
        for addr in GENESIS_PRECOMPILES {
            let exists = root
                .get(&TrieKeySlice::from_key(&account_key(&addr)), store)
                .is_some();
            if !exists {
                root = put_account(root, &addr, &AccountState::default(), store);
            }
            root = setup_contract(root, &addr, store);
        }
    }
    for (addr, upgrade) in CONSENSUS_ENABLED_PRECOMPILES {
        if hardfork_cfg.is_activating(upgrade, 0) {
            let fresh = AccountState::new(U256::ZERO, U256::ZERO);
            root = put_account(root, &addr, &fresh, store);
            root = setup_contract(root, &addr, store);
        }
    }

    root.save(store, true);
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_core::rskj_config::{GenesisAccount, RskjActivations};
    use rustock_trie::MemoryTrieStore;

    fn spec(accounts: Vec<GenesisAccount>) -> GenesisSpec {
        let mut spec = GenesisSpec::from_json(
            r#"{"alloc": {}, "timestamp": "0x00", "extraData": "0x",
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "gasLimit": "0x017D7840", "difficulty": "0x0010000000", "coinbase": "0x"}"#,
        )
        .unwrap();
        spec.accounts = accounts;
        spec
    }

    fn get(root: &TrieNode, key: &[u8], store: &MemoryTrieStore) -> Option<Vec<u8>> {
        root.get(&TrieKeySlice::from_key(key), store)
    }

    #[test]
    fn regtest_genesis_marks_every_precompile_like_rskj() {
        let bridge = crate::precompiles::BRIDGE_ADDR;
        let balance = U256::from(21_000_000u64) * U256::from(10u64).pow(U256::from(18));
        let store = MemoryTrieStore::new();
        let cfg = RskHardforkConfig::from_rskj(&RskjActivations::regtest(), 33);
        let root = build_genesis_state(
            &spec(vec![GenesisAccount {
                address: bridge,
                nonce: U256::ZERO,
                balance,
                code: None,
                storage: vec![],
            }]),
            &cfg,
            &store,
        );

        // The Bridge keeps its alloc balance and gets the marker.
        let acct =
            AccountState::decode(&get(&root, &account_key(&bridge), &store).unwrap()).unwrap();
        assert_eq!(acct.balance, balance);
        for addr in GENESIS_PRECOMPILES
            .iter()
            .chain(CONSENSUS_ENABLED_PRECOMPILES.iter().map(|(a, _)| a))
        {
            assert!(
                get(&root, &account_key(addr), &store).is_some(),
                "{addr} exists"
            );
            assert_eq!(
                get(&root, &storage_prefix_key(addr), &store),
                Some(vec![1]),
                "{addr} marked"
            );
        }
    }

    #[test]
    fn contract_alloc_gets_code_storage_and_marker() {
        let addr = Address::repeat_byte(0xaa);
        let store = MemoryTrieStore::new();
        let cfg = RskHardforkConfig::mainnet(); // no RSKIP126 at block 0: no precompiles
        let root = build_genesis_state(
            &spec(vec![GenesisAccount {
                address: addr,
                nonce: U256::from(1),
                balance: U256::from(5),
                code: Some(vec![0x60, 0x00].into()),
                storage: vec![(U256::from(1), vec![0x2a])],
            }]),
            &cfg,
            &store,
        );
        assert_eq!(get(&root, &code_key(&addr), &store), Some(vec![0x60, 0x00]));
        assert_eq!(
            get(&root, &storage_prefix_key(&addr), &store),
            Some(vec![1])
        );
        let slot = B256::from(U256::from(1));
        assert_eq!(
            get(&root, &storage_key(&addr, &slot), &store),
            Some(vec![0x2a])
        );
        assert!(get(
            &root,
            &account_key(&crate::precompiles::BRIDGE_ADDR),
            &store
        )
        .is_none());
    }
}
