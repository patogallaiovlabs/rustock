//! `rustock-cli import-blocks`: replay a file of blocks exported by rskj's
//! ExportBlocks tool into an empty data directory.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustock_core::validation::HeaderVerifier;
use rustock_storage::BlockStore;
use rustock_sync::import::{parse_export_line, BlockImporter, ImportTimings, ImportedBlock};
use tracing_subscriber::EnvFilter;

use crate::{load_or_build_state, prepare_genesis_spec, setup_genesis, ChainArgs};

#[derive(clap::Args, Debug)]
pub struct ImportBlocksArgs {
    /// Blocks to import, one per line as `number,hash,totalDifficulty,rlp`
    /// (hex), plain or gzip-compressed.
    #[arg(long)]
    file: PathBuf,

    /// Data directory; must not hold a chain yet.
    #[arg(long = "datadir", alias = "data-dir")]
    datadir: PathBuf,

    /// Network ID (33 for regtest).
    #[arg(long, default_value_t = 33)]
    network_id: u64,

    /// Flush the trie store to disk every this many blocks (rskj
    /// `blockchain.flushNumberOfBlocks`), and once more at the end.
    #[arg(long, default_value_t = 10)]
    flush_every: u64,

    /// Write one line per imported block to this file, in rskj's
    /// `block connect breakdown best:` format, with per-stage timings.
    #[arg(long)]
    breakdown_log: Option<PathBuf>,

    /// Log level for messages on stderr (RUST_LOG-style directives work too).
    #[arg(long, default_value = "warn")]
    log_level: String,

    #[command(flatten)]
    chain: ChainArgs,
}

/// Open a file, transparently decompressing gzip (detected by its magic bytes).
fn open_lines(path: &Path) -> Result<Box<dyn BufRead>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    let file = std::fs::File::open(path)?;
    Ok(if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    })
}

