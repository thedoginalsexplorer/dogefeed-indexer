//! dogefeed-indexer: reads D:// (DogeFeed's protocol) from a Dogecoin Core
//! node and serves it as JSON. See the README for settings.

mod api;
mod envelope;
mod index;
mod rpc;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use envelope::{Chain, Chains};
use index::{DfRecord, Index};
use rpc::{Rpc, Tx};

/// First block read by default: before the first D:// v1 record (6,448,293).
const DEFAULT_START: u64 = 6_440_000;
/// Blocks that can be taken back on a reorg.
const UNDO_BLOCKS: usize = 100;
/// Unfinished reveal chains older than this are dropped.
const CHAIN_KEEP: u64 = 50_000;

fn env(k: &str) -> Option<String> {
  std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// What one block changed in the reveal chains, to undo it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct BlockUndo {
  height: u64,
  hash: String,
  chains: Vec<(String, Option<Chain>)>,
}

/// What `state.json` holds. An older file is set aside and D:// is read again
/// from the start: 2 added the registrar key and reveal payouts.
const STATE_VERSION: u64 = 2;

#[derive(Default, Serialize, Deserialize)]
struct Saved {
  #[serde(default)]
  version: u64,
  next_height: u64,
  records: Vec<DfRecord>,
  chains: Chains,
  undo: VecDeque<BlockUndo>,
}

fn load(path: &PathBuf, start: u64) -> Result<Saved> {
  let fresh = Saved { version: STATE_VERSION, next_height: start, ..Default::default() };
  if !path.exists() {
    return Ok(fresh);
  }
  let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
  let saved: Saved = serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
  if saved.version < STATE_VERSION {
    let old = path.with_extension(format!("v{}.json", saved.version));
    std::fs::rename(path, &old)?;
    warn!("{} is from an older version, kept as {}; reading D:// again from block {start}", path.display(), old.display());
    return Ok(fresh);
  }
  Ok(saved)
}

fn save(path: &PathBuf, s: &Saved) -> Result<()> {
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, serde_json::to_vec(s)?)?;
  std::fs::rename(&tmp, path)?;
  Ok(())
}

/// Input addresses: from the block itself, else from the node (txindex=1).
async fn input_addresses(rpc: &Rpc, tx: &Tx, local: &HashMap<&str, &Tx>, cache: &mut HashMap<String, Tx>) -> Result<Vec<Option<String>>> {
  let mut out = Vec::new();
  for v in &tx.vin {
    let (Some(txid), Some(vout)) = (v.txid.as_deref(), v.vout) else {
      out.push(None);
      continue;
    };
    let prev = match local.get(txid) {
      Some(p) => (*p).clone(),
      None => {
        if !cache.contains_key(txid) {
          cache.insert(txid.to_string(), rpc.tx(txid).await?);
        }
        cache[txid].clone()
      }
    };
    out.push(prev.vout.get(vout as usize).and_then(|o| dproto::script_address(&o.script())));
  }
  Ok(out)
}

/// Reads one block into `ix` (so later transactions see earlier records),
/// returning the records it adds. `journal` gets each reveal chain it
/// changes, as it was before.
async fn read_block(rpc: &Rpc, ix: &mut Index, chains: &mut Chains, block: &rpc::Block, journal: &mut Vec<(String, Option<Chain>)>) -> Result<Vec<DfRecord>> {
  let local: HashMap<&str, &Tx> = block.tx.iter().map(|t| (t.txid.as_str(), t)).collect();
  let mut cache = HashMap::new();
  let mut found = Vec::new();
  for (i, tx) in block.tx.iter().enumerate() {
    let i = i as u32;
    // The chains this transaction may change, as they were.
    if let Some(prev) = tx.vin.first().and_then(|v| v.txid.clone()) {
      journal.push((prev.clone(), chains.partial.get(&prev).cloned()));
    }
    journal.push((tx.txid.clone(), chains.partial.get(&tx.txid).cloned()));
    let mut record = chains
      .push(tx, block.height)
      .and_then(|c| ix.inscription(block.height, i, block.time, &tx.txid, &tx.outputs(), &c.id, &c.content_type, &c.body, &c.script));
    if record.is_none() && tx.vout.iter().any(|o| o.script_pub_key.hex.starts_with("6a4c50443a2f2f")) {
      let addrs = input_addresses(rpc, tx, &local, &mut cache).await?;
      let inputs: Vec<dproto::Outpoint> =
        tx.vin.iter().map(|v| dproto::Outpoint { txid: v.txid.clone().unwrap_or_default(), vout: v.vout.unwrap_or(0) }).collect();
      record = ix.payment(block.height, i, block.time, &tx.txid, &inputs, &addrs, &tx.outputs());
    }
    if let Some(r) = record {
      ix.add(r.clone());
      found.push(r);
    }
  }
  Ok(found)
}

