# DECISIONS

Architecture decision record. Newest section last. Every entry states the
decision, why, and what would make us revisit it.

---

## D-001 — PostgreSQL, not MySQL

**Decided.** Postgres 16+.

Row Level Security gives DB-enforced multi-tenancy (MySQL has no equivalent);
`JSONB` + GIN for raw device payloads and per-customer custom fields; native
declarative partitioning for `measurement`; recursive CTEs for the N-level
genealogy walk; identical behaviour on an edge box and in managed cloud; schema
namespacing makes the future MES merge tractable.

`trace-store` keeps repository traits so another backend is *possible*. We do not
build one speculatively.

---

## D-002 — Laser DPM deferred (user instruction, 2026-09-09)

**Deferred, not cancelled.** The instruction was: *"skip the laser printing part
as of now, keep the idea we will implement later."*

What we did instead of deleting it:

- The full engineering analysis is preserved in `docs/LASER-DPM-DEFERRED.md`,
  including the Data-Matrix-not-QR rationale, the file-drop atomic-rename
  contract, the aluminium-vs-plastic per-revision mark strategy, the mark-verify
  gate, and the vendor procurement checklist.
- `product_revision` carries `mark_method` (`LASER_DPM | LABEL | BOTH | NONE`)
  from migration 001, so enabling laser later is data, not a migration.
- `MARK_VERIFIED` exists as a gate type in `trace-core` and is fully implemented
  and tested. Deferring the *device* did not require deferring the *gate*.
- `mark_record` is written by the label path today and is laser-ready.

**Consequence to accept:** at the first site, identity at operation 1 is applied
by **printed label**, which the spec already names as a legitimate engineering
answer for parts that cannot hold a gradeable mark. Every part is therefore
label-identified in v1.

**Revisit when:** a laser is selected. Expected new work is one `MarkerDriver`
impl plus a Data Matrix renderer. If it turns out to be more than that, the
abstraction is wrong and we should be told before anyone works around it.

---

## D-003 — Stripe payments on an offline-first factory box

**Decided.** This is the highest-risk new requirement, because it puts an
internet-dependent service into a product whose first rule is that the plant
keeps running with the internet down. Naively calling Stripe from the edge box
would violate Rule 3 of the charter.

**Resolution: split the control plane from the runtime plane.**

```
   Stripe  <--- HTTPS/webhooks --->  Billing control plane (internet side)
                                            |
                                     issues Ed25519-signed
                                     Entitlement document
                                            |
                                            v
                                  Edge box (plant LAN, may be offline)
                                  verifies signature locally, caches,
                                  enforces with a grace period.
                                  NEVER calls Stripe in the runtime path.
```

Rules that fall out of this, all enforced in code:

1. **The edge box never talks to Stripe.** It only ever verifies an
   `Entitlement` signed by the vendor's Ed25519 key. Verification is local,
   offline, and takes microseconds.
2. **Payment state can only ever be advisory at the edge.** A lapsed
   subscription degrades *non-production* capability (new config, extra seats,
   cloud sync) and raises warnings. **It never stops the line.** Production
   capture is life-support: stopping it would destroy traceability records for
   units physically on the line, which is a far worse outcome than an unpaid
   invoice. `EnforcementMode::BlockProduction` deliberately does not exist.
3. **Grace period is generous and configurable** (default 30 days past expiry,
   then `Restricted`, never `Stopped`). Clock-tamper is detected by monotonic
   high-water-mark, not by trusting the RTC.
4. **Entitlement delivery is pluggable and offline-capable**: online pull when
   the box has internet, or a signed file on a USB stick for air-gapped plants —
   the same transport OTA uses.
5. **Webhook signature verification is mandatory and constant-time.** Stripe's
   `Stripe-Signature` scheme (HMAC-SHA256 over `t.payload`) with a replay window.
   An unverified webhook is dropped, never processed.

**Revisit when:** we need metered/usage billing per unit produced. That would
push usage counters through the outbox, which the schema already supports.

---

