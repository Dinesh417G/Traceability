//! The public trace page served at `/t/{ulid}`.
//!
//! Scanning the code on a product returns this. It has to work on a phone, on a
//! plant LAN, with no internet — so it is a single self-contained HTML document
//! with no external stylesheet, font or script. Nothing here may reference a
//! CDN: the first customer's shop floor has no route to one.

use trace_store::query::UnitHistory;

/// Escape text for HTML.
///
/// Serials, lot numbers and raw device payloads are attacker-influenced in the
/// sense that they come from scanners and instruments rather than from us, and
/// the raw payload in particular is arbitrary bytes from a device. None of it
/// may reach the browser unescaped.
#[must_use]
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Render a unit's full history as a self-contained page.
#[must_use]
pub fn render(h: &UnitHistory) -> String {
    let mut events = String::new();
    for e in &h.events {
        events.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"h\">{}</td></tr>",
            escape(&e.recorded_at.to_rfc3339()),
            escape(&e.kind),
            e.operation_seq.map_or_else(|| "-".into(), |v| v.to_string()),
            escape(e.station.as_deref().unwrap_or("-")),
            escape(e.operator.as_deref().unwrap_or("-")),
            escape(&e.row_hash[..12.min(e.row_hash.len())]),
        ));
    }

    let mut measurements = String::new();
    for m in &h.measurements {
        let verdict_class = if m.verdict == "PASS" { "pass" } else { "fail" };
        measurements.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{} {}</td><td class=\"{}\">{}</td><td>{}</td><td class=\"raw\">{}</td></tr>",
            escape(&m.recorded_at.to_rfc3339()),
            escape(&m.dcp_name),
            escape(&m.value.canonical()),
            escape(m.unit_of_measure.as_deref().unwrap_or("")),
            verdict_class,
            escape(&m.verdict),
            escape(&m.source),
            escape(m.raw_payload.as_deref().unwrap_or("-")),
        ));
    }

    let mut components = String::new();
    for c in &h.components {
        let what = c.child_uid.as_deref().map_or_else(
            || {
                format!(
                    "lot {} &times; {}",
                    escape(c.lot_no.as_deref().unwrap_or("-")),
                    c.qty.unwrap_or_default()
                )
            },
            |uid| format!("unit <a href=\"/t/{0}\">{0}</a>", escape(uid)),
        );
        components.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            c.depth,
            what,
            c.operation_seq,
            escape(&c.recorded_at.to_rfc3339()),
        ));
    }

    let empty_note = |s: &str, what: &str| {
        if s.is_empty() {
            format!("<tr><td colspan=\"6\" class=\"none\">no {what} recorded</td></tr>")
        } else {
            s.to_owned()
        }
    };

    format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{serial} &middot; ElectronIx Trace</title>
