# Deploying the contract set to Ethereum and Robinhood Chain

Runbook for adding chain 1 (Ethereum mainnet) and chain 4663 (Robinhood Chain) to
the relay, bringing them up to the same uniform contract set that Base, Polygon
and Rise have carried since 2026-07-17.

Two repos are involved. Contracts and funding come from `account`; the
relay config and Railway deploy come from `relay`.

## What is already true

Verified on 2026-09-12, so recheck anything that looks stale before you rely on it.

| | Ethereum (1) | Robinhood (4663) |
| --- | --- | --- |
| Deployer `0x3824…25A7` balance | 0.082126 ETH, nonce 2 | 0.039531 ETH, nonce 0 |
| Safe Singleton Factory | present | present |
| LayerZero endpoint | present | present |
| Contract set | none deployed | none deployed |
| Deploy cost, 12.5M gas | ~0.0010 ETH at 0.079 gwei | ~0.0012 ETH at 0.095 gwei |

Both chains are funded with ample headroom. Gas is not a constraint.

`src/` has not changed since 2026-07-12 (`9c80b99`), which predates the July
deploy, so the same salt still reproduces the same addresses. That is the single
assumption the whole plan rests on, and step 3 proves it rather than trusting it.

The addresses every chain must land on:

```
orchestrator      0x6A9Aa49c955BF766b44979A9e375Bd83e2a8f485
ithaca_account    0xa172f2a1d80859aB66c67289b82C7b084dd4fFC8
account_proxy     0x9cEC2E86E7Cb43cB56350d8f928DFC194727E096   (relay: delegation_proxy)
simulator         0x3006de101E96e85272d5B5Ad07A9738fa7678008
simple_funder     0xFC4Cd2DCfF0F1e2781BF9fE7F48378843E176A2e   (relay: funder)
escrow            0xCd075ceb5Cd463a9233a8085fc915767139F655c
```

Salt, identical on every chain so far:
`0xdf08a9fa957a9ac004f84f3a8ab7318a7be55d59948f46497bd3717865295b61`

## Settled

**Topology: the full five-chain mesh.** Every chain lists the other four, so the
three live chain blocks get edited and `ConfigureLayerZeroSettler` re-runs
against them. A partial mesh would leave routes that silently do not exist, and
that failure shows up as a stuck message rather than an error at config time.

**SimpleFunder: funded after the deploy, 0.02 ETH per chain.** Funding last is
what makes the number safe to choose, because by then the deploy and signer costs
are actual rather than estimated. Each chain pays from its own deployer balance,
so check both at step 7 and send less on whichever would drop under 0.005 ETH.
Neither chain is in `sponsorship.sponsored_chains`, so users pay their own fees
and the funder only needs a working float plus a non-zero balance to satisfy the
boot check.

## Watch the Ethereum gas price before step 4

These figures are measured, not estimated. The July deploy left its receipts in
`account/broadcast/multi/`, and the same seven contracts at the same salt
cost the same gas on Ethereum — the schedule does not differ. Per chain:

| step | gas | value |
| --- | --- | --- |
| 4, DeployMain | 12,513,992 | — |
| 5, ConfigureLayerZeroSettler | 427,350 | — |
| 6, FundSigners | 203,324 | 0.003 ETH |
| 7, FundSimpleFunder | 21,055 | chosen |
| **total** | **~13.2M** | 0.023 ETH |

Polygon came in 13,440 gas higher on the deploy, so treat 12.6M as the ceiling.
Step 5 is quoted from the most expensive of the eleven July runs, and ours
configures four destinations rather than two, so allow it 1M and the total 14M.

Ignore the dry run's own "17,757,087" — forge pads its estimate by roughly a
third. The receipts are what actually happened.

The deployer holds a separate balance on each chain, and each pays for its own
work. As of 2026-09-12:

| chain | deployer balance | nonce | needs | spare |
| --- | --- | --- | --- | --- |
| Ethereum (1) | 0.082126 ETH | 2 | ~0.0237 ETH | 0.058 |
| Robinhood (4663) | 0.039531 ETH | 0 | ~0.0243 ETH | 0.015 |

Robinhood has the thinner margin of the two even though it is the cheap chain,
because the 0.023 ETH of transfers dominates and its balance is half Ethereum's.
It runs out of room at about 1.2 gwei against its current 0.094.

Against Ethereum's 0.082126 ETH, the deploy alone:

| gas price | deploy cost | share of balance |
| --- | --- | --- |
| 0.1 gwei | 0.0013 ETH | 2% |
| 1 gwei | 0.0126 ETH | 15% |
| 2 gwei | 0.0252 ETH | 31% |
| 3 gwei | 0.0378 ETH | 46% |
| 5 gwei | 0.0630 ETH | 77% |
| 10 gwei | 0.1260 ETH | **over balance** |

