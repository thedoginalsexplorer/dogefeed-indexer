//! A Dogecoin Core node over JSON-RPC.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub struct Rpc {
  client: reqwest::Client,
  url: String,
  auth: Option<(String, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Block {
  pub hash: String,
  pub previousblockhash: Option<String>,
  pub height: u64,
  #[serde(default)]
  pub time: u64,
  pub tx: Vec<Tx>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tx {
  pub txid: String,
  pub vin: Vec<Vin>,
  pub vout: Vec<Vout>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vin {
  pub txid: Option<String>,
  pub vout: Option<u32>,
  #[serde(rename = "scriptSig")]
  pub script_sig: Option<Script>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Script {
  pub hex: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vout {
  pub value: f64,
  #[serde(rename = "scriptPubKey")]
  pub script_pub_key: Script,
}

impl Vout {
  pub fn koinu(&self) -> u64 {
    (self.value * 100_000_000.0).round() as u64
  }
  pub fn script(&self) -> Vec<u8> {
    hex::decode(&self.script_pub_key.hex).unwrap_or_default()
  }
}

impl Tx {
  pub fn script_sig(&self) -> Option<Vec<u8>> {
    self.vin.first()?.script_sig.as_ref().and_then(|s| hex::decode(&s.hex).ok())
  }
  pub fn outputs(&self) -> Vec<dproto::TxOut> {
    self.vout.iter().map(|o| dproto::TxOut { koinu: o.koinu(), script: o.script() }).collect()
  }
}

#[derive(Deserialize)]
struct Response<T> {
  result: Option<T>,
  error: Option<Value>,
}

impl Rpc {
  /// `url` like http://127.0.0.1:22555; the login from the arguments, or
  /// from a dogecoin.conf.
  pub fn new(url: String, user: Option<String>, password: Option<String>, conf: Option<String>) -> Result<Self> {
    let mut auth = user.zip(password);
    if auth.is_none()
      && let Some(path) = conf
    {
      let text = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
      let val = |k: &str| {
        text.lines().find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.trim_start().strip_prefix('=')).map(|v| v.trim().to_string()))
      };
      auth = val("rpcuser").zip(val("rpcpassword"));
    }
    Ok(Self { client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build()?, url, auth })
  }

  pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
    let mut req = self.client.post(&self.url).json(&json!({ "jsonrpc": "1.0", "id": "dfeed", "method": method, "params": params }));
    if let Some((u, p)) = &self.auth {
      req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await.with_context(|| format!("{method}: node unreachable"))?;
    let status = resp.status();
    let body: Response<T> = resp.json().await.with_context(|| format!("{method}: HTTP {status}"))?;
    if let Some(e) = body.error.filter(|e| !e.is_null()) {
      bail!("{method}: {e}");
    }
    body.result.with_context(|| format!("{method}: empty result"))
  }

  pub async fn block_count(&self) -> Result<u64> {
    self.call("getblockcount", json!([])).await
  }

  pub async fn block(&self, height: u64) -> Result<Block> {
    let hash: String = self.call("getblockhash", json!([height])).await?;
    self.call("getblock", json!([hash, 2])).await
  }

  pub async fn tx(&self, txid: &str) -> Result<Tx> {
    self.call("getrawtransaction", json!([txid, true])).await
  }
}
