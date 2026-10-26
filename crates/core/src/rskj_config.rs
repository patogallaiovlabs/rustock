//! Chain settings expressed in rskj's own terms, so a regtest chain can be
//! configured exactly like an rskj node:
//!
//! - network-upgrade heights (`blockchain.config.hardforkActivationHeights`),
//! - per-RSKIP overrides (`blockchain.config.consensusRules`),
//! - the genesis JSON file (`genesis = <file>`, rskj `GenesisLoaderImpl`).
//!
//! Heights follow rskj: a negative height (`-1`) disables the upgrade or rule.

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use anyhow::{anyhow, bail, Context, Result};

use crate::config::ActivationHeights;
use crate::types::header::Header;

/// rskj network upgrades, in activation order (`NetworkUpgrade`).
pub const NETWORK_UPGRADES: [&str; 18] = [
    "bahamas",
    "afterBridgeSync",
    "orchid",
    "orchid060",
    "wasabi100",
    "twoToThree",
    "papyrus200",
    "iris300",
    "hop400",
    "hop401",
    "fingerroot500",
    "arrowhead600",
    "arrowhead631",
    "lovell700",
    "reed800",
    "reed810",
    "vetiver900",
    "tbd1000",
];

/// Consensus rules that can be overridden, with the network upgrade that
/// activates each one in rskj's reference.conf (`None`: disabled there).
/// These are the rules rustock consults individually; every other rule
/// follows its network upgrade.
pub const CONSENSUS_RULES: [(&str, Option<&str>); 8] = [
    ("rskip92", Some("orchid")),
    ("rskip97", Some("orchid")),
    ("rskip110", Some("wasabi100")),
    ("rskip144", Some("reed810")),
    ("rskip156", Some("papyrus200")),
    ("rskipUMM", Some("papyrus200")),
    ("rskip351", Some("reed810")),
    ("rskip535", None),
];

/// Network-upgrade heights plus consensus-rule overrides, as in an rskj
/// `blockchain.config` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RskjActivations {
    /// One entry per [`NETWORK_UPGRADES`] name, in that order. `None` = -1.
    upgrades: Vec<(&'static str, Option<u64>)>,
    /// Explicit `consensusRules` entries. `None` = -1.
    consensus_rules: Vec<(&'static str, Option<u64>)>,
}

impl RskjActivations {
    /// rskj `config/regtest.conf`.
    pub fn regtest() -> Self {
        let upgrades = NETWORK_UPGRADES
            .iter()
            .map(|name| {
                let height = match *name {
                    "afterBridgeSync" | "arrowhead631" => None,
                    _ => Some(0),
                };
                (*name, height)
            })
            .collect();
        Self {
            upgrades,
            consensus_rules: vec![
                ("rskip97", None),
                ("rskipUMM", Some(1)),
                ("rskip144", None),
                ("rskip351", Some(1)),
                ("rskip535", Some(1)),
            ],
        }
    }

    /// Height of a network upgrade (`None`: disabled or unknown name).
    pub fn upgrade_height(&self, name: &str) -> Option<u64> {
        self.upgrades
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, h)| *h)
    }

    /// All network upgrades with their heights, in activation order.
    pub fn upgrades(&self) -> impl Iterator<Item = (&'static str, Option<u64>)> + '_ {
        self.upgrades.iter().copied()
    }

    /// Height of a consensus rule: its override, else the height of the
    /// network upgrade that activates it in reference.conf.
    pub fn rule_height(&self, rule: &str) -> Option<u64> {
        if let Some((_, h)) = self.consensus_rules.iter().find(|(n, _)| *n == rule) {
            return *h;
        }
        CONSENSUS_RULES
            .iter()
            .find(|(n, _)| *n == rule)
            .and_then(|(_, upgrade)| upgrade.and_then(|u| self.upgrade_height(u)))
    }

    /// Apply one `name=height` override, where `name` is a network upgrade
    /// (`lovell700=0`) or a consensus rule (`rskip144=-1`) and `height` is a
    /// block number, `-1`, or (rules only) a network-upgrade name.
    pub fn apply_override(&mut self, spec: &str) -> Result<()> {
        let (name, value) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("expected name=height, got '{spec}'"))?;
        let (name, value) = (name.trim(), value.trim());
        if let Some(entry) = self.upgrades.iter_mut().find(|(n, _)| *n == name) {
            entry.1 = parse_height(value)?;
            return Ok(());
        }
        let rule = CONSENSUS_RULES
            .iter()
            .map(|(n, _)| *n)
            .find(|n| *n == name)
            .ok_or_else(|| {
                anyhow!(
                    "unknown network upgrade or unsupported consensus rule '{name}' \
                     (upgrades: {}; rules: {})",
                    NETWORK_UPGRADES.join(", "),
                    CONSENSUS_RULES.map(|(n, _)| n).join(", ")
                )
            })?;
        let height = if let Some(upgrade) = NETWORK_UPGRADES.iter().find(|u| **u == value) {
            self.upgrade_height(upgrade)
        } else {
            parse_height(value)?
        };
        self.consensus_rules.retain(|(n, _)| *n != rule);
        self.consensus_rules.push((rule, height));
        Ok(())
    }

    /// The per-rule heights the header layer uses. Fails for settings rustock
    /// cannot honour.
    pub fn activation_heights(&self) -> Result<ActivationHeights> {
        let h = |rule: &str| self.rule_height(rule).unwrap_or(u64::MAX);
        if self.rule_height("rskip144").is_some() {
            bail!("rskip144 (parallel transaction execution) is not implemented by rustock; set rskip144=-1");
        }
        Ok(ActivationHeights {
            orchid: h("rskip92"),
            wasabi100: h("rskip110"),
            papyrus200: h("rskip156"),
            rskip97: h("rskip97"),
            rskip_umm: h("rskipUMM"),
            rskip144: h("rskip144"),
            rskip351: h("rskip351"),
            rskip535: h("rskip535"),
        })
    }
}