Ethereum owes 0.023 ETH of transfers on top of that gas, leaving 0.059 ETH to
spend on gas, which runs out at about 4.3 gwei.

A 3.4-hour sample of 1025 blocks on 2026-09-12 puts the median base fee at 0.085
gwei but the maximum at 6.0 — past that exhaustion point. It was above 1 gwei in
18.5% of blocks and above 4.2 in 3.3%. So the risk is not a slow climb, it is a
spike landing between your check and your seventh transaction.

**Hold if the gas price is above 2 gwei.** That spends 0.028 ETH on gas and
leaves roughly 0.031 ETH of margin for a retry or a revised estimate. Above it,
wait for a quieter moment or top the deployer up first. This is the easiest
mistake in the runbook to walk into, because the simulation succeeds regardless
of price.

## Do not use `execute_config.sh`

It looks like the obvious entry point and it is a trap here. Step 3 of that
script **generates a random salt**, and it hardcodes Base Sepolia and OP Sepolia.
Running it would deploy this set to fresh addresses and break the uniformity the
relay depends on. Use the individual forge scripts below.

## How each step runs

One step at a time, four parts each, and the step is not finished until the
fourth:

1. **Simulate.** The command without `--broadcast`.
2. **Read the output.** Not "did it error" — what transactions would be sent, to
   which addresses, carrying what value. A script can succeed while doing the
   wrong thing, and that is precisely what the simulation is for.
3. **Broadcast.** The same command with `--broadcast`, only once the output says
   what it should.
4. **Verify on-chain.** Confirm the result independently before starting the next
   step. Each step names its check.

A simulation is evidence about one step, not clearance for the rest of the plan.

This ordering is a hard requirement, not a discipline. `forge script` simulates
against a fork of **current** chain state, so a step that depends on a contract
deployed by an earlier step cannot be simulated until that earlier step has
actually broadcast. On a fresh chain, SimpleFunder and LayerZeroSettler do not
exist until step 4 lands, so simulating steps 5, 6 or 7 beforehand tells you
nothing useful — at best it reverts, at worst it succeeds against state that will
not be there. Front-loading the simulations is therefore not an option.

For the same reason, run one chain at a time wherever a step touches something
already live. A `--multi` run that fails partway leaves some chains changed and
others not, and then the next simulation is against a state you have to reason
about rather than one you chose.

The final gate is the same shape at a larger scale: the relay runs read-only boot
diagnostics against every configured chain, so it can be pointed at the new
chains locally, before Railway, and refuses to start if anything is wrong. That
rehearsal is step 10.

## Step 1 — Environment

From `account`. Secrets live in Infisical, project `onramp`
(`034e61e3-e676-46e5-9e83-30d8bed8187f`), env `prod`, path `/relay`. Pull them
into variables; never echo them.

```bash
IPROJ="034e61e3-e676-46e5-9e83-30d8bed8187f"
iget() { infisical secrets get "$1" --projectId "$IPROJ" --env=prod --path=/relay --plain; }

export PRIVATE_KEY=$(iget DEPLOYER_KEY)
export GAS_SIGNER_MNEMONIC=$(iget RELAY_MNEMONIC)
export L0_SETTLER_OWNER_PK=$(iget RELAY_SETTLER_OWNER_KEY)
export RPC_1=$(iget RPC_URL_ETHEREUM)
export RPC_4663=$(iget RPC_URL_ROBINHOOD)
export RPC_8453=$(iget RPC_URL_BASE)
export RPC_137=$(iget RPC_URL_POLYGON)
export RPC_4153=$(iget RPC_URL_RISE)
export RPC_84532=https://sepolia.base.org
export RPC_11155931=https://testnet.riselabs.xyz
```

**All eight are needed even to touch one chain.** The config loader resolves
`${RPC_*}` for every block in the file before any chain is selected, so a run
targeting `[1,4663]` still dies on a missing `RPC_84532`. The two testnet values
are never dialled by these steps; they only have to exist.

`verify_config.sh` reads a `.env` file rather than the environment, so it needs
the same eight written to `account/.env`. That path is gitignored.
Delete it when the deploy is done.

`RPC_URL_ETHEREUM` and `RPC_URL_ROBINHOOD` were added to Infisical `/relay` prod
on 2026-09-12 and both answer `eth_chainId` correctly. No new credential was
needed: one QuickNode endpoint serves all four chains, same endpoint name and
same token path, differing only in the network segment of the hostname:

```
https://<name>.base-mainnet.quiknode.pro/<token>/       chainId 8453
https://<name>.matic.quiknode.pro/<token>/              chainId 137
https://<name>.ethereum-mainnet.quiknode.pro/<token>/   chainId 1
https://<name>.robinhood-mainnet.quiknode.pro/<token>/  chainId 4663
```

All four were confirmed by `eth_chainId` on 2026-09-12. Ethereum also answers
with no network segment at all (`https://<name>.quiknode.pro/<token>/`); prefer
the explicit `ethereum-mainnet` form so every entry reads the same way.

