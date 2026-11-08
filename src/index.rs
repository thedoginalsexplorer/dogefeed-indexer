//! D:// records in chain order, with the lookups checking needs, in memory.
//! Derived state (counts, follows, deletes, edits) is worked out when read.

use std::collections::HashMap;

use dproto::{Detect, Op, Outpoint, PaymentTx, PostCandidate, Record, RecordType, Target, TxOut};
use serde::{Deserialize, Serialize};

/// One D:// record: an inscription or a payment, valid or not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DfRecord {
  pub txid: String,
  pub height: u64,
  pub tx_index: u32,
  pub time: u64,
  pub kind: String,
  pub inscription_id: Option<String>,
  pub author: String,
  pub account: String,
  pub target: Option<String>,
  pub text: Option<String>,
  #[serde(default)]
  pub media: Vec<String>,
  #[serde(default)]
  pub tags: Vec<String>,
  #[serde(default)]
  pub mentions: Vec<String>,
  pub ts: Option<u64>,
  pub body: Option<String>,
  pub label: Option<String>,
  #[serde(default)]
  pub ops: Vec<String>,
  /// Who it pays, and how much: from the OP_RETURN payment, or for a comment
  /// or quote, what its reveal pays the original author it answers.
  pub recipient: Option<String>,
  pub recipient_koinu: Option<u64>,
  /// What it pays DogeFeed.
  pub fee_koinu: Option<u64>,
  /// The registrar key (`kid`) whose signature on it verifies: DogeFeed's
  /// signature, showing it was issued through DogeFeed.
  #[serde(default)]
  pub registrar: Option<String>,
  #[serde(default)]
  pub split_body: bool,
  pub valid: bool,
  pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Link {
  account: String,
  ops: Vec<String>,
  from_height: u64,
  revoked_at: Option<u64>,
}

#[derive(Default)]
pub struct Index {
  pub records: Vec<DfRecord>,
  by_txid: HashMap<String, usize>,
  by_inscription: HashMap<String, usize>,
  prefix: HashMap<[u8; 11], Vec<usize>>,
  by_account: HashMap<String, Vec<usize>>,
  pointing: HashMap<String, Vec<usize>>,
  links: HashMap<String, Link>,
  keys_of: HashMap<String, Vec<String>>,
}

impl Index {
  pub fn from_records(records: Vec<DfRecord>) -> Self {
    let mut ix = Self::default();
    for r in records {
      ix.add(r);
    }
    ix
  }

  /// Adds a record after every one before it in chain order.
  pub fn add(&mut self, r: DfRecord) {
    let n = self.records.len();
    self.by_txid.insert(r.txid.clone(), n);
    self.by_account.entry(r.account.clone()).or_default().push(n);
    if r.author != r.account {
      self.by_account.entry(r.author.clone()).or_default().push(n);
    }
    if r.valid {
      if let Some(id) = &r.inscription_id {
        self.by_inscription.insert(id.clone(), n);
        if RecordType::parse(&r.kind).is_some_and(RecordType::is_post_like)
          && let Some(t) = dproto::post_target(id)
        {
          self.prefix.entry(t).or_default().push(n);
        }
      }
      if let Some(t) = &r.target {
        self.pointing.entry(t.clone()).or_default().push(n);
      }
      match (r.kind.as_str(), r.target.as_deref()) {
        ("keylink", Some(key)) => {
          self.links.insert(key.to_string(), Link { account: r.author.clone(), ops: r.ops.clone(), from_height: r.height, revoked_at: None });
          self.keys_of.entry(r.author.clone()).or_default().push(key.to_string());
        }
        ("keyrevoke", Some(key)) => {
          if let Some(l) = self.links.get_mut(key) {
            l.revoked_at = Some(r.height);
          }
        }
        _ => {}
      }
    }
    self.records.push(r);
  }

  fn account_for(&self, author: &str, op: &str, height: u64) -> String {
    match self.links.get(author) {
      Some(l) if l.from_height <= height && l.revoked_at.is_none_or(|r| height < r) && l.ops.iter().any(|o| o == op) => l.account.clone(),
      _ => author.to_string(),
    }
  }

  pub fn by_inscription(&self, id: &str) -> Option<&DfRecord> {
    self.by_inscription.get(id).map(|&i| &self.records[i])
  }

  pub fn by_txid(&self, txid: &str) -> Option<&DfRecord> {
    self.by_txid.get(txid).map(|&i| &self.records[i])
  }

