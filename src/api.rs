//! The JSON API, the same shape as The Doginals Explorer's `/v1/dfeed`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use axum::{
  Json, Router,
  extract::{Path, Query, State},
  http::StatusCode,
  response::{IntoResponse, Response},
  routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::index::{DfRecord, Index};

pub struct Shared {
  pub index: Index,
  /// The last block read.
  pub height: u64,
  pub start: u64,
  /// The node's tip, for confirmations.
  pub tip: u64,
}

pub type AppState = Arc<RwLock<Shared>>;

const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 100;

fn error(status: StatusCode, msg: &str) -> Response {
  (status, Json(json!({ "error": msg }))).into_response()
}

fn confirmations(tip: u64, height: u64) -> u64 {
  (tip + 1).saturating_sub(height)
}

fn counts(tip: u64, r: &DfRecord) -> bool {
  r.valid && confirmations(tip, r.height) >= dproto::CONFIRMATIONS
}

pub fn record_json(tip: u64, r: &DfRecord) -> Value {
  let conf = confirmations(tip, r.height);
  json!({
    "kind": r.kind,
    "valid": r.valid,
    "reason": r.reason,
    "status": if !r.valid { "invalid" } else if conf >= dproto::CONFIRMATIONS { "confirmed" } else { "pending" },
    "confirmations": conf,
    "txid": r.txid,
    "height": r.height,
    "txIndex": r.tx_index,
    "cursor": format!("{}:{}", r.height, r.tx_index),
    "time": r.time,
    "inscriptionId": r.inscription_id,
    "author": r.author,
    "account": r.account,
    "target": r.target,
    "text": r.text,
    "media": r.media,
    "tags": r.tags,
    "mentions": r.mentions,
    "ts": r.ts,
    "label": r.label,
    "ops": r.ops,
    "recipient": r.recipient,
    "recipientKoinu": r.recipient_koinu,
    "feeKoinu": r.fee_koinu,
    "splitBody": r.split_body,
    "body": r.body.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok()),
  })
}

fn post_summary(tip: u64, around: &[&DfRecord]) -> Value {
  let mut c = BTreeMap::<&str, u64>::new();
  let (mut likers, mut tipped, mut pending) = (BTreeSet::new(), 0u64, 0u64);
  let (mut edit, mut deleted, mut delete_pending) = (None::<&DfRecord>, false, false);
  for r in around.iter().filter(|r| r.valid) {
    if !counts(tip, r) {
      pending += 1;
      delete_pending |= r.kind == "delete";
      continue;
    }
    *c.entry(r.kind.as_str()).or_default() += 1;
    match r.kind.as_str() {
      "like" => {
        likers.insert(r.account.as_str());
      }
      "tip" | "reveal" => tipped += r.recipient_koinu.unwrap_or(0),
      "edit" => edit = Some(r),
      "delete" => deleted = true,
      _ => {}
    }
  }
  let get = |k: &str| c.get(k).copied().unwrap_or(0);
  json!({
    "likes": get("like"), "likers": likers.len(), "branches": get("branch"), "comments": get("comment"), "quotes": get("quote"),
    "tips": get("tip"), "reveals": get("reveal"), "tippedKoinu": tipped, "pending": pending,
    "deleted": deleted, "deletePending": delete_pending && !deleted,
    "edited": edit.map(|e| json!({ "text": e.text, "media": e.media, "inscriptionId": e.inscription_id, "height": e.height, "time": e.time })),
  })
}

fn with_summary(ix: &Index, tip: u64, r: &DfRecord) -> Value {
  let mut v = record_json(tip, r);
  if let Some(id) = r.inscription_id.as_deref().filter(|_| r.valid && matches!(r.kind.as_str(), "post" | "comment" | "quote")) {
    v["summary"] = post_summary(tip, &ix.pointing_at(id));
  }
  v
}

async fn status(State(s): State<AppState>) -> Response {
  let s = s.read().unwrap();
  let valid = s.index.records.iter().filter(|r| r.valid).count();
  Json(json!({
    "protocol": "D://", "version": 1, "startHeight": s.start, "height": s.height, "tip": s.tip,
    "records": s.index.records.len(), "valid": valid, "invalid": s.index.records.len() - valid,
    "confirmations": dproto::CONFIRMATIONS, "feeAddress": dproto::FEE_ADDRESS,
    "registrarKeys": dproto::REGISTRAR_KEYS.iter().map(|k| json!({ "kid": k.kid, "publicKey": k.pubkey_hex, "address": k.address })).collect::<Vec<_>>(),
  }))
  .into_response()
}

#[derive(Deserialize)]
struct ListQuery {
  before: Option<String>,
  limit: Option<usize>,
  kind: Option<String>,
  valid: Option<String>,
}

/// Records newest first, older than `before` (height:position).
fn newest<'a>(ix: &'a Index, before: Option<&str>, limit: usize, keep: impl Fn(&DfRecord) -> bool) -> Result<Vec<&'a DfRecord>, Response> {
  let cut = match before.map(str::trim).filter(|s| !s.is_empty()) {
    None => None,
    Some(b) => Some(
      b.split_once(':')
        .and_then(|(h, i)| Some((h.parse::<u64>().ok()?, i.parse::<u32>().ok()?)))
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "before is height:position, as a record's cursor"))?,
    ),
  };
  Ok(ix.records.iter().rev().filter(|r| cut.is_none_or(|c| (r.height, r.tx_index) < c)).filter(|r| keep(r)).take(limit).collect())
}