The public endpoints in `relay.yaml` are fallbacks only, and
`ethereum-rpc.publicnode.com` was unreachable during this check, so the keyed
URLs are what should be used everywhere including Railway at step 8.

These four keys were derived on 2026-09-12 and each matches the address the
config expects, so the mapping is confirmed rather than assumed. They are baked
into constructor arguments, so re-check if anything has been rotated since:

| Infisical secret | config field | address |
| --- | --- | --- |
| `RELAY_FUNDER_OWNER_KEY` | `funder_owner` | `0x6249F20C11C1cF2162A848b4bFA6FC8972d6D8d9` |
| `RELAY_FUNDER_SIGNER_KEY` | `funder_signer` | `0x06f516be4cB5D9C2A70FAEfBA83cf72fF39d68d3` |
| `RELAY_SETTLER_OWNER_KEY` | `settler_owner`, `l0_settler_owner` | `0x9bFc4c0004e99422Cd5262843dE1A5C63673dDfe` |
| `RELAY_SETTLER_SIGNER_KEY` | `l0_settler_signer` | `0x08b0f99f1ca1057C6F54cbADaC3B2dD615cb0255` |

`ConfigureLayerZeroSettler` must be run by the settler owner, which is why
`L0_SETTLER_OWNER_PK` above is `RELAY_SETTLER_OWNER_KEY` and not the deployer.

Three identities sign in this runbook, and each needs its own gas on a chain
where it has never transacted. Two of them start empty:

| key | signs for | starting balance on a new chain |
| --- | --- | --- |
| deployer `0x3824…25A7` | steps 4 and 7 | funded ahead of time |
| settler owner `0x9bFc…dDfe` | step 5 | **zero, fund it** |
| funder owner `0x6249…D8d9` | step 6 | **zero, fund it** |

Each step says how much to send. Discovering this mid-run is what leaves a step
half-finished.

## Step 2 — Add both chains to `deploy/config.toml`

**Already done**, on branch `feat/ethereum-robinhood-chains` in `account`,
and the section below is the record of what was written rather than work left to
do. Read it to check the values, then move to step 3.

Copy the `[8453]` block twice and change only what differs. Keep the salt, the
owner addresses, `supported_orchestrators`, `target_balance` and
`default_num_signers = 3` exactly as Base has them.

For `[1]`: `chain_id = 1`, `name = "Ethereum"`, `is_testnet = false`,
`endpoint_url = "${RPC_1}"`.

For `[4663]`: `chain_id = 4663`, `name = "Robinhood"`, `is_testnet = false`,
`endpoint_url = "${RPC_4663}"`.

LayerZero values, read from `metadata.layerzero-api.com/v1/metadata` on
2026-09-12 and each confirmed to hold live bytecode on its chain. The same fetch
re-confirmed the three existing entries (Base 30184, Polygon 30109, Rise 30401),
so the metadata agrees with what is already in the config.

| field | Ethereum (1) | Robinhood (4663) |
| --- | --- | --- |
| `layerzero_eid` | 30101 | 30416 |
| `layerzero_endpoint` | `0x1a44076050125825900e736c501f859c50fe728c` | `0x6f475642a6e85809b1c36fa62763669b1b48dd5b` |
| `layerzero_send_uln302` | `0xbb2ea70c9e858123480642cf96acbcce1372dce1` | `0xc39161c743d0307eb9bcc9fef03eeb9dc4802de7` |
| `layerzero_receive_uln302` | `0xc02ab410f0734efa3f14628780e6e695156024c2` | `0xe1844c5d63a9543023008d332bd3d2e6f1fe1043` |
| `dvn_layerzero_labs` | `0x589dedbd617e0cbcb916a9223f4d1300c294236b` | `0xd01ae6905d48315f7be10c7330aecf8360ef5b12` |
| `dvn_nethermind` | `0xa59ba433ac34d2927232918ef5b2eaafcf130ba5` | `0x0ffe02df012299a370d5dd69298a5826eacafdf8` |
| `dvn_horizen` | `0x380275805876ff19055ea900cdb2b46a94ecf20d` | `0x1258a278519c7f4bd997a9c3bfd4aa802a028d89` |

Robinhood shares its endpoint and ULN addresses with Rise, which is what
deterministic deployment looks like and not a copy-paste error. Its DVNs are its
own, so those cannot be lifted from the Rise block.

### Validators

Keep the policy the three live mainnets already use:

```toml
layerzero_required_dvns = ["dvn_layerzero_labs"]
layerzero_optional_dvns = ["dvn_horizen", "dvn_nethermind"]
layerzero_optional_dvn_threshold = 1
```

That is **two independent validators per message** — LayerZero Labs always, plus
whichever of Horizen or Nethermind answers first. All three were confirmed
non-deprecated and live on all five chains, which is what makes a full mesh
possible with one uniform policy.

