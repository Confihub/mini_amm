# mini_amm

`mini_amm` is the reference constant-product AMM used by the ConfiBatch/confi.cash
Soroban stack. It follows Uniswap V2/Soroswap-style pricing, charges a 0.30% swap
fee through the pricing formula, and provides the push-input swap interface used
by `route_adapter`.

This repository is the standalone build of the latest contract implementation.
It targets Soroban SDK `25.3.2`, Rust `1.91`, and Apache-2.0.

> Status: testnet/reference contract. It is useful for integration, controlled
> liquidity, and end-to-end protocol validation. Treat a production deployment as a
> separate security and operational decision.

## Where it fits

```text
confibatch_pool
    │ pushes the net input
    ▼
route_adapter
    │ pushes input + calls swap
    ▼
mini_amm
    │ transfers output
    └──────────────────────► confibatch_pool
```

Only the batch's net imbalance reaches this public AMM. Individual private order
amounts remain inside the ConfiBatch proof/batch flow.

## Pricing

For input reserve `x`, output reserve `y`, and real input `dx`, the contract uses:

```text
dx_with_fee = dx × 997
amount_out  = (y × dx_with_fee) / (x × 1000 + dx_with_fee)
```

All arithmetic that can overflow is checked. Integer division rounds down.

The `amount_in` argument passed to `swap` is deliberately not trusted. The
contract calculates the real input from:

```text
current token-in balance - stored token-in reserve
```

The input must therefore be transferred to the AMM before `swap` is invoked.
Calling `swap` without first pushing tokens returns `NoInput`; a caller cannot
obtain a free output by lying about `amount_in`.

## Contract interface

| Function | Authorization | Description |
| --- | --- | --- |
| `__constructor(admin, token_a, token_b)` | deployment | Atomically initializes a fresh deployment. |
| `init(admin, token_a, token_b)` | first call only | Legacy initialization path. Rejects reinitialization. |
| `sync()` | admin | Replaces stored reserves with the AMM's actual token balances. Intended as a PoC/operator recovery convenience. |
| `bump_ttl()` | none | Permissionless instance-storage TTL keeper. |
| `reserves()` | none | Returns `(reserve_a, reserve_b)`. |
| `get_amount_out(token_in, amount_in)` | none | Quotes the constant-product output using current stored reserves. |
| `quote_add(amount_a)` | none | Returns the ideal token-B contribution at the current reserve ratio. |
| `swap(token_in, amount_in, amount_out_min, to)` | pushed funds | Executes a push-model swap and returns the output amount. The declared `amount_in` is ignored in favor of the balance delta. |
| `add_liquidity(from, amount_a, amount_b)` | `from` | Adds liquidity without a minimum-shares floor. |
| `add_liquidity_min(from, amount_a, amount_b, min_shares)` | `from` | Adds liquidity and reverts if fewer than `min_shares` would be minted. |
| `remove_liquidity(from, shares)` | `from` | Burns shares and returns pro-rata token A and B amounts. |
| `remove_liquidity_min(from, shares, min_a, min_b)` | `from` | Removes liquidity with minimum-output protection on both legs. |
| `lp_balance(addr)` | none | Returns the address's LP-share balance. |
| `lp_supply()` | none | Returns total LP shares, including permanently locked first-mint shares. |

### Initialization rules

- `token_a` and `token_b` must be different.
- Neither token address may be the AMM's own contract address.
- Fresh deployments should use the constructor so there is no deploy-to-init
  takeover window.
- `init` remains for legacy upgrades and controlled compatibility flows.

### Swap sequence

1. Quote with `get_amount_out`.
2. Choose `amount_out_min`.
3. Transfer the input token to the AMM contract.
4. Call `swap(token_in, amount_in, amount_out_min, recipient)`.
5. The contract derives the real input, updates reserves, and transfers the
   output token to `recipient`.

The transfer and call should be composed in the same Soroban transaction or
invocation tree. Leaving tokens at the AMM without immediately calling `swap`
allows the next caller to consume the balance delta as input.

## Liquidity accounting