fn undo_chains(chains: &mut Chains, journal: Vec<(String, Option<Chain>)>) {
  for (k, old) in journal.into_iter().rev() {
    match old {
      Some(c) => chains.partial.insert(k, c),
      None => chains.partial.remove(&k),
    };
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
    .init();
  let rpc = Rpc::new(
    env("DFEED_RPC_URL").unwrap_or_else(|| "http://127.0.0.1:22555".into()),
    env("DFEED_RPC_USER"),
    env("DFEED_RPC_PASSWORD"),
    env("DFEED_DOGECOIN_CONF"),
  )?;
  let start: u64 = env("DFEED_START").map(|s| s.parse()).transpose().context("DFEED_START")?.unwrap_or(DEFAULT_START);
  let dir = PathBuf::from(env("DFEED_DATA_DIR").unwrap_or_else(|| "dfeed-data".into()));
  std::fs::create_dir_all(&dir)?;
  let path = dir.join("state.json");
  let mut saved = load(&path, start)?;
  let tip = rpc.block_count().await.context("is the node running, with server=1 and txindex=1?")?;
  info!("reading D:// from block {} (node at {tip}), {} records so far", saved.next_height, saved.records.len());

  let shared = Arc::new(RwLock::new(api::Shared {
    index: Index::from_records(std::mem::take(&mut saved.records)),
    height: saved.next_height.saturating_sub(1),
    start,
    tip,
  }));
  let listen = env("DFEED_LISTEN").unwrap_or_else(|| "127.0.0.1:8795".into());
  if listen != "off" {
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("listen on {listen}"))?;
    info!("API on http://{listen}/v1/dfeed");
    let app = api::router(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
  }

  let mut own = Index::from_records(shared.read().unwrap().index.records.clone());
  let (mut last_save, mut last_log, mut dirty) = (Instant::now(), Instant::now(), false);
  loop {
    let tip = match rpc.block_count().await {
      Ok(t) => t,
      Err(e) => {
        warn!("{e:#}");
        tokio::time::sleep(Duration::from_secs(10)).await;
        continue;
      }
    };
    shared.write().unwrap().tip = tip;
    let h = saved.next_height;
    if h > tip {
      if dirty {
        saved.records = shared.read().unwrap().index.records.clone();
        save(&path, &saved)?;
        saved.records.clear();
        dirty = false;
      }
      tokio::time::sleep(Duration::from_secs(5)).await;
      continue;
    }
    let block = match rpc.block(h).await {
      Ok(b) => b,
      Err(e) => {
        warn!("block {h}: {e:#}");
        tokio::time::sleep(Duration::from_secs(10)).await;
        continue;
      }
    };
    // A reorg: take the last block back and look again.
    if let Some(last) = saved.undo.back()
      && last.height + 1 == h
      && block.previousblockhash.as_deref() != Some(last.hash.as_str())
    {
      let last = saved.undo.pop_back().unwrap();
      undo_chains(&mut saved.chains, last.chains);
      let mut s = shared.write().unwrap();
      let kept: Vec<DfRecord> = s.index.records.iter().filter(|r| r.height < last.height).cloned().collect();
      let gone = s.index.records.len() - kept.len();
      s.index = Index::from_records(kept);
      own = Index::from_records(s.index.records.clone());
      s.height = last.height - 1;
      saved.next_height = last.height;
      warn!("reorg at block {}: {gone} records taken back", last.height);
      dirty = true;
      continue;
    }
    // The indexer's own copy; the API's is updated once the block is read.
    let mut journal = Vec::new();
    let records = match read_block(&rpc, &mut own, &mut saved.chains, &block, &mut journal).await {
      Ok(x) => x,
      Err(e) => {
        // Nothing of the block stays.
        undo_chains(&mut saved.chains, journal);
        own = Index::from_records(shared.read().unwrap().index.records.clone());
        warn!("block {h}: {e:#}; reading it again");
        tokio::time::sleep(Duration::from_secs(10)).await;
        continue;
      }
    };
    {
      let mut s = shared.write().unwrap();
      for r in records {
        info!("block {h}: {} {} {}", r.kind, if r.valid { "valid" } else { "invalid" }, r.reason.as_deref().unwrap_or(""));
        s.index.add(r);
      }
      s.height = h;
    }
    saved.undo.push_back(BlockUndo { height: h, hash: block.hash.clone(), chains: journal });
    while saved.undo.len() > UNDO_BLOCKS {
      saved.undo.pop_front();
    }
    if h % 1000 == 0 {
      saved.chains.prune(h, CHAIN_KEEP);
    }
    saved.next_height = h + 1;
    dirty = true;
    if last_save.elapsed() > Duration::from_secs(60) {
      saved.records = shared.read().unwrap().index.records.clone();
      save(&path, &saved)?;
      saved.records.clear();
      last_save = Instant::now();
      dirty = false;
    }
    if last_log.elapsed() > Duration::from_secs(30) {
      last_log = Instant::now();
      let s = shared.read().unwrap();
      info!("at block {h} of {tip}: {} records", s.index.records.len());
    }
  }
}