  pub fn pointing_at(&self, target: &str) -> Vec<&DfRecord> {
    self.pointing.get(target).map(|v| v.iter().map(|&i| &self.records[i]).collect()).unwrap_or_default()
  }

  pub fn by_account(&self, address: &str) -> Vec<&DfRecord> {
    self.by_account.get(address).map(|v| v.iter().map(|&i| &self.records[i]).collect()).unwrap_or_default()
  }

  fn post_candidates(&self, target: &[u8; 11]) -> Vec<PostCandidate> {
    self
      .prefix
      .get(target)
      .map(|v| {
        v.iter()
          .filter_map(|&i| Some(PostCandidate { id: self.records[i].inscription_id.clone()?, author: self.records[i].author.clone() }))
          .collect()
      })
      .unwrap_or_default()
  }

  /// The record an inscription holds, if it is a D:// one.
  #[allow(clippy::too_many_arguments)]
  pub fn inscription(
    &self,
    height: u64,
    tx_index: u32,
    time: u64,
    txid: &str,
    outputs: &[TxOut],
    id: &str,
    content_type: &str,
    body: &[u8],
    script: &[u8],
  ) -> Option<DfRecord> {
    if !content_type.to_ascii_lowercase().starts_with("application/json") {
      return None;
    }
    let (d, split) = dproto::read_inscription(body, Some(script));
    let base = DfRecord { txid: txid.into(), height, tx_index, time, inscription_id: Some(id.into()), split_body: split, ..Default::default() };
    match d {
      Detect::NotDproto | Detect::UnknownVersion => None,
      Detect::Invalid(reason) => {
        let v: Option<serde_json::Value> = serde_json::from_slice(body).ok();
        let author = v.as_ref().and_then(|v| v.pointer("/aip/addr")?.as_str().map(str::to_string)).unwrap_or_default();
        let kind = v
          .as_ref()
          .and_then(|v| v.pointer("/map/type")?.as_str().map(str::to_string))
          .filter(|k| RecordType::parse(k).is_some())
          .unwrap_or_else(|| "unknown".into());
        Some(DfRecord { kind, account: author.clone(), author, reason: Some(reason), ..base })
      }
      Detect::Record(rec) => Some(self.check_inscription(base, *rec, outputs, content_type)),
    }
  }

  fn check_inscription(&self, base: DfRecord, rec: Record, outputs: &[TxOut], content_type: &str) -> DfRecord {
    let mut r = DfRecord {
      kind: rec.rtype.name().into(),
      author: rec.addr.clone(),
      account: self.account_for(&rec.addr, rec.rtype.name(), base.height),
      target: rec.reference.clone().or_else(|| rec.keylink.as_ref().map(|k| k.key.clone())),
      text: rec.text.clone(),
      media: rec.media.clone(),
      tags: rec.tags.clone(),
      mentions: rec.mentions.clone(),
      ts: Some(rec.ts),
      body: dproto::jcs(&rec.value).ok(),
      label: rec.keylink.as_ref().map(|k| k.label.clone()),
      ops: rec.keylink.as_ref().map(|k| k.ops.clone()).unwrap_or_default(),
      registrar: dproto::attestation_key(&rec).map(str::to_string),
      fee_koinu: Some(dproto::paid_to(outputs, dproto::FEE_ADDRESS)),
      ..base
    };
    if matches!(rec.rtype, RecordType::Comment | RecordType::Quote)
      && let Some(parent) = rec.reference.as_deref().and_then(|p| self.by_inscription(p))
    {
      r.recipient_koinu = Some(dproto::paid_to(outputs, &parent.author));
      r.recipient = Some(parent.author.clone());
    }
    let check = || -> Result<(), String> {
      if content_type.split(';').next().unwrap_or("").trim() != "application/json" {
        return Err(format!("content type is {content_type:?}, not application/json"));
      }
      let mut parent_author = None;
      if let Some(reference) = &rec.reference {
        let parent = self.by_inscription(reference).ok_or("what it points at is not a valid earlier D:// record")?;
        let post_like = RecordType::parse(&parent.kind).is_some_and(RecordType::is_post_like);
        if rec.rtype == RecordType::Edit && !(post_like && parent.account == r.account) {
          return Err("only the original author can edit it".into());
        }
        if !post_like {
          return Err(format!("it points at a {}, not a post", parent.kind));
        }
        parent_author = Some(parent.author.clone());
      }
      if let Some(k) = &rec.keylink {
        if dproto::address_hash(&k.key).is_none() {
          return Err("map.key is not a Dogecoin address".into());
        }
        if let Some(l) = self.links.get(&k.key)
          && l.revoked_at.is_none()
          && l.account != rec.addr
        {
          return Err(format!("that key is already linked to {}", l.account));
        }
      }
      dproto::verify_inscription(&rec, outputs, &[rec.addr.as_str(), r.account.as_str()], parent_author.as_deref())
    };
    match check() {
      Ok(()) => r.valid = true,
      Err(e) => r.reason = Some(e),
    }
    r
  }