Two required DVNs would also give two validators, but strictly worse: both would
have to be live for any message to pass, so either one stalling halts the route.
The one-required-plus-one-of-two shape keeps two independent attestations while
tolerating a single DVN outage. Raising the threshold to 2 means all three must
verify — more expensive per message and a third liveness dependency. Reserve that
for Ethereum only if you decide L1 value at risk justifies it, and know it is a
deliberate break from the other four chains.

### Confirmations

Block times measured directly on 2026-09-12, because the LayerZero metadata
reports 366s for Robinhood, which is wrong — it is a 100ms chain, exactly as
`relay.yaml` says.

| chain | block time | confirmations | wall clock |
| --- | --- | --- | --- |
| Ethereum (1) | 12.06s | **15** | ~181s |
| Robinhood (4663) | 0.100s | **50** | ~5s |
| Base, existing | 1.8s | 10 | ~18s |
| Polygon, existing | 2.0s | 120 | ~240s |
| Rise, existing | 1.00s | 5 | ~5s |

Fifteen is the conventional figure for Ethereum L1 and lands in the same range as
Polygon's deliberately conservative setting. Fifty on Robinhood matches Rise's
five-second posture, which is the closest analogue — a fast L2. Copying Rise's
literal `5` would mean half a second of finality, which is not a meaningful wait.

Contracts list for both, matching Base and Rise exactly:

```toml
contracts = [
    "Orchestrator",
    "IthacaAccount",
    "AccountProxy",
    "Simulator",
    "SimpleFunder",
    "Escrow",
    "LayerZeroSettler",
]
```

Destination lists for the full mesh, by chain id rather than eid:

| block | `layerzero_destination_chain_ids` |
| --- | --- |
| `[1]` new | `[8453, 137, 4153, 4663]` |
| `[4663]` new | `[1, 8453, 137, 4153]` |
| `[8453]` edit | add `1, 4663` |
| `[137]` edit | add `1, 4663` |
| `[4153]` edit | add `1, 4663` |

Leave the `*_deployed` fields out of the new blocks. The scripts write them on
broadcast. LayerZeroSettler should land at
`0xF8f849C9Ad809f3A27d08D05A9fe71b4F978C9Bd`, the address Base and Rise share.

Then check the config reads back cleanly before any script runs:

```bash
./deploy/verify_config.sh 1 4663
```

It checks that the required environment variables and RPC URLs are present and
that addresses match what is on-chain. Run it for the three existing chains too
after editing their destination lists, since those edits are easy to get wrong in
a way nothing else notices until a message goes missing.

Both runs passed on 2026-09-12. Two of its complaints are script bugs rather than
real problems, and they show up on the live chains as well, so do not chase them:

- `Endpoint mismatch: Expected 0x1a44…fe728c, got 0x1a44…fE728c`. Same address,
  compared as a string without lowercasing.
- Polygon signers reported "below target" at 85 POL against a 0.5 POL target.
  Bash integer arithmetic overflows above 2^63, and 85e18 wei does not fit.

Everything else it reported for chains 1 and 4663 was an expected absence: no
contracts, no settler, no funder, zero signer balances.

## Step 3 — Dry run, and check the addresses match

This is the gate. No `--broadcast`, so nothing is sent and nothing is written.

```bash
forge script deploy/DeployMain.s.sol:DeployMain \
  --sig "run(uint256[])" "[1,4663]"
```

Read the predicted addresses out of the output and compare them against the six
in the table above, plus LayerZeroSettler at
`0xF8f849C9Ad809f3A27d08D05A9fe71b4F978C9Bd`. **If even one differs, stop.** A
mismatch means the bytecode or the salt has drifted, and deploying anyway would
leave these two chains on a different contract set from the other three, which is
exactly the thing the uniform-address invariant exists to prevent.

**Run on 2026-09-12: 7/7 matched on both chains.** The salt reproduces the live
set, which is what makes step 4 safe to broadcast. Re-run this if `src/` or the
config changes, because that result is what it is certifying. Disregard its
17,757,087 gas estimate; the gas-price ceiling above is built on the July
receipts instead.

## Step 4 — Deploy

One chain at a time, so a failure on one does not leave the other half-done.
Ethereum first, since it is the one worth watching.

Check the gas price in the same breath as the broadcast, and do not send if it
prints above 2:

```bash
cast to-unit $(cast gas-price --rpc-url $RPC_1) gwei

forge script deploy/DeployMain.s.sol:DeployMain \
  --broadcast --slow --gas-estimate-multiplier 300 \
  --private-key $PRIVATE_KEY --sig "run(uint256[])" "[1]"

forge script deploy/DeployMain.s.sol:DeployMain \
  --broadcast --slow --gas-estimate-multiplier 300 \
  --private-key $PRIVATE_KEY --sig "run(uint256[])" "[4663]"
```