The first provider establishes the initial price and receives:

```text
sqrt(amount_a × amount_b) - 1,000
```

The missing `1,000` shares are included in total supply but assigned to no
owner. This permanent minimum-liquidity lock prevents the pool from returning
to zero supply and closes the classic first-depositor share-price inflation
footgun. The initial geometric mean must be greater than `1,000`.

Later providers receive the smaller of:

```text
amount_a × total_supply / reserve_a
amount_b × total_supply / reserve_b
```

Supplying assets away from the current ratio donates the excess leg to existing
LPs. Use `quote_add` and `add_liquidity_min` to control that risk.

Removal pays:

```text
shares × reserve_a / total_supply
shares × reserve_b / total_supply
```

Use `remove_liquidity_min` when the quote and execution may occur in different
ledgers.

## Storage and TTL

Instance storage contains:

- admin;
- token A and token B addresses;
- stored reserves;
- total LP supply; and
- LP balance per provider.

Mutating paths renew instance storage. `bump_ttl` is a permissionless keeper for
quiet pools. The implementation bumps to roughly 30 days when TTL falls below
roughly 20 days, assuming about 17,280 ledgers per day.

## Errors

| Code | Name | Meaning |
| ---: | --- | --- |
| 1 | `NotInitialized` | Required AMM configuration is absent. |
| 2 | `NotAdmin` | Reinitialization was attempted or admin state is unavailable. Admin authorization failures are enforced by Soroban auth. |
| 3 | `BadToken` | Token pair or `token_in` is invalid. |
| 4 | `Slippage` | A minimum output/share floor was not met. |
| 5 | `NoLiquidity` | Reserves or supply are insufficient for the operation. |
| 6 | `Overflow` | Checked `i128` arithmetic would overflow. |
| 7 | `NoInput` | No positive input was supplied, or an amount/share is invalid. |
| 8 | `InsufficientShares` | The provider tried to burn more shares than it owns. |

## Security properties and limitations

- Swap output is based on the observed token-balance delta, not caller input.
- Output, reserve, and LP arithmetic uses checked operations.
- Add/remove slippage checks happen before state writes or token movement.
- LP additions pull tokens from the authorized provider.
- The first-mint lock prevents complete supply removal.
- `sync` is admin-controlled and trusts actual contract balances; it should not
  be used as ordinary pricing flow.
- The contract has no oracle or TWAP and provides no protection against normal
  AMM price manipulation beyond caller-selected minimums.
- LP ownership is public in this contract. `confibatch_pool` can hold the public
  AMM position while representing end-user LP positions as shielded notes.

## Build

Prerequisites:

- Rust `1.91` with the `wasm32v1-none` target;
- Stellar CLI compatible with Soroban SDK 25; and
- network access for the first Cargo dependency fetch.

```bash
rustup target add wasm32v1-none
stellar contract build
```

The optimized Wasm is produced under:

```text
target/wasm32v1-none/release/mini_amm.wasm
```

The release profile enables LTO, checked overflow, symbol stripping, one codegen
unit, and abort-on-panic.

## Deployment outline

```bash
stellar contract build

stellar contract deploy \
  --wasm target/wasm32v1-none/release/mini_amm.wasm \
  --source-account <DEPLOYER> \
  --network <NETWORK> \
  -- \
  --admin <ADMIN_ADDRESS> \
  --token_a <TOKEN_A_SAC> \
  --token_b <TOKEN_B_SAC>
```

Exact CLI argument spelling can vary with the installed Stellar CLI. Inspect
the built contract specification before deployment:

```bash
stellar contract info interface \
  --wasm target/wasm32v1-none/release/mini_amm.wasm
```

After deployment, verify the token ordering, seed liquidity through the
authorized LP flow, inspect `reserves`, and configure the matching
`route_adapter`.

## Repository contents

```text
src/lib.rs       Contract and generated client
Cargo.toml       Standalone Soroban build configuration
Cargo.lock       Reproducible dependency lock
```

## Security and license

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
The source is licensed under the [Apache License 2.0](LICENSE).