fn parse_height(value: &str) -> Result<Option<u64>> {
    let n: i64 = value
        .parse()
        .with_context(|| format!("invalid height '{value}'"))?;
    Ok(u64::try_from(n).ok())
}

/// One `alloc` entry of an rskj genesis file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisAccount {
    pub address: Address,
    pub nonce: U256,
    pub balance: U256,
    /// Contract code and storage (`contract.code`, `contract.data`).
    pub code: Option<Bytes>,
    pub storage: Vec<(U256, Vec<u8>)>,
}

/// A genesis block loaded from an rskj genesis JSON file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisSpec {
    pub parent_hash: B256,
    pub coinbase: Address,
    pub timestamp: u64,
    pub extra_data: Bytes,
    pub gas_limit: U256,
    /// The `difficulty` field's exact bytes: rskj hashes them as given
    /// (`GenesisHeader.encodeBlockDifficulty`), leading zeros included.
    pub difficulty_bytes: Bytes,
    pub minimum_gas_price: U256,
    pub bitcoin_merged_mining_header: Bytes,
    pub bitcoin_merged_mining_merkle_proof: Bytes,
    pub bitcoin_merged_mining_coinbase_transaction: Bytes,
    pub accounts: Vec<GenesisAccount>,
    /// `stateRoot` from the file, or the root of the genesis state once built.
    pub state_root: Option<B256>,
    /// RSKIP92 active at genesis: the hash leaves out the merkle proof and
    /// coinbase transaction.
    pub use_rskip92_encoding: bool,
}

/// rskj `org.ethereum.json.Utils.parseData`: optional 0x, odd length padded.
fn parse_data(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let padded;
    let s = if s.len() % 2 == 1 {
        padded = format!("0{s}");
        padded.as_str()
    } else {
        s
    };
    hex::decode(s).with_context(|| format!("invalid hex '{s}'"))
}

fn json_str<'a>(obj: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(|v| v.as_str())
}

fn decimal(s: &str) -> Result<U256> {
    U256::from_str_radix(s, 10).with_context(|| format!("invalid decimal '{s}'"))
}

fn u256_from_be(bytes: &[u8]) -> Result<U256> {
    if bytes.len() > 32 {
        bail!("value longer than 32 bytes");
    }
    Ok(U256::from_be_slice(bytes))
}

