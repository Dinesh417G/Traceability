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
# 1. Postgres 16 with a NON-superuser role (superusers bypass RLS)
createdb trace_dev
psql -c "CREATE ROLE trace_app LOGIN PASSWORD 'trace_dev_pw' NOSUPERUSER;"

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

| Crate | Purpose |
|---|---|
| `trace-core` | Domain types, route state machine, gates. **No I/O**, heavily unit tested |
| `trace-store` | Postgres repositories, migrations, RLS, hash chain |
| `trace-devices` | `DeviceDriver` trait, drivers, virtual device simulators |
| `trace-mark` | Label templates, binding engine, native ZPL II renderer |
| `trace-billing` | Stripe control plane → signed entitlements |
| `trace-license` | Ed25519 entitlement verification, node-locked, grace period |
| `trace-updater` | OTA: signed manifests, atomic apply, automatic rollback |
| `trace-sync` | Outbox drain and cloud transport (cloud phase) |
| `trace-edge` | Axum service, one per plant, serves `/t/{ulid}` |
| `trace-station` | Headless station agent: offline spool |
| `route-sim` / `device-sim` | Dry-run routes and fake hardware, no plant required |

## Design rules

Read [`CLAUDE.md`](CLAUDE.md) before contributing. The short version:
configuration over code, records are immutable, and **nothing in the runtime path
blocks on a remote call** — including licensing, billing and OTA.

- [`DECISIONS.md`](DECISIONS.md) — architecture decisions and open questions
- [`MERGE-NOTES.md`](MERGE-NOTES.md) — how this merges into ElectronIx MES later
- [`HARDWARE-PROFILE.md`](HARDWARE-PROFILE.md) — validated hardware spec
- [`docs/LASER-DPM-DEFERRED.md`](docs/LASER-DPM-DEFERRED.md) — laser marking design, deferred