pub fn run(args: ImportBlocksArgs) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let mut config = args.chain.chain_config(args.network_id)?;
    config.activation_heights.clone().install();
    let hardfork_cfg = rustock_execution::RskHardforkConfig::for_chain(&config);

    std::fs::create_dir_all(&args.datadir).context("cannot create the data directory")?;
    let store = Arc::new(BlockStore::open(&args.datadir)?);
    anyhow::ensure!(
        store.head()?.is_none(),
        "{} already holds a chain; import-blocks needs an empty data directory",
        args.datadir.display()
    );
    let trie_store: Arc<dyn rustock_trie::TrieStore> = Arc::new(
        rustock_storage::CachedTrieStore::with_defaults(store.db().clone()),
    );
    prepare_genesis_spec(&mut config, &hardfork_cfg, trie_store.as_ref());
    let config = Arc::new(config);
    let genesis_hash = setup_genesis(&store, &config)?;
    let state_root = load_or_build_state(&store, &config, &hardfork_cfg, trie_store.as_ref())?;
    trie_store.flush();
    eprintln!("genesis {genesis_hash}");

    let processor = rustock_execution::BlockProcessor::new(hardfork_cfg, store.clone());
    let verifier = HeaderVerifier::default_rsk(config.clone());
    let mut importer = BlockImporter::new(
        store,
        verifier,
        processor,
        trie_store,
        state_root,
        args.flush_every,
    )?
    .measure(args.breakdown_log.is_some());
    let mut breakdown = match &args.breakdown_log {
        Some(path) => {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let file = std::fs::File::create(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            Some(BufWriter::new(file))
        }
        None => None,
    };

    let started = Instant::now();
    let mut first = None;
    let mut count = 0u64;
    for (index, line) in open_lines(&args.file)?.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", args.file.display()))?;
        let exported = match parse_export_line(&line) {
            Ok(Some(exported)) => exported,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("import failed at line {}: decode: {e:#}", index + 1);
                flush_breakdown(&mut breakdown);
                std::process::exit(1);
            }
        };
        let (number, hash) = (exported.number, exported.hash);
        match importer.import(exported.block) {
            Ok(imported) => {
                first.get_or_insert(number);
                count += 1;
                // Written after the block's timings were taken: not part of them.
                if let (Some(out), Some(t)) = (breakdown.as_mut(), imported.timings.as_ref()) {
                    writeln!(out, "{}", breakdown_line(&imported, t))?;
                }
            }
            Err(e) => {
                eprintln!(
                    "import failed at block {number} hash {hash}: check={}: {e}",
                    e.check()
                );
                flush_breakdown(&mut breakdown);
                std::process::exit(1);
            }
        }
    }
    importer.finish()?;
    flush_breakdown(&mut breakdown);

    let range = match first {
        Some(first) => format!(" (#{first}..#{})", first + count - 1),
        None => String::new(),
    };
    eprintln!(
        "imported {count} blocks{range} in {:.3} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn flush_breakdown(out: &mut Option<BufWriter<std::fs::File>>) {
    if let Some(out) = out {
        let _ = out.flush();
    }
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

/// One `block connect breakdown best:` line, in rskj's format
/// (BlockChainImpl.internalTryToConnect) so the same analyzer reads both
/// clients. Keys named like rskj's cover the same work; the others are
/// rustock's own stages, which add up to them (see the import-blocks docs).
fn breakdown_line(b: &ImportedBlock, t: &ImportTimings) -> String {
    let p = &t.process;
    let tx_execution = p.sender_recovery + p.execution + p.receipts;
    let state_persist = p.apply_state + p.state_root + p.trie_save;
    let fields = [
        ("totalMs", t.total),
        ("preambleMs", t.preamble),
        (
            "initialValidationMs",
            t.header_validation + p.pre_validation,
        ),
        ("executeMs", tx_execution + state_persist),
        (
            "postExecuteValidationMs",
            p.receipts_root + p.post_validation,
        ),
        ("switchChainMs", t.store_block),
        ("saveReceiptsMs", p.save_receipts),
        ("onBestBlockMs", t.flush + t.exec_head),
        ("txExecutionMs", tx_execution),
        ("statePersistMs", state_persist),
        ("headerValidationMs", t.header_validation),
        ("txRootValidationMs", p.pre_validation),
        ("senderRecoveryMs", p.sender_recovery),
        ("evmMs", p.execution),
        ("receiptsMs", p.receipts),
        ("applyStateMs", p.apply_state),
        ("stateRootMs", p.state_root),
        ("trieSaveMs", p.trie_save),
        ("receiptsRootMs", p.receipts_root),
        ("flushMs", t.flush),
        ("execHeadMs", t.exec_head),
    ];
    let mut line = format!(
        "block connect breakdown best: block={} hash={}",
        b.number,
        alloy_primitives::hex::encode(b.hash)
    );
    for (key, value) in fields {
        line.push_str(&format!(" {key}={}", ms(value)));
    }
    line.push_str(&format!(" txCount={} gasUsed={}", b.tx_count, b.gas_used));
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rustock-import-{}-{name}", std::process::id()))
    }

    #[test]
    fn reads_plain_and_gzip_files() {
        let text = "1,aa,01,c0\n2,bb,02,c0\n";
        let plain = temp_path("plain.txt");
        std::fs::write(&plain, text).unwrap();
        let gz = temp_path("blocks.txt.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(text.as_bytes()).unwrap();
        std::fs::write(&gz, enc.finish().unwrap()).unwrap();

        for path in [&plain, &gz] {
            let lines: Vec<String> = open_lines(path)
                .unwrap()
                .lines()
                .map(|l| l.unwrap())
                .collect();
            assert_eq!(lines, ["1,aa,01,c0", "2,bb,02,c0"]);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn breakdown_line_has_rskj_marker_and_keys() {
        let block = ImportedBlock {
            number: 7,
            hash: alloy_primitives::B256::repeat_byte(0xab),
            tx_count: 3,
            gas_used: 21_000,
            flushed: false,
            timings: None,
        };
        let mut t = ImportTimings {
            total: Duration::from_micros(12_345),
            ..Default::default()
        };
        t.process.execution = Duration::from_micros(1_500);
        t.process.trie_save = Duration::from_micros(250);
        let line = breakdown_line(&block, &t);
        assert!(line.starts_with(&format!(
            "block connect breakdown best: block=7 hash={} totalMs=12.345 ",
            "ab".repeat(32)
        )));
        assert!(line.contains(" executeMs=1.750 "));
        assert!(line.contains(" evmMs=1.500 "));
        assert!(line.contains(" statePersistMs=0.250 "));
        assert!(line.contains(" flushMs=0.000 "));
        assert!(line.ends_with(" txCount=3 gasUsed=21000"));
    }
}
