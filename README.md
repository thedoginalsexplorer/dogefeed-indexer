# dogefeed-indexer

A standalone indexer and JSON API for **D://**, DogeFeed's protocol on Dogecoin. It reads a Dogecoin Core node block by block, checks every D:// record against the v1 standard, and serves what it finds as JSON.

The same checks run in [The Doginals Explorer](https://explorer.doginals.store/dfeed) (DogeOrd). They live in the `dproto` crate in this repo.

## What D:// is

| Record | On chain |
|---|---|
| post, comment, quote, edit, keylink | A Doginal inscription of signed JSON (`application/json`, at most 4096 bytes) |
| like, branch (repost), follow, tip, reveal, delete, keyrevoke | A payment with one 80-byte OP_RETURN (`6a 4c 50` + `D://` + op + target + registrar signature) |

Unfollows, DMs and DM payments are off chain, so they are not here. An on-chain follow does not show that someone still follows.

## What is checked

For an inscription:
- The JSON follows the v1 field rules.
- The author signed it: a Dogecoin signed message over `"D://:v1:" + JCS(record without att, aip cut to alg and addr)`, using RFC 8785 canonical JSON.
- The DogeFeed registrar countersigned that signature (`att`, by `kid`).
- It was first sent to its author.
- Its reveal pays DogeFeed at least 0.1 DOGE.
- A comment or quote also pays the author it answers at least 0.01 DOGE. That author is the original author, not whoever holds the inscription now.

For a payment:
- It has exactly one OP_RETURN.
- Every input spends from one address.
- It pays DogeFeed at least 0.1 DOGE, and pays the recipient at least 0.01 DOGE.
- For a tip or reveal, the payout splits 90/10.
- The registrar's raw low-S ECDSA signature verifies over the single SHA-256 of the exact digest: op, full target, inputs, recipient output and fee output.
- A post target is tried against every earlier post that matches its 10-byte txid prefix and index, in chain order, and the first one whose digest verifies wins.

It also applies the protocol's rules about who may do what:
- Deletes and edits count only from the original author.
- A CLI key acts for the account that linked it, only for the ops it was linked for, and only from the block of the link until the block of its revocation.

Records are ordered by block and position, never by their own `ts`. They count at 6 confirmations; shallower ones are listed as `pending`. Invalid records are kept, with the reason.

The registrar key `dfreg1` is pinned from the standard: public key `0393d51f…3eebd`, address `D5n1bGXmDEkvgz6fDX1vHHtWYBwGrk5u2Q`. DogeFeed is paid at `D73nYZFPhhCx2JMqBqifF3BXH46Xbvk9v6`.

### Records split over several pushes

DogeFeed's builder writes a record's JSON as several data pushes within one Doginal piece: `ord`, `1`, the content type, `0`, then the pushes. Standard Doginals parsing, including ord-dogecoin, keeps only the first push, which is not valid JSON on its own. So a record is read from the standard body joined with the data pushes that follow it in the reveal. The pushes are taken up to the spend's own signature and redeem script, which are the last two pushes. This is done only when the result reads as a D:// record. Such records show `"splitBody": true`.

### Known difference from ord-dogecoin

This indexer reads every reveal it sees. ord-dogecoin skips a reveal whose first input already carries an inscription at offset 0. This indexer does not track inscription locations, so it cannot apply that rule. DogeOrd does apply it.

## Run

You need a Dogecoin Core node with `server=1` and `txindex=1`, and Rust 1.85 or newer.

```bash
cargo build --release
DFEED_DOGECOIN_CONF=~/.dogecoin/dogecoin.conf ./target/release/dogefeed-indexer
```

| Setting | Default | Meaning |
|---|---|---|
| `DFEED_RPC_URL` | `http://127.0.0.1:22555` | Node RPC |
| `DFEED_RPC_USER`, `DFEED_RPC_PASSWORD` | — | RPC login |
| `DFEED_DOGECOIN_CONF` | — | Or read `rpcuser`/`rpcpassword` from this dogecoin.conf |
| `DFEED_START` | `6440000` | First block read. The first D:// v1 record is at 6,448,293. |
| `DFEED_DATA_DIR` | `./dfeed-data` | Where `state.json` is kept |
| `DFEED_LISTEN` | `127.0.0.1:8795` | API address, or `off` |

Everything is kept in memory and saved to `state.json` about once a minute, so a restart carries on where it left off. Reorgs up to 100 blocks deep are taken back.

## API

The same shape as `https://explorer.doginals.store/v1/dfeed`:

- `GET /v1/dfeed`: progress, totals, the fee address and the registrar keys.
- `GET /v1/dfeed/feed?before=&limit=`: valid posts and quotes, newest first, each with confirmed likes, branches, comments, quotes and tips.
- `GET /v1/dfeed/activity?kind=&valid=valid|invalid|all&before=&limit=`: every record, newest first.
- `GET /v1/dfeed/post/:inscriptionId`: a post, comment or quote with its replies and interactions.
- `GET /v1/dfeed/user/:address`: posts, followers, who it follows, linked keys and records.
- `GET /v1/dfeed/tx/:txid`: the verdict on one transaction.

`before` is a record's `cursor` (`height:position`). Amounts are integer koinu (1 DOGE = 100,000,000).

Each record carries:
- `author`: who signed it (`aip.addr`, or the address a payment spends from).
- `registrar` and `signedByDogeFeed`: the DogeFeed registrar key whose signature on it verifies (`dfreg1`), which shows it was issued through DogeFeed. For an inscription this is the `att` attestation. For a payment it is the signature in its OP_RETURN.
- `feeKoinu`: what it pays DogeFeed.
- `recipient` and `recipientKoinu`: who it pays and how much. For a comment or quote this is what its reveal pays the original author of the post it answers (0.86 DOGE in DogeFeed's test records).

This indexer has no names index. The Doginals Explorer adds a `names` map to each response: the `.dogefeed` or `.doge` name to show for each address.

`state.json` from an older version is set aside as `state.v<N>.json`, and D:// is read again from `DFEED_START`.

## Tests

```bash
cargo test --workspace
```

The tests in `crates/dproto/tests` check all seven confirmed mainnet records of the standard (post, comment, quote, follow, like, branch and tip), read from the chain bytes in `mainnet-vectors.json`. They also check mutations of those records that must be refused: a changed text, a borrowed attestation, the wrong destination, a short fee, a flipped signature bit, a changed payout, mixed inputs, a second OP_RETURN, an unknown target and a broken 90/10 split.

## License

MIT