## D-004 — OTA update model

**Decided.** Signed manifest + content-addressed artifacts + atomic swap +
automatic rollback, modelled on ElectronIx DNC.

- Manifest is Ed25519-signed and names each artifact by SHA-256 digest. Signature
  is checked **before** any download, digest checked after. A digest mismatch
  aborts and is never retried against the same bytes.
- Downgrade protection: refuse a manifest whose version is below the installed
  version unless it carries an explicit `rollback: true` flag.
- **Apply is atomic**: unpack to a staging dir, fsync, swap a `current` symlink,
  never mutate the running install in place.
- **Health-gated rollback**: after swap the new version must pass a health probe
  within a deadline or the previous release is restored automatically. The
  previous release is retained until the new one is confirmed healthy.
- **Offline path**: the same signed bundle can be delivered on USB, for plants
  with no internet. Identical verification code path — no "trusted because local".
- Rollout is staged by cohort so one plant proves a release before the fleet.

---

## D-005 — Runtime-checked SQL, not the `query!` macro

**Decided.** We use `sqlx::query`/`query_as` with explicit binds rather than the
compile-time-verified `query!` macros.

Why: `query!` requires either a live `DATABASE_URL` at *compile* time or a
checked-in `.sqlx` offline cache that must be regenerated on every schema change.
On an on-prem product built by a small team that is a recurring build-breaker,
and it makes CI depend on a database to *compile* rather than to *test*.

**What we do instead**, so this is not a loss of safety: every repository method
is covered by an integration test that runs against a real Postgres with the real
migrations applied. Type errors surface in `cargo test`, one layer later than the
macro would catch them, but they cannot reach a release.

**Revisit when:** the team is large enough to keep an offline cache honest.

---

## D-006 — Station app is a headless agent in this workspace

**Decided.** `apps/trace-station` is a **headless Rust agent** (spool, offline
queue, edge reconnect), not a Tauri GUI crate.

Why: Tauri pulls `webkit2gtk`/WebView2 system libraries that do not exist in CI
containers, which would make the whole workspace unbuildable on a clean machine —
a bad trade for a shell around logic that is testable headless. The genuinely
hard part of the station is offline durability, and that is what lives here.

The Tauri 2 shell wraps this agent when terminal hardware is chosen. The touch
UI requirements (glove-friendly hit targets, badge/PIN login, numeric pads, no
hover, no right-click) are recorded in `HARDWARE-PROFILE.md`.

---

## D-007 — ULIDs stored as canonical text

**Decided.** Public identifiers are stored as `TEXT` constrained to the 26-char
Crockford base32 form, not as `uuid`/`bytea`.

Why: lexicographic order equals time order (so plain b-tree indexes give
time-ordered scans); it is directly greppable in a factory support call; and the
value in the database is byte-identical to the value printed on the part.

Cost accepted: 26 bytes vs 16. This only lands on identity columns —
`measurement` references units by `BIGINT`, so the 7M-rows/year table is
unaffected.

---

## OPEN QUESTIONS (proceeding on stated defaults; confirm when you can)

These were not answerable from the brief. Each has a default already implemented,
so nothing is blocked — but the defaults are guesses and should be confirmed.

| # | Question | Default we implemented |
|---|---|---|
| Q1 | Stripe: who is the paying customer — the factory (self-serve) or ElectronIx invoicing on their behalf? | Self-serve Checkout + Customer Portal, one Stripe customer per **tenant** |
| Q2 | Pricing shape: per plant, per station, per seat, or per unit produced? | Per-tier subscription with a **station-count** entitlement cap |
| Q3 | Tier names and what each unlocks | `Starter / Professional / Enterprise` — see `trace-license::Tier` |
| Q4 | Currency and tax: INR with GST, or USD? | INR, GST handled by Stripe Tax; currency is config |
| Q5 | Grace period length before a lapsed subscription restricts config changes | 30 days, configurable |
| Q6 | OTA: who hosts the update server, and is it reachable from plant LANs? | HTTPS artifact server, host configurable; USB path always available |
| Q7 | Signing key custody for OTA + entitlements (HSM? offline laptop?) | Keys are loaded from files/env; **generation is out of band**. Production keys must never live in the repo |
| Q8 | Is the first site's Zebra printer networked (TCP 9100) or USB? | TCP 9100 with a file/USB sink fallback |
| Q9 | Retention policy for measurements | Partitioned monthly, no automatic drop; retention is config |
| Q10 | Confirm label-at-operation-1 is acceptable for v1 given laser deferral (see D-002) | Assumed yes |