**`--private-key` is not optional.** Every script here broadcasts through a bare
`vm.broadcast()`, so forge takes the sender from the flag and refuses to guess:
without it the run dies on `You seem to be using Foundry's default sender`. The
step-3 dry run does not need it, which is exactly why the omission survives into
step 4 unnoticed.

No `--verify`: the `[etherscan]` block in `foundry.toml` is entirely commented
out and no verification key exists, so Base, Polygon and Rise were all deployed
unverified. These two match that.

### Do not trust `config.toml` as proof

The script writes the seven `*_deployed` fields while it is collecting
transactions, before any of them land. A run that fails at the sender check
still leaves them behind, naming contracts that do not exist. It also silently
strips the file's header comments, because the TOML writer does not preserve
them.

So after a failed run, reset the file before retrying:

```bash
git checkout -- deploy/config.toml
```

**Verify before moving on.** Ask the chain, not the config:

```bash
for a in 0x6A9Aa49c955BF766b44979A9e375Bd83e2a8f485 \
         0xa172f2a1d80859aB66c67289b82C7b084dd4fFC8 \
         0x9cEC2E86E7Cb43cB56350d8f928DFC194727E096 \
         0x3006de101E96e85272d5B5Ad07A9738fa7678008 \
         0xFC4Cd2DCfF0F1e2781BF9fE7F48378843E176A2e \
         0xCd075ceb5Cd463a9233a8085fc915767139F655c \
         0xF8f849C9Ad809f3A27d08D05A9fe71b4F978C9Bd; do
  printf '%s %s\n' "$a" "$(cast code $a --rpc-url $RPC_1 | head -c 12)"
done
```

Seven non-empty results mean it landed. Only then commit the config change as
the deployment record. The deployer's nonce moving by seven is the corroborating
signal; an unchanged nonce and balance means nothing was sent at all.

## Step 5 — Configure LayerZero

Run by the settler owner, not the deployer. This step was executed on 2026-09-12
and the notes below are what it actually took, corrected from the plan that
preceded it.

### It cannot be split into new chains then live chains

The obvious sequencing — new chains first, live chains after — does not work.
`configureChain` reads `chainConfigs[destChainId]` to get each destination's eid,
and only chains passed on the command line get loaded. Passing `[1,4663]` leaves
Base, Polygon and Rise unloaded, their eids read as zero, and the run dies with
`LZ_ULN_UnsupportedEid(0)`. The two entry points are `run()` and
`run(uint256[])`, and in both the set you pass is simultaneously the load set and
the configure set, so there is no way to load a chain without configuring it.

One five-chain run is the only available shape, and it is what July used.

### Fund the settler owner first

`0x9bFc4c0004e99422Cd5262843dE1A5C63673dDfe` pays for this step, not the
deployer, and it starts empty on any new chain. Base, Polygon and Rise were
funded back in July; Ethereum and Robinhood were not, and a run that reaches them
with a zero balance leaves the mesh half-built.

```bash
cast send 0x9bFc4c0004e99422Cd5262843dE1A5C63673dDfe \
  --value 0.01ether --rpc-url $RPC_1 --private-key $PRIVATE_KEY
cast send 0x9bFc4c0004e99422Cd5262843dE1A5C63673dDfe \
  --value 0.005ether --rpc-url $RPC_4663 --private-key $PRIVATE_KEY
```

Thirteen transactions per new chain at roughly 142k gas each is about 2M gas, so
0.01 ETH covers Ethereum up to about 5 gwei. Both amounts are generous on
purpose: running out mid-run is far more expensive than over-funding.

### DVN lists must be sorted ascending by address

ULN302 rejects an unsorted list with `LZ_ULN_Unsorted()`. Four of the five chains
happen to be ascending when horizen is listed before nethermind, which is why the
existing config reads uniformly. Robinhood is the exception and needs the reverse
order. Check any chain added later rather than copying a neighbour's block.

### Run it

```bash
forge script deploy/ConfigureLayerZeroSettler.s.sol:ConfigureLayerZeroSettler \
  --multi --slow --gas-estimate-multiplier 300 \
  --private-key $L0_SETTLER_OWNER_PK \
  --sig "run(uint256[])" "[1,4663,8453,137,4153]"

forge script deploy/ConfigureLayerZeroSettler.s.sol:ConfigureLayerZeroSettler \
  --broadcast --multi --slow --gas-estimate-multiplier 300 \
  --private-key $L0_SETTLER_OWNER_PK \
  --sig "run(uint256[])" "[1,4663,8453,137,4153]"
```

`--gas-estimate-multiplier 300` is not optional. The first attempt ran without it
and died 45 transactions in, when a Polygon `setConfig` reverted having burned
87,388 of its 88,088 gas limit. Forge's estimate was simply too tight, and
Polygon was at roughly 280 gwei at the time.

62 transactions: 12 per chain, being 4 pathways at 3 transactions each — executor
config and ULN config on the send library, ULN config on the receive library —
plus one `setEndpoint` on each of the two new settlers.

