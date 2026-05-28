# Setup — IBKR Web API first-party OAuth 2.0

TheWheel authenticates to IBKR's **Web API** with **first-party OAuth 2.0**
(`private_key_jwt`). Requests go directly to `https://api.ibkr.com` and are
signed with **your RSA private key** — there is no local gateway and no
always-open socket. This is a one-time setup.

> Security: the private key is the credential. Keep `private_key.pem` readable
> only by you (`chmod 600 private_key.pem`). It and `config.toml` are gitignored.

## 1. Generate an RSA key pair

```bash
openssl genrsa -out private_key.pem 3072
openssl rsa -in private_key.pem -pubout -out public_key.pem
chmod 600 private_key.pem
```

## 2. Register for first-party OAuth

In IBKR's **Self-Service OAuth portal** (Client Portal → Settings → API, or
contact `webapionboarding@interactivebrokers.com` if it isn't enabled for your
account), register a consumer and **upload `public_key.pem`**. You'll receive:

- **`client_id`** — your consumer key
- **`kid`** — key id assigned to the uploaded public key
- **`credential`** — your IBKR username (used as the JWT `sub`)

> A freshly uploaded key can take **up to ~24h** to propagate before tokens work.

## 3. Fill in `config.toml`

```bash
cp config.toml.example config.toml
```

Set the `[connection.oauth]` block:

```toml
[connection.oauth]
client_id  = "YOUR_CONSUMER_KEY"
kid        = "YOUR_KEY_ID"
credential = "your_ibkr_username"
private_key_path = "private_key.pem"
```

Use `mode = "paper"` (default) with your paper account first.

## 4. Verify connectivity

```bash
cargo run --bin spike -- AAPL
```

This authenticates, opens a brokerage session, and prints your account summary,
positions, an option chain with greeks, and a **what-if** (never transmitted)
order preview. It also probes EU/PRIIPs tradability — try a US single-name like
`AAPL` (should be ALLOWED) and a US ETF like `SPY` (expected BLOCKED for EU retail).

## Things that may need adjusting (and how)

These bits of the Web API are sparsely documented; the spike will tell you if a
default is off. All are config-driven so you needn't touch code:

| Symptom | Fix in `config.toml` |
|---|---|
| Token request rejected (`invalid grant_type`/`aud`) | set `[connection.oauth] grant_type` / `token_url` to the exact values from your onboarding docs |
| `no put strikes returned` | the `/secdef/strikes` `month` format may be `MMMYY` not `YYYYMM` (the spike currently sends `YYYYMM`) — tell me and I'll switch it |
| Greeks show as `-` | wrong `[connection.fields]` codes, or no market-data entitlement → set `market_data` and verify field ids in IBKR's Web API field reference |

## Notes

- **Session**: tokens last ~1h; the app refreshes automatically. A brokerage
  session needs a keep-alive "tickle" every ~60s (the running app does this; the
  spike is short-lived so doesn't need to).
- **Rate limit**: ~10 requests/second per username.
- **Paper vs live**: determined by which account your OAuth consumer is linked
  to, not by a port. Keep `read_only = true` until you deliberately arm trading.