impl GenesisSpec {
    /// Parse an rskj genesis JSON document (`GenesisJson`).
    pub fn from_json(json: &str) -> Result<Self> {
        let v: serde_json::Value =
            serde_json::from_str(json).context("genesis is not valid JSON")?;
        let field = |key: &str| -> Result<Vec<u8>> {
            parse_data(json_str(&v, key).ok_or_else(|| anyhow!("genesis lacks '{key}'"))?)
        };
        let optional = |key: &str| -> Result<Vec<u8>> {
            json_str(&v, key)
                .map(parse_data)
                .unwrap_or_else(|| Ok(Vec::new()))
        };

        let mut accounts = Vec::new();
        if let Some(alloc) = v.get("alloc").and_then(|a| a.as_object()) {
            for (key, entry) in alloc {
                // rskj GenesisLoaderImpl skips the "00" key.
                if key == "00" {
                    continue;
                }
                let addr_bytes = parse_data(key)?;
                if addr_bytes.len() != 20 {
                    bail!("alloc address '{key}' is not 20 bytes");
                }
                let balance = decimal(
                    json_str(entry, "balance")
                        .ok_or_else(|| anyhow!("alloc {key} lacks balance"))?,
                )?;
                // rskj: the network's initial nonce (0 on every network) when absent.
                let nonce = json_str(entry, "nonce")
                    .map(decimal)
                    .transpose()?
                    .unwrap_or(U256::ZERO);
                let (code, storage) = match entry.get("contract") {
                    Some(contract) => {
                        let code = parse_data(json_str(contract, "code").unwrap_or(""))?;
                        let mut storage = Vec::new();
                        if let Some(data) = contract.get("data").and_then(|d| d.as_object()) {
                            for (k, val) in data {
                                let key = u256_from_be(&parse_data(k)?)?;
                                let value = parse_data(val.as_str().unwrap_or(""))?;
                                storage.push((key, value));
                            }
                        }
                        (Some(Bytes::from(code)), storage)
                    }
                    None => (None, Vec::new()),
                };
                accounts.push(GenesisAccount {
                    address: Address::from_slice(&addr_bytes),
                    nonce,
                    balance,
                    code,
                    storage,
                });
            }
        }

        let parent_hash = field("parentHash")?;
        let coinbase = field("coinbase")?;
        let timestamp = u256_from_be(&field("timestamp")?)?;
        let gas_limit = u256_from_be(&field("gasLimit")?)?;
        let state_root = json_str(&v, "stateRoot").map(parse_data).transpose()?;
        Ok(Self {
            parent_hash: B256::try_from(parent_hash.as_slice())
                .context("parentHash is not 32 bytes")?,
            coinbase: if coinbase.is_empty() {
                Address::ZERO
            } else {
                Address::try_from(coinbase.as_slice()).context("coinbase is not 20 bytes")?
            },
            timestamp: u64::try_from(timestamp).context("timestamp out of range")?,
            extra_data: Bytes::from(field("extraData")?),
            gas_limit,
            difficulty_bytes: Bytes::from(field("difficulty")?),
            minimum_gas_price: u256_from_be(&optional("minimumGasPrice")?)?,
            bitcoin_merged_mining_header: Bytes::from(optional("bitcoinMergedMiningHeader")?),
            bitcoin_merged_mining_merkle_proof: Bytes::from(optional(
                "bitcoinMergedMiningMerkleProof",
            )?),
            bitcoin_merged_mining_coinbase_transaction: Bytes::from(optional(
                "bitcoinMergedMiningCoinbaseTransaction",
            )?),
            accounts,
            state_root: state_root
                .map(|r| B256::try_from(r.as_slice()).context("stateRoot is not 32 bytes"))
                .transpose()?,
            use_rskip92_encoding: true,
        })
    }

