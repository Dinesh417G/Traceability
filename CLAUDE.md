# ElectronIx Trace — Engineering Charter

Product traceability platform for Indian MSME / mid-size manufacturers.
Built by ElectronIx (Coimbatore, India). Backend is **Rust**.

## Rules of engagement (binding on all contributors, human and AI)

1. **Everything is configuration-driven.** If you are about to hardcode a process
   step, a test limit, a label field, a station name, a machine protocol, or a
   product family — stop. It belongs in the database.
2. **Traceability records are immutable.** Never `UPDATE` or `DELETE` a captured
   measurement or a genealogy link. Corrections are new rows that supersede old
   ones, carrying reason and operator.
3. **Offline-first.** A factory station must keep working with the network, the
   server and the internet all down. Nothing in the runtime path may block on a
   remote call. This explicitly includes licensing, billing and OTA.
4. **Trace owns its own Postgres database**, in a dedicated `trace` schema, never
   `public`, so a future merge into ElectronIx MES is a schema attach.
5. **Multi-tenancy from migration 001.** `tenant_id` on every table with Row Level
   Security. Not retrofitted later.

## Architecture

```
crates/
  trace-core/     domain types, route state machine, gates. NO I/O. Heavily unit tested.
  trace-store/    sqlx repositories, migrations, RLS policies (schema: trace)
  trace-devices/  DeviceDriver trait + drivers + simulators
  trace-mark/     template model, ZPL renderer, binding engine, print queue
  trace-billing/  Stripe: subscriptions, checkout, webhooks -> entitlements
  trace-license/  Ed25519 signed entitlements, node-locked, grace period
  trace-updater/  OTA: signed manifests, delta download, atomic apply, rollback
  trace-sync/     outbox drain + transport (cloud phase)
apps/
  trace-edge/     Axum service, one per plant. Owns Postgres, devices, /t/{ulid}.
  trace-station/  headless station agent: local SQLite spool, offline operation
  trace-cloud/    DEFERRED — stub only
tools/
  route-sim/      dry-run a route with no hardware
  device-sim/     virtual PLC / serial device / virtual printer
```

`trace-core` must never gain an I/O dependency. That is the property that makes
the route engine testable and the reason gates are pure functions.

## Layering rule

```
trace-core  <- depends on nothing but std/serde
trace-store <- trace-core
trace-*     <- trace-core (+ store where genuinely needed)
apps/*      <- everything
```
No crate may depend on an app. No cycles.

## Identity

- Internal keys are `BIGINT` surrogates. **Never expose a sequential id.**
- Anything printed, marked or put in a URL is a **ULID** (26-char Crockford
  base32, lexicographically sortable by time).
- A scrapped UID is retired forever and never reissued.

## Audit

`unit_event` and `measurement` are append-only with a per-unit hash chain
(`prev_hash`, `row_hash`) so tampering is detectable. This is what makes the
trace defensible in an IATF 16949 or customer audit. Every raw device payload is
stored verbatim next to the parsed value.

## Deferred by explicit instruction

- **Laser direct part marking (DPM)** is deferred. The design is preserved in
  `docs/LASER-DPM-DEFERRED.md` and the `MarkerDriver` seam exists in
  `trace-mark` so the future implementation is additive. Do not delete the seam.
- **Cloud / multi-tenant activation** is Phase 6. The schema and outbox exist now.

## Commands

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                    # unit tests, no DB needed
TRACE_TEST_DATABASE_URL=postgres://... cargo test --workspace -- --include-ignored   # + integration
```