  /// The record a payment holds, if it carries a D:// OP_RETURN.
  #[allow(clippy::too_many_arguments)]
  pub fn payment(
    &self,
    height: u64,
    tx_index: u32,
    time: u64,
    txid: &str,
    inputs: &[Outpoint],
    addrs: &[Option<String>],
    outputs: &[TxOut],
  ) -> Option<DfRecord> {
    let ptx = PaymentTx { inputs, input_addresses: addrs, outputs };
    let verdict = dproto::verify_payment_by_key(&ptx, |t| self.post_candidates(t), |a| self.keys_of.get(a).cloned().unwrap_or_default());
    let payer = addrs.first().cloned().flatten().unwrap_or_default();
    let mut r = DfRecord { txid: txid.into(), height, tx_index, time, author: payer.clone(), account: payer, ..Default::default() };
    match verdict {
      Ok(None) => return None,
      Err(e) => {
        r.kind = outputs
          .iter()
          .find_map(|o| dproto::op_return_payload(&o.script))
          .and_then(|p| dproto::decode_payload(&p))
          .map(|p| p.op.name().to_string())
          .unwrap_or_else(|| "unknown".into());
        r.reason = Some(e);
      }
      Ok(Some((p, kid))) => {
        r.registrar = Some(kid.to_string());
        r.kind = p.op.name().into();
        r.account = if p.op == Op::Keyrevoke { p.author.clone() } else { self.account_for(&p.author, p.op.name(), height) };
        r.author = p.author.clone();
        r.target = Some(match &p.target {
          Target::Post(id) | Target::Address(id) => id.clone(),
        });
        r.recipient = p.recipient.and_then(|(v, _)| outputs.get(v as usize)).and_then(|o| dproto::script_address(&o.script));
        r.recipient_koinu = p.recipient.map(|(_, k)| k);
        r.fee_koinu = Some(p.fee.1);
        r.valid = true;
        match (p.op, &p.target) {
          (Op::Delete, Target::Post(id)) if self.by_inscription(id).is_none_or(|post| post.account != r.account) => {
            r.valid = false;
            r.reason = Some("only the original author can delete it".into());
          }
          (Op::Keyrevoke, Target::Address(key)) if self.links.get(key).is_none_or(|l| l.account != r.author || l.revoked_at.is_some()) => {
            r.valid = false;
            r.reason = Some("it revokes no live key of this account".into());
          }
          _ => {}
        }
      }
    }
    Some(r)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_linked_key_acts_for_its_account_only_for_its_ops() {
    let mut ix = Index::default();
    ix.add(DfRecord {
      txid: "a".into(),
      height: 5,
      kind: "keylink".into(),
      author: "DMAIN".into(),
      account: "DMAIN".into(),
      target: Some("DKEY".into()),
      ops: vec!["like".into()],
      valid: true,
      ..Default::default()
    });
    assert_eq!(ix.account_for("DKEY", "like", 5), "DMAIN");
    assert_eq!(ix.account_for("DKEY", "like", 4), "DKEY");
    assert_eq!(ix.account_for("DKEY", "post", 6), "DKEY");
    ix.add(DfRecord {
      txid: "b".into(),
      height: 9,
      kind: "keyrevoke".into(),
      author: "DMAIN".into(),
      account: "DMAIN".into(),
      target: Some("DKEY".into()),
      valid: true,
      ..Default::default()
    });
    assert_eq!(ix.account_for("DKEY", "like", 8), "DMAIN");
    assert_eq!(ix.account_for("DKEY", "like", 9), "DKEY", "revoked from its block on");
    assert_eq!(ix.by_account("DMAIN").len(), 2);
  }
}
