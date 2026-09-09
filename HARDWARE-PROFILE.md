# HARDWARE-PROFILE

What the software is validated against. **Update when terminal hardware is chosen.**

## Status

Operator terminal hardware is **not yet selected**. Assume nothing. The station
app runs on **Windows and Linux equally** — no Windows-only dependency anywhere
in the runtime path.

## Edge box (minimum validated spec)

| | Minimum | Recommended |
|---|---|---|
| CPU | x86-64, 4 cores | 8 cores |
| RAM | 8 GB | 16 GB |
| Disk | 128 GB SSD | 512 GB SSD (measurements grow ~7M rows/plant/year) |
| OS | Debian 12 / Ubuntu 22.04+ | same |
| Postgres | 16 | 16 |
| Network | 1 GbE plant LAN. **Internet not required.** | same |

## Station terminal (target profile, unvalidated until hardware is chosen)

| | Requirement |
|---|---|
| OS | Windows 10/11 **or** Linux — both must work |
| Display | ≥ 10", ≥ 1280×800, readable at arm's length under factory lighting |
| Input | Capacitive touch, **operable with gloves**. No keyboard assumed |
| Storage | Local SSD for the SQLite spool — the station must survive edge loss |
| RTC | Battery-backed. A drifting RTC silently corrupts traceability |

### UI constraints that follow from "gloves, no keyboard"

- Large hit targets (≥ 12 mm physical), generous spacing.
- Badge scan or PIN login. No password typing.
- On-screen numeric pads for all manual data entry.
- **No hover states. No right-click.** Neither exists on a touchscreen.
- High contrast; assume poor lighting and a scratched screen protector.

## Printers

- **Zebra, driven by raw ZPL II over TCP 9100.** No OS print driver, which is
  what keeps us OS-agnostic. USB and file sinks exist for sites with no network
  printer.
- TSPL / Godex is feature-gated and **not built** in v1.

## Scanners

- 1D/2D imager, USB-HID keyboard-wedge or serial. Any scanner works for
  **printed labels**.
- **If and when laser DPM is enabled** (deferred — see `docs/LASER-DPM-DEFERRED.md`),
  stations downstream of operation 1 need a **DPM-capable imager** with
  appropriate illumination (diffuse dome for shiny/curved, low-angle for etched
  flat). An ordinary 1D scanner will not reliably read a laser mark on bare
  metal. Budgeting a cheap scanner per station and discovering this at
  commissioning is the classic way these projects fail.

## Clock discipline

Stations sync to the edge; the edge syncs to NTP or a local time source. Every
row records device time (`recorded_at`) **and** server time (`received_at`), and
events whose skew exceeds a configured threshold are flagged. A bad RTC on a
panel PC corrupts traceability in a way that is very hard to detect after the fact.
