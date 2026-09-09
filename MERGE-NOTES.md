# MERGE-NOTES — Trace ↔ ElectronIx MES

Reviewed at the end of every phase. Goal: merging Trace into MES is a week of
work, not a rewrite.

## Principle

Trace owns its own database today, entirely inside the `trace` schema. It never
writes to `public`. A merge is therefore a **schema attach + foreign key
repoint**, not a data reconciliation project.

## Entities MES also owns

| Trace entity | Natural key we use | MES owns it? | What changes on merge day |
|---|---|---|---|
| `tenant` | `code` (short slug, unique) | yes | Repoint FK to MES tenant; keep `code` as the join key |
| `plant` | (`tenant_id`, `code`) | yes | Same — join on `code`, drop Trace's copy |
| `line` | (`plant_id`, `code`) | yes | Same |
| `station` | (`line_id`, `code`) | yes | MES may model stations differently; `code` is the bridge |
| `app_user` | (`tenant_id`, `username`) | yes | Trace keeps badge/PIN auth locally for offline login; MES becomes the identity source |
| `product` | (`tenant_id`, `model_no`) | yes | Join on `model_no` |
| `product_revision` | (`product_id`, `revision`) | probably | Engineering revision may be owned by PLM, not MES |
| `job_card` | (`tenant_id`, `number`) | **yes, eventually** | This is the one to watch — see below |
| `bom` / `bom_line` | (`product_revision_id`, `revision`) | probably | May come from ERP instead |
| `route` / `route_operation` | (`product_revision_id`, `line_id`, `seq`) | no | Trace-specific; the route engine stays ours |
| `unit`, `unit_event`, `measurement`, `genealogy_link` | ULID / surrogate | no | Pure Trace. The valuable data. Never migrated away |

## Job cards — the pluggable seam

MES will eventually own work orders. So the job card **source is pluggable**:

- `trait JobCardSource` in `trace-core`, with `LocalJobCardSource` for v1.
- `MesJobCardSource` / `ErpJobCardSource` are declared and unimplemented.
- `job_card.external_ref` exists from migration 001 to hold the MES/ERP key.
- **Route engine logic must never reach for a job card source directly.** It
  receives a resolved `JobCard`. Keeping that boundary clean is what makes the
  swap a one-adapter change.

## Naming alignment

The `tenant / plant / line / station / user` hierarchy deliberately uses the same
names, semantics and natural keys as MES. Do not rename these for local
convenience, and do not add a Trace-only level in the middle of the hierarchy.

## Explicitly out of scope (these are MES, not Trace)

OEE, scheduling, downtime analysis, CMMS, maintenance planning, work
instructions authoring. Trace *consumes* the model number from ElectronIx
Digital SOP and *emits* operation-complete events rather than reimplementing
work instructions.

## New since the original brief

- `trace-billing` (Stripe) and `trace-license` are **ElectronIx commercial
  concerns, not MES concerns**. On merge they either stay with Trace or move to a
  shared commercial service. They deliberately have **no foreign keys into the
  manufacturing tables** — the only link is `tenant_id`, which keeps them
  detachable.
- `trace-updater` (OTA) is infrastructure and merges independently of the domain.

## Phase review log

- **Phase 0** — schema and hierarchy named to match MES. No divergence yet.
- **Phase 1** — `JobCardSource` seam in place; route engine takes a resolved job card.
- **Phase 2** — natural keys implemented as unique constraints exactly as tabled above.