### What to check in the simulation

The earlier version of this document said to stop if the run touched any pathway
other than the two new eids. That rule is wrong and would have blocked a correct
run. The script rewrites every pathway on every chain it configures, including
the six that already existed, so 18 of the 62 transactions are rewrites of live
routes.

What matters is not whether it touches them but whether it would change them.
Decode each proposed `setConfig` and compare it against `getConfig` on-chain:
identical bytes mean the rewrite is a no-op costing only gas. **Stop only if a
live pathway's bytes would actually change.** All six were byte-identical here.

Beware decoding the payload by hand. A first pass at this reported all six as
differing, which was an ABI offset bug rather than a real finding. The nested
`bytes` member sits at an offset relative to its tuple start, not the array
start.

### Recovery from a partial run

Re-running is the correct fix and the script is idempotent: configurations that
already match are rewritten with identical bytes. That holds because a reverted
transaction writes nothing, so a failed run leaves missing writes rather than
wrong values. Verify first, then re-run the whole five-chain command.

**Verify before moving on.** Two independent checks, both of which must be clean:

1. Every `setConfig` payload the dry run proposed, compared against `getConfig`
   on-chain. 60 of them across the five chains.
2. `endpoint()` on each settler, and `peers(eid)` for all four destinations on
   each chain, which must equal the settler address left-padded to bytes32. 5
   endpoints and 20 peers.

A pathway is one-directional and configured on each side separately, so both ends
must land before a route works at all.

## Step 6 — Register and fund the signers

Derives three signers from the mnemonic, tops them up to `target_balance`,
registers them as gas wallets in SimpleFunder, and sets the supported
orchestrators. The relay refuses to boot if a signer is not a registered gas
wallet, so this is not optional.

Two things about the invocation matter more than the step itself, and both were
wrong in the first draft of this document.

**Run it as the funder owner, not the deployer.** SimpleFunder's `owner()` is
`0x6249F20C11C1cF2162A848b4bFA6FC8972d6D8d9`, and `setGasWallet` and
`setOrchestrators` are owner-only. Running as the deployer reverts with
`Unauthorized()` after the three transfers have already been simulated, which
reads as a permissions problem rather than a wrong-key problem. Like the settler
owner, this address starts empty on a new chain and pays for the whole step:

```bash
cast send 0x6249F20C11C1cF2162A848b4bFA6FC8972d6D8d9 \
  --value 0.01ether --rpc-url $RPC_1 --private-key $PRIVATE_KEY
cast send 0x6249F20C11C1cF2162A848b4bFA6FC8972d6D8d9 \
  --value 0.005ether --rpc-url $RPC_4663 --private-key $PRIVATE_KEY
```

**Pass the signer count explicitly.** `run(uint256[])` hardcodes `numSigners = 10`
at line 103 and never reads `default_num_signers` from the config, so the
two-argument form funds ten signers and grants all ten gas-wallet authority on a
production SimpleFunder. Base has exactly three. Use the three-argument form:

```bash
export FUNDER_OWNER_PK=$(iget RELAY_FUNDER_OWNER_KEY)

forge script deploy/FundSigners.s.sol:FundSigners \
  --multi --slow --private-key $FUNDER_OWNER_PK \
  --sig "run(uint256[],uint256)" "[1,4663]" 3

forge script deploy/FundSigners.s.sol:FundSigners \
  --broadcast --multi --slow --gas-estimate-multiplier 300 \
  --private-key $FUNDER_OWNER_PK \
  --sig "run(uint256[],uint256)" "[1,4663]" 3
```

Five transactions per chain and 3 × 0.001 ETH, matching the July receipts exactly.
The simulation must name these three and no others, the same addresses already
registered as gas wallets in SimpleFunder on Base:

```
0x281B006E1B77ddbd31965C74053A4371Ebc04BC9
0x3Ec680844c8A0505891227dF004cE9D4369ac90D
0xFb8DBB7D5614bE237b8bB537C8D5677f0D031ECe
```

A wrong `GAS_SIGNER_MNEMONIC` produces a script that succeeds while funding and
authorising strangers.

**Verify before moving on.** For each of the three signers on each chain, read
`SimpleFunder` back and confirm the signer is registered as a gas wallet, and
that its balance is at or above `target_balance`. Check `orchestrators()` too.
These are the exact conditions the relay refuses to boot without, so checking
them here turns a confusing step-10 failure into an obvious step-6 one.

## Step 7 — Fund SimpleFunder

0.02 ETH per chain as decided above, but check each balance before committing to
it. The two chains are funded independently, so check the one you are about to
send on:

```bash
cast balance 0x3824525F5913F1908ce62ea5532C9757Bf3E25A7 --rpc-url $RPC_1 --ether
cast balance 0x3824525F5913F1908ce62ea5532C9757Bf3E25A7 --rpc-url $RPC_4663 --ether
```

