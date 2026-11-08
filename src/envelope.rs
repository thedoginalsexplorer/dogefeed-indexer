//! Doginal inscriptions, as ord-dogecoin reads them: `"ord"`, the number of
//! pieces, the content type, then (countdown, data) pairs, possibly over
//! several transactions, each continuing the one whose output its first
//! input spends.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::rpc::Tx;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chain {
  pub start: String,
  pub scripts: Vec<String>,
  /// Block it was last continued in.
  pub height: u64,
}

pub enum Outcome {
  None,
  Partial,
  Complete { content_type: String, body: Vec<u8> },
}

fn number(d: &[u8]) -> Option<u64> {
  if d.len() > 8 {
    return None;
  }
  let mut b = [0u8; 8];
  b[..d.len()].copy_from_slice(d);
  Some(u64::from_le_bytes(b))
}

pub fn parse(scripts: &[Vec<u8>]) -> Outcome {
  let Some(mut p) = scripts.first().and_then(|s| dproto::script_pushes(s)) else { return Outcome::None };
  if p.len() < 3 || p[0] != b"ord" {
    return Outcome::None;
  }
  let Some(mut left) = number(&p[1]).filter(|n| *n > 0) else { return Outcome::None };
  let content_type = String::from_utf8(p[2].clone()).unwrap_or_default();
  let (mut body, mut si, mut idx) = (Vec::new(), 0usize, 3usize);
  loop {
    while left > 0 && idx + 1 < p.len() {
      match number(&p[idx]) {
        Some(n) if n == left - 1 => {
          body.extend_from_slice(&p[idx + 1]);
          left -= 1;
          idx += 2;
        }
        _ => break,
      }
    }
    if left == 0 {
      return Outcome::Complete { content_type, body };
    }
    si += 1;
    let Some(s) = scripts.get(si) else { return Outcome::Partial };
    let Some(next) = dproto::script_pushes(s) else { return Outcome::None };
    if next.len() < 2 || number(&next[0]) != Some(left - 1) {
      return Outcome::None;
    }
    p = next;
    idx = 0;
  }
}

/// A completed inscription: its id, content type, standard body and the
/// scriptSig of the transaction that completed it.
pub struct Completed {
  pub id: String,
  pub content_type: String,
  pub body: Vec<u8>,
  pub script: Vec<u8>,
}

/// Reveal chains in progress, by the txid that a continuation must spend.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Chains {
  pub partial: HashMap<String, Chain>,
}

impl Chains {
  /// Reads one transaction. A spend that does not continue a chain leaves the
  /// chain where it is (as ord-dogecoin does).
  pub fn push(&mut self, tx: &Tx, height: u64) -> Option<Completed> {
    let script = tx.script_sig()?;
    let prev = tx.vin.first().and_then(|v| v.txid.clone());
    let taken = prev.as_deref().and_then(|p| self.partial.remove(p));
    let continues = taken.is_some();
    let mut chain = taken.unwrap_or_else(|| Chain { start: tx.txid.clone(), scripts: Vec::new(), height });
    chain.scripts.push(hex::encode(&script));
    let raw: Vec<Vec<u8>> = chain.scripts.iter().filter_map(|s| hex::decode(s).ok()).collect();
    match parse(&raw) {
      Outcome::Complete { content_type, body } => Some(Completed { id: format!("{}i0", chain.start), content_type, body, script }),
      Outcome::Partial => {
        chain.height = height;
        self.partial.insert(tx.txid.clone(), chain);
        None
      }
      Outcome::None => {
        if continues {
          chain.scripts.pop();
          self.partial.insert(prev.unwrap(), chain);
        }
        None
      }
    }
  }

  /// Drops chains nobody continued for a long while, to bound memory.
  pub fn prune(&mut self, height: u64, keep_blocks: u64) {
    self.partial.retain(|_, c| c.height + keep_blocks >= height);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn push(d: &[u8]) -> Vec<u8> {
    let mut v = vec![d.len() as u8];
    v.extend_from_slice(d);
    v
  }

  #[test]
  fn a_two_piece_inscription_completes_in_its_second_transaction() {
    let mut first = push(b"ord");
    first.push(0x52); // 2 pieces
    first.extend(push(b"text/plain"));
    first.push(0x51); // piece 1
    first.extend(push(b"hello "));
    let mut second = vec![0x00]; // piece 0
    second.extend(push(b"world"));
    match parse(&[first.clone()]) {
      Outcome::Partial => {}
      _ => panic!("partial"),
    }
    match parse(&[first, second]) {
      Outcome::Complete { content_type, body } => assert_eq!((content_type.as_str(), body.as_slice()), ("text/plain", b"hello world".as_slice())),
      _ => panic!("complete"),
    }
  }
}