---

## D-008 — Tenant provisioning is a privileged operation

**Decided.** `trace.provision_tenant(code, name)` is the only way to create a
tenant, and it is `SECURITY DEFINER`.

This is forced by RLS, and it is the right answer rather than a workaround. The
policy on `trace.tenant` checks `id = current_tenant_id()`, but the id does not
exist until the row is inserted — so there is deliberately **no application path
that creates a tenant**. A station cannot provision one, which is exactly the
posture we want.

The function takes the identity value from the sequence first, sets
`app.tenant_id` to it transaction-locally, then inserts. Because it replaces the
caller's tenant context for the rest of the transaction, **provisioning must run
in a transaction of its own**. The function comment says so, and the test
fixture does so.

---

## D-009 — One hash chain per unit, spanning both evidence tables

**Decided.** `unit_event` and `measurement` share a single per-unit `chain_seq`
rather than keeping two parallel chains.

Why: with two chains, deleting a measurement leaves the event chain perfectly
intact, so half the evidence can be removed without detection. With one
interleaved chain, removing any row breaks everything recorded after it,
whichever table it lived in.

Appends take `SELECT ... FOR UPDATE` on the unit row, which serialises writers
for that unit and makes a duplicate `chain_seq` impossible.

### Gotcha worth knowing: `sqlx::migrate!` staleness

`sqlx::migrate!` embeds migrations at **compile** time, and adding a *new*
migration file does not reliably invalidate the macro. The symptom is a
"function does not exist" error for something you just wrote. Force a rebuild of
`trace-store` (`touch crates/trace-store/src/lib.rs`, or
`cargo clean -p trace-store`) after adding a migration.

### Note on the recall performance test

The index-usage test seeds 20,000 genealogy rows before asserting on the query
plan. That is not padding: on a small table a sequential scan genuinely is
cheaper and Postgres is right to choose one, so asserting index usage against a
50-row fixture would pass or fail for reasons unrelated to the schema. The
5M-row figure in the definition of done is a benchmark, not a CI test.

---

## D-010 — OTA ordering is the security property

**Decided.** The update sequence is fixed and the order is deliberate:

1. verify the manifest signature — **before any artifact byte is fetched**;
2. apply the version decision (downgrade protection, cohort, upgrade path);
3. fetch artifacts;
4. verify each artifact's size, then its SHA-256, against the signed manifest;
5. atomically swap the `current` symlink;
6. health-probe, and roll back automatically on failure.

Doing 1 before 3 means an attacker who can serve bytes cannot even make the box
spend bandwidth on their payload. Doing 4 before 5 means a substituted binary is
caught before it can be linked. Doing 5 by `rename` over a symlink means power
loss never leaves the box with half of each version.

**Downgrade protection matters more than it looks.** A manifest we signed last
year is still perfectly signed. Without a version rule, replaying it walks a box
backwards into a version with a known hole — a valid signature is not a fresh
one. Downgrades are refused unless the manifest is explicitly marked
`rollback: true`.

**The offline USB path uses the identical verification code.** There is no
"trusted because it came from local media" branch. A USB stick found in a car
park is not a trusted source, and a plant with no internet deserves the same
integrity guarantees as one with it.

**Rollback keeps the previous release until the new one is proven healthy**, not
until it is merely installed. If the *first* ever release fails its probe it is
left in place, because removing it would leave the box with no software at all —
the least bad option, and one worth stating rather than discovering.