<style>
:root{{color-scheme:light dark}}
*{{box-sizing:border-box}}
body{{margin:0;padding:1rem;font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
background:#f6f7f9;color:#14181f}}
@media(prefers-color-scheme:dark){{body{{background:#12151a;color:#e6e9ee}}
.card{{background:#1b1f27!important;border-color:#2a303b!important}}
th{{background:#232833!important}} td,th{{border-color:#2a303b!important}}}}
.wrap{{max-width:60rem;margin:0 auto}}
h1{{font-size:1.3rem;margin:.2rem 0}}
.uid{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:1rem;
letter-spacing:.02em;word-break:break-all}}
.card{{background:#fff;border:1px solid #dfe3ea;border-radius:10px;padding:1rem;margin:1rem 0}}
.meta{{display:grid;grid-template-columns:repeat(auto-fit,minmax(11rem,1fr));gap:.6rem}}
.meta div span{{display:block;font-size:.75rem;text-transform:uppercase;
letter-spacing:.05em;opacity:.6}}
table{{width:100%;border-collapse:collapse;font-size:.85rem}}
th,td{{text-align:left;padding:.4rem .5rem;border-bottom:1px solid #e6e9ef;vertical-align:top}}
th{{background:#f0f2f6;font-weight:600;position:sticky;top:0}}
.scroll{{overflow-x:auto}}
.pass{{color:#0a7d33;font-weight:600}}
.fail{{color:#c0281c;font-weight:700}}
.state{{display:inline-block;padding:.15rem .5rem;border-radius:99px;font-size:.75rem;
font-weight:700;background:#e7edf5;letter-spacing:.04em}}
.h,.raw{{font-family:ui-monospace,Menlo,monospace;font-size:.75rem;opacity:.75;
word-break:break-all}}
.none{{opacity:.55;font-style:italic}}
footer{{font-size:.75rem;opacity:.6;margin:1.5rem 0 .5rem}}
</style></head>
<body><div class="wrap">

<h1>{model} <span class="state">{state}</span></h1>
<div class="uid">{uid}</div>

<div class="card"><div class="meta">
<div><span>Serial</span>{serial}</div>
<div><span>Model</span>{model}</div>
<div><span>Revision</span>{revision}</div>
<div><span>Job card</span>{job}</div>
<div><span>Plant</span>{plant}</div>
<div><span>Born</span>{created}</div>
</div></div>

<div class="card"><h2>Components consumed</h2><div class="scroll"><table>
<thead><tr><th>Depth</th><th>Item</th><th>Op</th><th>When</th></tr></thead>
<tbody>{components}</tbody></table></div></div>

<div class="card"><h2>Measurements</h2><div class="scroll"><table>
<thead><tr><th>When</th><th>Point</th><th>Value</th><th>Verdict</th><th>Source</th>
<th>Raw device payload</th></tr></thead>
<tbody>{measurements}</tbody></table></div></div>

<div class="card"><h2>Event history</h2><div class="scroll"><table>
<thead><tr><th>When</th><th>Event</th><th>Op</th><th>Station</th><th>Operator</th>
<th>Hash</th></tr></thead>
<tbody>{events}</tbody></table></div></div>

<footer>ElectronIx Trace &middot; every measurement is hash chained and append only.
Raw device payloads are retained verbatim.</footer>
</div></body></html>"#,
        uid = escape(&h.uid),
        serial = escape(&h.serial),
        model = escape(&h.model_no),
        revision = escape(&h.revision),
        job = escape(&h.job_number),
        plant = escape(&h.plant),
        state = escape(&h.state),
        created = escape(&h.created_at.to_rfc3339()),
        events = empty_note(&events, "events"),
        measurements = empty_note(&measurements, "measurements"),
        components = empty_note(&components, "components"),
    )
}

/// The page shown when a scanned code resolves to nothing.
#[must_use]
pub fn not_found(uid: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Not found &middot; ElectronIx Trace</title>
<style>body{{font:15px/1.6 system-ui,sans-serif;margin:0;padding:2rem;text-align:center;
color:#14181f;background:#f6f7f9}}
code{{font-family:ui-monospace,Menlo,monospace;background:#e7ebf1;padding:.2rem .4rem;
border-radius:4px;word-break:break-all}}
@media(prefers-color-scheme:dark){{body{{background:#12151a;color:#e6e9ee}}
code{{background:#232833}}}}</style></head>
<body><h1>No unit found</h1>
<p><code>{}</code> does not match any unit known to this plant.</p>
<p style="opacity:.7;font-size:.9rem">If this code was printed at another site, it must be
looked up there. Check the human-readable text beside the code.</p>
</body></html>"#,
        escape(uid)
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use chrono::Utc;
    use trace_store::query::{HistoryComponent, HistoryEvent, HistoryMeasurement};

    fn history() -> UnitHistory {
        UnitHistory {
            uid: "01JX8P2K7F3QW9ABCDEFGHJKMN".into(),
            serial: "EX24A0001537".into(),
            state: "COMPLETED".into(),
            model_no: "EX-VLV-2200".into(),
            revision: "A".into(),
            job_number: "JC-2026-0912".into(),
            plant: "CBE-1".into(),
            created_at: Utc::now(),
            events: vec![HistoryEvent {
                id: "01EV".into(),
                kind: "BORN".into(),
                operation_seq: Some(10),
                station: Some("ST-01".into()),
                operator: Some("R. Kumar".into()),
                detail: None,
                recorded_at: Utc::now(),
                received_at: Utc::now(),
                clock_skewed: false,
                chain_seq: 0,
                row_hash: "abcdef0123456789".into(),
            }],
            measurements: vec![HistoryMeasurement {
                id: "01ME".into(),
                dcp_name: "final_torque".into(),
                unit_of_measure: Some("Nm".into()),
                operation_seq: 20,
                value: trace_core::dcp::Value::Numeric(12.1),
                verdict: "PASS".into(),
                source: "DEVICE".into(),
                raw_payload: Some("T:12.1NM".into()),
                recorded_at: Utc::now(),
                chain_seq: 1,
                row_hash: "beef".into(),
                supersedes: None,
            }],
            components: vec![HistoryComponent {
                depth: 1,
                child_uid: None,
                lot_no: Some("LOT-SCREW-42".into()),
                qty: Some(4.0),
                operation_seq: 20,
                recorded_at: Utc::now(),
            }],
        }
    }

    #[test]
    fn the_page_shows_the_whole_birth_certificate() {
        let html = render(&history());
        assert!(html.contains("EX24A0001537"));
        assert!(html.contains("01JX8P2K7F3QW9ABCDEFGHJKMN"));
        assert!(html.contains("EX-VLV-2200"));
        assert!(html.contains("JC-2026-0912"));
        assert!(html.contains("final_torque"));
        assert!(html.contains("LOT-SCREW-42"));
        assert!(
            html.contains("T:12.1NM"),
            "the raw device payload is the evidence"
        );
    }

    #[test]
    fn the_page_is_self_contained_because_the_plant_has_no_internet() {
        let html = render(&history());
        for external in ["http://", "https://", "//cdn", "<script"] {
            assert!(
                !html.contains(external),
                "page must not reference {external}: the shop floor has no route to it"
            );
        }
    }

    #[test]
    fn device_supplied_text_cannot_inject_markup() {
        // A raw payload is arbitrary bytes from an instrument, and a serial
        // comes off a scanner. Neither may reach the browser unescaped.
        let mut h = history();
        h.serial = "<script>alert(1)</script>".into();
        h.measurements[0].raw_payload = Some("<img src=x onerror=alert(2)>".into());

        let html = render(&h);
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn a_unit_with_no_history_yet_still_renders() {
        // A raw part marked at operation 1 has an empty genealogy. That is a
        // normal state, not an error.
        let mut h = history();
        h.events.clear();
        h.measurements.clear();
        h.components.clear();
        let html = render(&h);
        assert!(html.contains("no components recorded"));
        assert!(html.contains("EX24A0001537"));
    }

    #[test]
    fn the_not_found_page_escapes_the_scanned_value() {
        let html = not_found("<script>x</script>");
        assert!(!html.contains("<script>x"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn escaping_covers_the_dangerous_characters() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }
}