    /// Read and parse an rskj genesis JSON file.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read genesis file {}", path.display()))?;
        Self::from_json(&json).with_context(|| format!("in genesis file {}", path.display()))
    }

    fn has_mining_fields(&self) -> bool {
        !self.bitcoin_merged_mining_header.is_empty()
            || !self.bitcoin_merged_mining_merkle_proof.is_empty()
            || !self.bitcoin_merged_mining_coinbase_transaction.is_empty()
    }

    /// The genesis hash: rskj `GenesisHeader.getEncoded` (a v0 header with the
    /// raw difficulty bytes and the gas limit without leading zeros).
    pub fn hash(&self) -> B256 {
        const EMPTY_LIST_HASH: B256 = B256::new(hex_literal::hex!(
            "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
        ));
        const EMPTY_TRIE_HASH: B256 = B256::new(hex_literal::hex!(
            "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
        ));
        let mut list = Vec::new();
        self.parent_hash.encode(&mut list);
        EMPTY_LIST_HASH.encode(&mut list);
        if self.coinbase == Address::ZERO {
            // RLP.encodeRskAddress: the null address is an empty element.
            list.push(0x80);
        } else {
            self.coinbase.encode(&mut list);
        }
        self.state_root.unwrap_or_default().encode(&mut list);
        EMPTY_TRIE_HASH.encode(&mut list);
        EMPTY_TRIE_HASH.encode(&mut list);
        alloy_primitives::Bloom::ZERO.encode(&mut list);
        self.difficulty_bytes.encode(&mut list);
        0u64.encode(&mut list); // number
        let gas_limit = self.gas_limit.to_be_bytes::<32>();
        let start = gas_limit.iter().position(|b| *b != 0).unwrap_or(32);
        gas_limit[start..].encode(&mut list);
        0u64.encode(&mut list); // gasUsed
        self.timestamp.encode(&mut list);
        self.extra_data.encode(&mut list);
        U256::ZERO.encode(&mut list); // paidFees
                                      // RLP.encodeSignedCoinNonNullZero: zero is the single byte 0x00.
        let mgp = self.minimum_gas_price.to_be_bytes::<32>();
        match mgp.iter().position(|b| *b != 0) {
            None => list.push(0x00),
            Some(i) if mgp[i] & 0x80 != 0 => [&[0u8][..], &mgp[i..]].concat().encode(&mut list),
            Some(i) => mgp[i..].encode(&mut list),
        }
        0u64.encode(&mut list); // uncleCount
        if self.has_mining_fields() {
            self.bitcoin_merged_mining_header.encode(&mut list);
            if !self.use_rskip92_encoding {
                self.bitcoin_merged_mining_merkle_proof.encode(&mut list);
                self.bitcoin_merged_mining_coinbase_transaction
                    .encode(&mut list);
            }
        }
        let mut out = Vec::with_capacity(list.len() + 4);
        alloy_rlp::Header {
            list: true,
            payload_length: list.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&list);
        alloy_primitives::keccak256(&out)
    }

    /// The genesis header, with its hash cached (it cannot be recomputed from
    /// the header fields, see [`GenesisSpec::hash`]).
    pub fn header(&self) -> Header {
        let opt = |b: &Bytes| Some(b.clone());
        Header {
            parent_hash: self.parent_hash,
            ommers_hash: B256::new(hex_literal::hex!(
                "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
            )),
            beneficiary: self.coinbase,
            state_root: self.state_root.unwrap_or_default(),
            transactions_root: B256::new(hex_literal::hex!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            )),
            receipts_root: B256::new(hex_literal::hex!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            )),
            logs_bloom: Default::default(),
            extension_data: None,
            header_extension: None,
            difficulty: U256::from_be_slice(&self.difficulty_bytes),
            number: 0,
            gas_limit: self.gas_limit,
            gas_used: 0,
            timestamp: self.timestamp,
            extra_data: self.extra_data.clone(),
            paid_fees: U256::ZERO,
            minimum_gas_price: self.minimum_gas_price,
            uncle_count: 0,
            umm_root: None,
            bitcoin_merged_mining_header: opt(&self.bitcoin_merged_mining_header),
            bitcoin_merged_mining_merkle_proof: opt(&self.bitcoin_merged_mining_merkle_proof),
            bitcoin_merged_mining_coinbase_transaction: opt(
                &self.bitcoin_merged_mining_coinbase_transaction
            ),
            cached_hash: Some(self.hash()),
            cached_hash_for_merged_mining: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regtest_defaults_match_rskj_regtest_conf() {
        let a = RskjActivations::regtest();
        assert_eq!(a.upgrade_height("lovell700"), Some(0));
        assert_eq!(a.upgrade_height("arrowhead631"), None);
        assert_eq!(a.upgrade_height("afterBridgeSync"), None);
        assert_eq!(a.rule_height("rskip97"), None);
        assert_eq!(a.rule_height("rskipUMM"), Some(1));
        assert_eq!(a.rule_height("rskip351"), Some(1));
        assert_eq!(a.rule_height("rskip535"), Some(1));
        assert_eq!(a.rule_height("rskip144"), None);
        // Not overridden: follows its upgrade.
        assert_eq!(a.rule_height("rskip156"), Some(0));

        let h = a.activation_heights().unwrap();
        assert_eq!((h.orchid, h.wasabi100, h.papyrus200), (0, 0, 0));
        assert_eq!(h.rskip97, u64::MAX);
        assert_eq!((h.rskip_umm, h.rskip351, h.rskip535), (1, 1, 1));
        assert_eq!(h.rskip144, u64::MAX);
    }

    #[test]
    fn overrides_like_the_lab_rsk_conf() {
        let mut a = RskjActivations::regtest();
        a.apply_override("rskip144=-1").unwrap();
        a.apply_override("rskip97=0").unwrap();
        a.apply_override("iris300=5").unwrap();
        assert_eq!(a.rule_height("rskip97"), Some(0));
        assert_eq!(a.upgrade_height("iris300"), Some(5));
        assert_eq!(a.activation_heights().unwrap().rskip97, 0);

        // A rule may name an upgrade, as in rskj's reference.conf.
        a.apply_override("rskip97=iris300").unwrap();
        assert_eq!(a.rule_height("rskip97"), Some(5));
    }

    #[test]
    fn rejects_what_rustock_cannot_run() {
        let mut a = RskjActivations::regtest();
        a.apply_override("rskip144=0").unwrap();
        assert!(a.activation_heights().is_err());
        assert!(a.apply_override("rskip999=0").is_err());
        assert!(a.apply_override("lovell700").is_err());
        assert!(a.apply_override("lovell700=soon").is_err());
    }

    const GENESIS: &str = r#"{
      "alloc": {
        "0000000000000000000000000000000001000006": { "balance": "21000000000000000000000000" },
        "cd2a3d9f938e13cd947ec05abc7fe734df8dd826": { "balance": "1000", "nonce": "7" },
        "00": { "balance": "1" }
      },
      "nonce": "0xdeadbeefdeadbeef",
      "mixhash": "0x00",
      "bitcoinMergedMiningHeader": "0x00",
      "bitcoinMergedMiningMerkleProof": "0x00",
      "bitcoinMergedMiningCoinbaseTransaction": "0x00",
      "timestamp": "0x00",
      "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
      "extraData": "0x686f727365",
      "gasLimit": "0x017D7840",
      "difficulty": "0x0010000000",
      "coinbase": "0x3333333333333333333333333333333333333333",
      "minimumGasPrice": "0x00"
    }"#;

    #[test]
    fn parses_rskj_genesis_json() {
        let g = GenesisSpec::from_json(GENESIS).unwrap();
        assert_eq!(g.accounts.len(), 2, "the 00 key is skipped");
        let funded = g
            .accounts
            .iter()
            .find(|a| a.nonce == U256::from(7))
            .unwrap();
        assert_eq!(funded.balance, U256::from(1000));
        assert_eq!(g.gas_limit, U256::from(25_000_000));
        assert_eq!(g.difficulty_bytes.as_ref(), &[0x00, 0x10, 0x00, 0x00, 0x00]);
        assert_eq!(g.header().difficulty, U256::from(0x1000_0000u64));
        assert_eq!(g.extra_data.as_ref(), b"horse");
        assert!(g.state_root.is_none());
    }

    #[test]
    fn genesis_hash_keeps_the_raw_difficulty_bytes() {
        let mut g = GenesisSpec::from_json(GENESIS).unwrap();
        g.state_root = Some(B256::repeat_byte(1));
        let a = g.hash();
        g.difficulty_bytes = Bytes::from(vec![0x10, 0x00, 0x00, 0x00]);
        assert_ne!(a, g.hash(), "same value, different bytes, different hash");
        assert_eq!(
            g.header().hash(),
            g.hash(),
            "the header carries the cached hash"
        );
    }
}