Ethereum started at 0.082126 and should still hold about 0.058 here, so 0.02 is
comfortable unless step 4 hit a spike. Robinhood started at 0.039531 and should
hold about 0.015 after its own 0.02 goes out — the thinner of the two, and the
one to check rather than assume. **If sending 0.02 would leave either chain under
0.005 ETH, send less on that chain.** An underfunded funder is a top-up later; an
empty deployer is a stuck runbook.

For context, July funded Base with 0.0459 ETH and Rise with 0.0536 ETH. We are
deliberately below that precedent, because neither new chain is sponsored and the
deployer is paying L1 prices this time.

Native token, so the zero address, and `20000000000000000` wei each. One chain at
a time, same as step 4:

```bash
forge script deploy/FundSimpleFunder.s.sol:FundSimpleFunder \
  --slow --gas-estimate-multiplier 300 --private-key $PRIVATE_KEY \
  --sig "run(address,(uint256,address,uint256)[])" \
  0xFC4Cd2DCfF0F1e2781BF9fE7F48378843E176A2e \
  "[(1,0x0000000000000000000000000000000000000000,20000000000000000)]"

forge script deploy/FundSimpleFunder.s.sol:FundSimpleFunder \
  --slow --gas-estimate-multiplier 300 --private-key $PRIVATE_KEY \
  --sig "run(address,(uint256,address,uint256)[])" \
  0xFC4Cd2DCfF0F1e2781BF9fE7F48378843E176A2e \
  "[(4663,0x0000000000000000000000000000000000000000,20000000000000000)]"

# each again with --broadcast once its simulated amount reads correctly
```

Check the simulated value transfers digit by digit. This is the only step that
moves a chosen amount rather than a computed one, the amounts are in wei, and an
extra zero on Ethereum is a real loss with no undo. Confirm the destination is
the SimpleFunder address above and not the funder owner.

The relay refuses to boot if the funder holds no native token, so a zero here
fails later in a confusing place.

**Verify before moving on.** `eth_getBalance` on
`0xFC4Cd2DCfF0F1e2781BF9fE7F48378843E176A2e` on both chains, and check the figure
is the one you intended rather than merely non-zero.

## Step 8 — Point Railway at the chains

Set `RPC_1` and `RPC_4663` on the Railway relay service to the QuickNode URLs,
the same way `RPC_8453` gets `RPC_URL_BASE`. Without these the relay falls back
to the public endpoints written in `relay.yaml`, which is not what you want in
production and, for Ethereum, may not even respond.

Use `--skip-deploys`. Setting a variable normally triggers a redeploy, which
would restart the live relay on an image that does not yet have the new chain
blocks — a pointless production restart. The variables sit unused until step 11
ships the image that reads them.

```bash
railway variables --service relay --environment production --skip-deploys \
  --set "RPC_1=$(iget RPC_URL_ETHEREUM)"
railway variables --service relay --environment production --skip-deploys \
  --set "RPC_4663=$(iget RPC_URL_ROBINHOOD)"
```

Check the names landed with `railway variables --service relay --kv`, and filter
the output rather than reading it whole: the table prints raw values, so an
unfiltered `--kv` puts every secret on your screen.

## Step 9 — Uncomment the chain blocks

In `relay/deploy/railway/relay.yaml`, uncomment the `1:` and `4663:`
blocks. They are already written with the right settings and should not need
editing:

- Ethereum: `sim_mode: simulatev1`, no L1 fee, no sync send, 3 signers.
- Robinhood: `sim_mode: simulatev1`, `send_raw_transaction_sync: true`,
  `min_watch_window_ms: 2000`, and `l1_fee: arbitrum` set explicitly because
  alloy-chains has no entry for 4663 and `auto` would quote no L1 fee at all.

Do not script this with a blanket comment-stripper. The 4663 block has a prose
comment inside it, above `global-dollar`, and a naive pass stops there and
silently leaves the rest of the block commented — including `signers`, whose
absence YAML accepts without complaint. Uncomment it, then parse the file and
assert every chain has the keys you expect:

```bash
python3 -c "
import yaml
d=yaml.safe_load(open('deploy/railway/relay.yaml'))
for c,b in d['chains'].items(): print(c, b.get('signers'), list(b['assets']))
"
```

Neither chain goes into `sponsorship.sponsored_chains` yet. Users pay their own
fees in the listed fee tokens until a `chain_sponsorship` entry is sized.

## Step 10 — Rehearse the relay locally

The last gate before production and the one that catches the most, but it is not
the harmless read-only exercise an earlier draft of this document claimed. Read
the warning below before running it.

### The rehearsal sends real transactions

The diagnostics are read-only. **The relay is not.** Nothing stops at the
diagnostics report: the boot continues straight into the transaction service,
which manages signers, pulls from SimpleFunder to top them up, and closes nonce
gaps. On mainnet. With production keys.