async fn feed(State(s): State<AppState>, Query(q): Query<ListQuery>) -> Response {
  let s = s.read().unwrap();
  let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
  match newest(&s.index, q.before.as_deref(), limit, |r| r.valid && matches!(r.kind.as_str(), "post" | "quote")) {
    Ok(items) => Json(json!({ "height": s.height, "items": items.iter().map(|r| with_summary(&s.index, s.tip, r)).collect::<Vec<_>>() })).into_response(),
    Err(e) => e,
  }
}

async fn activity(State(s): State<AppState>, Query(q): Query<ListQuery>) -> Response {
  let s = s.read().unwrap();
  let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
  let kinds: Option<Vec<String>> = q.kind.as_deref().filter(|k| !k.is_empty()).map(|k| k.split(',').map(|x| x.trim().to_string()).collect());
  let valid = match q.valid.as_deref().unwrap_or("all") {
    "all" => None,
    "valid" => Some(true),
    "invalid" => Some(false),
    _ => return error(StatusCode::BAD_REQUEST, "valid is valid, invalid or all"),
  };
  let keep = |r: &DfRecord| valid.is_none_or(|v| r.valid == v) && kinds.as_ref().is_none_or(|ks| ks.iter().any(|k| *k == r.kind));
  match newest(&s.index, q.before.as_deref(), limit, keep) {
    Ok(items) => Json(json!({ "height": s.height, "items": items.iter().map(|r| record_json(s.tip, r)).collect::<Vec<_>>() })).into_response(),
    Err(e) => e,
  }
}

async fn post(State(s): State<AppState>, Path(id): Path<String>) -> Response {
  let s = s.read().unwrap();
  let Some(r) = s.index.by_inscription(&id) else { return error(StatusCode::NOT_FOUND, "not a valid D:// post, comment or quote") };
  let around = s.index.pointing_at(&id);
  let mut v = record_json(s.tip, r);
  v["summary"] = post_summary(s.tip, &around);
  v["replies"] = json!(around.iter().filter(|x| matches!(x.kind.as_str(), "comment" | "quote")).map(|x| with_summary(&s.index, s.tip, x)).collect::<Vec<_>>());
  v["interactions"] = json!(around.iter().filter(|x| !matches!(x.kind.as_str(), "comment" | "quote")).map(|x| record_json(s.tip, x)).collect::<Vec<_>>());
  if let Some(p) = r.target.as_deref().filter(|_| matches!(r.kind.as_str(), "comment" | "quote")) {
    v["parent"] = s.index.by_inscription(p).map(|p| record_json(s.tip, p)).unwrap_or(Value::Null);
  }
  Json(v).into_response()
}

async fn user(State(s): State<AppState>, Path(address): Path<String>) -> Response {
  if dproto::address_hash(&address).is_none() {
    return error(StatusCode::BAD_REQUEST, "not a Dogecoin address");
  }
  let s = s.read().unwrap();
  let tip = s.tip;
  let mine = s.index.by_account(&address);
  let followers: BTreeSet<&str> = s.index.pointing_at(&address).into_iter().filter(|r| r.kind == "follow" && counts(tip, r)).map(|r| r.account.as_str()).collect();
  let following: BTreeSet<&str> =
    mine.iter().filter(|r| r.kind == "follow" && r.account == address && counts(tip, r)).filter_map(|r| r.target.as_deref()).collect();
  let n = |k: &str| mine.iter().filter(|r| r.kind == k && r.account == address && counts(tip, r)).count();
  let keys: Vec<Value> = mine
    .iter()
    .filter(|r| r.kind == "keylink" && r.valid && r.account == address)
    .map(|r| {
      let revoked = mine.iter().find(|x| x.kind == "keyrevoke" && x.valid && x.target == r.target).map(|x| x.height);
      json!({ "key": r.target, "label": r.label, "ops": r.ops, "fromHeight": r.height, "revokedAt": revoked })
    })
    .collect();
  let items: Vec<Value> = mine
    .iter()
    .rev()
    .take(MAX_LIMIT)
    .map(|r| if r.account == address { with_summary(&s.index, tip, r) } else { record_json(tip, r) })
    .collect();
  Json(json!({
    "address": address, "height": s.height, "posts": n("post"), "comments": n("comment"), "quotes": n("quote"),
    "likesGiven": n("like"), "followers": followers, "following": following, "linkedKeys": keys, "records": mine.len(), "items": items,
  }))
  .into_response()
}

async fn tx(State(s): State<AppState>, Path(txid): Path<String>) -> Response {
  let s = s.read().unwrap();
  match s.index.by_txid(&txid.to_ascii_lowercase()) {
    Some(r) => Json(record_json(s.tip, r)).into_response(),
    None => error(StatusCode::NOT_FOUND, "no D:// record in that transaction (or not read yet)"),
  }
}

pub fn router(state: AppState) -> Router {
  Router::new()
    .route("/v1/dfeed", get(status))
    .route("/v1/dfeed/feed", get(feed))
    .route("/v1/dfeed/activity", get(activity))
    .route("/v1/dfeed/post/:id", get(post))
    .route("/v1/dfeed/user/:address", get(user))
    .route("/v1/dfeed/tx/:txid", get(tx))
    .with_state(state)
}
