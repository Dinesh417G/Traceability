# ElectronIx Trace

Product traceability for Indian MSME and mid-size manufacturers.
Rust backend, PostgreSQL, offline-first, on-premise.

A unit of product is born, assembled, tested, labelled and shipped. At every step
Trace captures *what went into it*, *what was done to it*, *by whom*, *on which
machine*, and *with what measured result*. Scanning the code on the product
returns the complete, auditable history of that unit.

## The two queries that matter

- **Backward trace** (unit → history): scan a returned unit, get its birth
  certificate — job card, BOM revision, every component serial/lot consumed,
  every torque value, every test reading, every operator, every rework.
- **Forward trace** (component/lot → units): given a bad supplier lot or a machine
  that drifted out of calibration between 14:00 and 16:00, list every finished
  unit affected. **This is the recall query, and it is the most valuable feature
  in the product.** The schema is indexed so it is a fast lookup, not a scan.

## Quick start

```bash
# 1. Postgres 16 with a NON-superuser role that OWNS the database.
#    Non-superuser because superusers bypass RLS, which would make the
#    cross-tenant isolation tests silently pass for the wrong reason.
#    Owner because on PG15+ a non-owner cannot create tables in the public
#    schema, where sqlx keeps its migration ledger.
psql -c "CREATE ROLE trace_app LOGIN PASSWORD 'trace_dev_pw' NOSUPERUSER;"
psql -c "CREATE DATABASE trace_dev OWNER trace_app;"

# 2. Configure
cp .env.example .env && $EDITOR .env

# 3. Build and test
cargo test --workspace                                   # unit tests, no DB
TRACE_TEST_DATABASE_URL=postgres://trace_app:trace_dev_pw@127.0.0.1/trace_dev \
  cargo test --workspace -- --include-ignored            # + integration

# 4. Run the edge service
cargo run -p trace-edge
```

## Workspace

| Crate | Purpose | Tests |
|---|---|---|
| `trace-core` | Domain types, route state machine, gates. **No I/O** | 88 |
| `trace-sign` | Canonical JSON + Ed25519, shared by licensing and OTA | 9 |
| `trace-store` | Postgres repositories, migrations, RLS, hash chain | 13 |
| `trace-devices` | `DeviceDriver` trait, drivers, virtual device simulators | 34 |
| `trace-mark` | Label templates, binding engine, native ZPL II renderer | 36 |
| `trace-billing` | Stripe control plane → signed entitlements | 29 |
| `trace-license` | Ed25519 entitlement verification, node-locked, grace period | 16 |
| `trace-updater` | OTA: signed manifests, atomic apply, automatic rollback | 36 |
| `trace-sync` | Outbox drain and cloud transport (cloud phase) | — |
| `trace-edge` | Axum service, one per plant, serves `/t/{ulid}` | 20 |
| `trace-station` | Headless station agent: durable offline spool | 8 |
| `route-sim` / `device-sim` | Dry-run routes and fake hardware, no plant required | 12 |

**301 tests**, clippy clean. Integration tests run against a real PostgreSQL
with the real migrations and a **non-superuser** role, because superusers bypass
RLS and would make the isolation tests silently vacuous.

## Try it without a plant

```bash
# Push a unit through a four-operation route: gates, interlock, the lot.
cargo run -p route-sim -- --verbose

# See what a failing route looks like
cargo run -p route-sim -- --example > scenario.json
# edit scenario.json, then:
cargo run -p route-sim -- --scenario scenario.json

# A virtual torque wrench that fails every 5th part
cargo run -p device-sim -- instrument --port 4001 --bad-every 5

# A virtual Zebra that prints the ZPL it receives
cargo run -p device-sim -- printer --port 9100
```

## Payments and updates

Two features that would normally fight offline-first, and how they were
reconciled:

- **Stripe** runs in a control plane that the factory box never talks to. What
  reaches the plant is an Ed25519-signed entitlement, verified locally in
  microseconds. A lapsed subscription restricts configuration changes and
  **never stops production** — halting capture would destroy the traceability
  record for units physically on the line, and those units then cannot ship at
  all. `Enforcement` has no `Stopped` variant, deliberately.
- **OTA** verifies the manifest signature *before downloading anything*, checks
  every artifact's SHA-256 against that signed manifest, swaps an atomic
  symlink, and rolls back automatically if the new version fails a health probe.
  Downgrades are refused unless explicitly marked as a rollback, because a
  manifest signed last year is still perfectly signed. The offline USB path runs
  the identical verification code.

## Design rules

Read [`CLAUDE.md`](CLAUDE.md) before contributing. The short version:
configuration over code, records are immutable, and **nothing in the runtime path
blocks on a remote call** — including licensing, billing and OTA.

- [`DECISIONS.md`](DECISIONS.md) — architecture decisions and open questions
- [`MERGE-NOTES.md`](MERGE-NOTES.md) — how this merges into ElectronIx MES later
- [`HARDWARE-PROFILE.md`](HARDWARE-PROFILE.md) — validated hardware spec
- [`docs/LASER-DPM-DEFERRED.md`](docs/LASER-DPM-DEFERRED.md) — laser marking design, deferred