Running this on 2026-09-12 moved real money within about four minutes:

```
SimpleFunder Robinhood   0.020000 -> 0.003242    (-0.0168)
each of three signers    0.001    -> ~0.0066     (nonce 0 -> 1)
```

Nothing was lost — the funder exists to fund gas wallets and it did exactly that
— but it was not intended, and Ethereum escaped the same treatment only because
the process was killed first. Six `ERROR` lines about `nonce too low: tx: 0
state: 1` were the relay's own retries racing its own first transactions.

`--config-only` does not mean read-only. It controls where configuration comes
from, nothing else.

### It also collides with production

Railway runs on the same `RELAY_MNEMONIC`, so a local boot drives the same three
signer addresses as the live relay on every chain both have configured. That is
two processes assigning nonces to one account. The 2026-09-12 run overlapped a
live production relay for four minutes and got away with it because production
was idle and the affected chain was one production does not carry. Do not rely on
that twice.

### How to run it safely

Kill the process the moment the diagnostics report prints. Everything worth
learning is in the report; everything after it is the relay going to work.

```bash
RUST_LOG=info ./target/release/relay --config deploy/railway/relay.yaml --config-only \
  2>&1 | tee rehearsal.log | grep -m1 "Diagnostics completed"
# then kill it immediately: the pipeline above does not stop the relay
```

Two details that make the run work at all. Omit `RELAY_DB_URL` and the relay
falls back to in-memory storage (`src/spawn.rs:130`), so no database is needed
and production's is never touched. Build first rather than using `cargo run`,
because a stale `target/release/relay` will happily rehearse code that is months
old.

### What the report must say

A clean report means every check passed; the relay refuses to start otherwise.
Between them `src/diagnostics/chain.rs` and `src/diagnostics/layerzero.rs`
verify:

- every signer is a registered gas wallet, and the funder holds native token;
- each contract's EIP-712 domain matches what the relay expects;
- assets resolve and their decimals match;
- LayerZero configuration per pair, when interop is configured.

Two warnings are expected and not failures while interop is absent from
`relay.yaml`:

```
Chain connectivity: 0 connections found: []
No configuration for interop found, but more than one endpoint was configured
```

The on-chain mesh from step 5 exists regardless; the relay simply does not use it
until an interop block is added. Base, Polygon and Rise have run this way since
July.

Never pass `--skip-diagnostics`. It exists for a known-degraded chain, and using
it here skips the only thing between a misconfiguration and production.

## Step 11 — Deploy the relay and verify from outside

```bash
railway up --service relay --environment production --ci
```

`--ci` is what makes it work unattended; without it the command wants a terminal.
The build is a full `cargo build --profile maxperf`, so expect to wait. The same
diagnostics run on Railway, so a clean start is again the pass condition, and a
refusal names what failed.

Then confirm independently of the relay's own opinion.

**Ask the live relay what it serves.** The quickest end-to-end proof that the new
config actually shipped:

```bash
curl -s -X POST https://id.oportet.xyz \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"wallet_getCapabilities","params":[]}'
```

Every configured chain id should appear, 1 and 4663 among them.

**Check the contracts, but do not expect identical bytecode.** An earlier draft
said to compare `eth_getCode` against Base and treat identical code as proof.
That is wrong. Orchestrator, IthacaAccount, SimpleFunder and LayerZeroSettler
cache chain-specific EIP-712 domain separators in immutables, which the
constructor writes after the CREATE2 address is already fixed, so those four
differ on every chain — Base, Polygon and Rise differ from each other too. The
other three are byte-identical everywhere. Identical bytecode on all seven would
mean replay protection was broken. Check that code is present at all seven
addresses, and compare hashes across the existing chains before calling any
difference suspicious.

**Check the DVNs by address.** On each new chain, read back the send and receive
configs for the four destination eids and confirm each names LayerZero Labs as
required and both Horizen and Nethermind as optional with a threshold of 1.
Diagnostics do not cover this: `layerzero.rs` checks that DVN counts are
consistent, that no address is zero and that at least one DVN exists, but it
never compares the addresses against the ones chosen here. A pathway that
silently fell back to LayerZero's default DVNs passes every automated check and
still delivers messages, just with validators nobody picked.

**Expect two warnings** until interop is configured in `relay.yaml`:
`Chain connectivity: 0 connections found` and `No configuration for interop
found`. The on-chain mesh exists regardless; the relay does not use it yet.

## If it goes wrong

Contracts at CREATE2 addresses cannot be removed. Recovery from a bad deploy
means changing the salt and re-deploying the whole set everywhere, which is a far
larger job than getting step 3 right. That is why step 3 is a hard stop.

Everything before step 4 is free to abandon: config edits are local, and the dry
run sends nothing. After step 4, re-comment the `relay.yaml` blocks to take the
chains back out of the relay without touching what is on-chain.
