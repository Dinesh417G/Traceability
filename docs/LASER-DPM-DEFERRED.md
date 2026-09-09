# Laser Direct Part Marking (DPM) — DEFERRED DESIGN

> **Status: deferred by instruction (2026-09-09).** Not cancelled, not forgotten.
> This document is the complete design so that implementing it later is a
> mechanical exercise rather than a rediscovery. See `DECISIONS.md` D-002.

## What is already in place, so this stays additive

| Seam | Where | State |
|---|---|---|
| `MarkMethod` per product revision | `trace-core`, migration 001 | **built** — `LASER_DPM \| LABEL \| BOTH \| NONE` |
| `MARK_VERIFIED` gate | `trace-core::gate` | **built and tested** |
| `mark_record` table (attempt no., outcome, verification, raw bytes) | migration 001 | **built** |
| Template + binding engine (`{{unit.uid}}` etc.) | `trace-mark` | **built**, renderer-agnostic |
| `MarkerDriver` trait seam | `trace-mark::marker` | **declared**, one simulated impl |
| ZPL renderer | `trace-mark::zpl` | **built** |
| Data Matrix renderer | — | **not built** (deferred) |
| File-drop / serial / PLC marker drivers | — | **not built** (deferred) |

Deferring the *device* did not require deferring the *gate*. `MARK_VERIFIED`
works today against printed labels.

## Data Matrix ECC200, not QR — do not "helpfully" switch this back

- Higher data density in a small area — critical with 6 × 6 mm of flat surface
  on a casting.
- Reed–Solomon error correction recovers a mark damaged by machining, handling,
  paint overspray or corrosion.
- It is the accepted DPM standard; handheld DPM scanners and vision systems are
  tuned for it.
- Reads reliably at low contrast on bare metal, where QR often will not.

**QR stays on the paper sticker** (consumer-scannable with any phone).
**Data Matrix goes on the part.** Both encode the same ULID.

## Mark-then-verify is mandatory

An unreadable mark is a silently destroyed traceability record, discovered months
later.

- Every marking operation is followed by a `MARK_VERIFIED` gate: a reader scans
  the mark back and the decoded value must equal the intended UID.
- With a grading-capable reader, record the **ISO/IEC 29158 (AIM DPM)** grade on
  `mark_record` and enforce a per-product minimum. Without one, the minimum bar
  is a successful read-back.
- A failed verify triggers a configured remark or quarantine path.
  **Never let an unverified unit advance.**

## Driver strategies (plant picks in config)

1. **File drop + trigger** — build first, the v1 default. Contract below.
2. **Serial / TCP command protocol** — vendor ASCII: send content, trigger, await
   completion/error.
3. **Digital I/O handshake via PLC** — Trace writes the string to a PLC tag, the
   PLC runs the laser handshake and returns done/fault. Very common on
   integrated lines.
4. **Vendor SDK / DLL** — Windows-only, feature-gated. Typical of EZCAD / BJJCZ
   controller fibre lasers, widespread in Indian MSME shops.
5. **Simulated** — renders the mark to PNG and can be told to produce a bad mark
   on demand, so the verify gate is testable in CI.

### File-drop contract — get this exactly right

A laser watching a folder will happily pick up a half-written file and mark
garbage onto a customer's part.

- Write to a temporary name **in the same filesystem**, `fsync`, then
  **atomically rename** into the watched folder. Never write directly into the
  watched path.
- One file per unit, named with the ULID, so a stale file is traceable rather
  than mysterious.
- **Define the completion signal explicitly in config**: result file in a
  done/error folder, a digital input, or a serial ack. *A file-drop with no
  completion signal is not an integration* — if the chosen laser cannot report
  done/fault, drive it through a PLC (strategy 3) instead.
- Timeout, retry and stale-file sweep policies are **configuration, not constants**.
- Log the exact bytes written and the exact response into `mark_record`.

## Because the mark is the identity from operation 1

- A unit is **born at operation 1 on a raw part** — job card and product revision,
  zero components. The model must support state `MARKED` with an **empty
  genealogy**. (This is already true in `trace-core`, and tested.)
- A marked raw part can fail incoming inspection or be damaged before assembly.
  `scrap` on a marked-but-unbuilt unit is supported. **A scrapped UID is retired
  forever and never reissued** — there is a test for this.
- **Remarking:** on verify failure allow N configured attempts (typically to an
  alternate position defined per product), then quarantine. Every attempt is its
  own `mark_record` row with attempt number and outcome. A part must never leave
  operation 1 with two conflicting readable marks — if a failed mark is still
  partially readable, the route must force it to be defaced or the part scrapped.
- Mark placement is per-product config: datum reference, X/Y offset, cell size,
  target module size, laser recipe/template name. Never hardcode it.

## Aluminium and plastic are different problems

The first site marks **aluminium castings and plastics**, sometimes on one line.
This is not one setting.

- Aluminium takes a fibre laser well — annealed or engraved Data Matrix with good
  contrast is routine.
- Plastics are polymer-dependent and far less forgiving: low contrast, melting,
  unpredictable discolouration. Light-coloured plastics are the worst case, and
  **a mark a human can see is not necessarily a mark a DPM reader can decode.**
- **Therefore mark strategy is per product revision, not per plant.** Config
  allows laser DPM, a printed label applied at operation 1, or both. If a plastic
  part cannot hold a gradeable mark, a labelled identity at operation 1 is the
  correct engineering answer, and the route engine treats it as first-class.
- Store per revision: material, mark method, laser recipe reference, target
  module size, and **minimum acceptable read/grade** — which will legitimately
  differ between the aluminium and the plastic parts.

## BLOCKING PROCUREMENT TASK

**Before any laser is purchased, run a marking trial on the customer's actual
parts** — every material and every colour variant — and grade the resulting Data
Matrix **with a verifier, not by eye**.

A laser that handles the castings beautifully and cannot produce a readable code
on the plastic housings is a discovery you want at the quotation stage, not at
commissioning.

## Questions to put to laser vendors before purchase

1. How does it accept variable data — watched folder, serial/TCP, PLC I/O, or
   SDK? Is there documentation, or only a GUI?
2. Does it report mark **completion and failure** back to the host, and how?
3. Can it mark Data Matrix ECC200 at the module size we need on our material, and
   at what cycle time?
4. Does the vendor supply a DPM reader, and ideally a grading-capable one?
5. Is the marking template stored on the laser (we send data only) or sent per
   part (we send geometry)? The first is far simpler to integrate.
6. Windows-only SDK, or a protocol usable from Linux?

## Non-negotiable

Mark content is generated by the same binding engine as labels, so `{{unit.uid}}`
means the same thing everywhere.

**Never let the laser software own the serial number sequence.** Trace owns
identity; the laser only renders what it is given. Vendor marking software that
auto-increments its own counter is the single most common cause of duplicate and
skipped serials in the field. If the chosen laser insists on that mode, we reject
that mode.

## Reader hardware — flag before the customer buys

Every station downstream of operation 1 would identify the unit by reading a
laser mark on bare metal. **An ordinary barcode scanner will not do this
reliably.** Stations need a DPM-capable imager with appropriate illumination.
Put the reader spec in the hardware BOM at design time.
