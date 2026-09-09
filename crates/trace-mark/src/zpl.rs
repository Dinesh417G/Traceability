//! Native ZPL II generation.
//!
//! Emitting ZPL directly, rather than going through an OS print driver, is what
//! keeps the station identical on Windows and Linux and lets a printer be
//! swapped without touching a terminal.
//!
//! ZPL is not escaped by the language itself: the caret and tilde are control
//! characters wherever they appear in field data. A serial or a URL containing
//! one would otherwise be interpreted as a command, so field data goes through
//! [`escape_field`] before it is emitted.

use crate::template::{Align, Element, LabelTemplate, Symbology, TemplateError};

/// Render a bound template to ZPL II.
///
/// The template must already have had its bindings resolved by
/// [`LabelTemplate::bind`]; anything still containing `{{` is a caller bug and
/// is rejected rather than printed literally onto a part.
///
/// # Errors
/// Returns [`TemplateError::Invalid`] if the template does not validate or
/// still contains unresolved bindings.
pub fn render_zpl(template: &LabelTemplate) -> Result<String, TemplateError> {
    template.validate()?;

    for el in &template.elements {
        if let Element::Text { value, .. } | Element::Barcode { value, .. } = el
            && value.contains("{{")
        {
            return Err(TemplateError::Invalid(format!(
                "unresolved binding in {value:?}: call bind() before rendering"
            )));
        }
    }

    let mut z = String::with_capacity(512);
    z.push_str("^XA\n");
    z.push_str(&format!("^PW{}", template.width_dots()));
    z.push_str(&format!("^LL{}", template.height_dots()));
    z.push_str("^LH0,0");
    if let Some(d) = template.darkness {
        z.push_str(&format!("^MD{d}"));
    }
    z.push('\n');

    for el in &template.elements {
        match el {
            Element::Text {
                x,
                y,
                height,
                width,
                value,
                align,
            } => {
                let justify = match align {
                    Align::Left => "",
                    Align::Center => "^FB400,1,0,C",
                    Align::Right => "^FB400,1,0,R",
                };
                z.push_str(&format!(
                    "^FO{x},{y}{justify}^A0N,{height},{width}^FD{}^FS\n",
                    escape_field(value)
                ));
            }

            Element::Barcode {
                x,
                y,
                symbology,
                module,
                height,
                value,
                human_readable,
            } => {
                let hr = if *human_readable { "Y" } else { "N" };
                match symbology {
                    Symbology::Code128 => {
                        // ^BY sets module width and ratio; ^BC is Code 128.
                        z.push_str(&format!(
                            "^FO{x},{y}^BY{module},2.0,{height}^BCN,{height},{hr},N,N^FD{}^FS\n",
                            escape_field(value)
                        ));
                    }
                    Symbology::Qr => {
                        // ^BQN,2,<magnification>; QA, selects automatic mode
                        // with error correction level A.
                        z.push_str(&format!(
                            "^FO{x},{y}^BQN,2,{module}^FDQA,{}^FS\n",
                            escape_field(value)
                        ));
                    }
                    Symbology::DataMatrix => {
                        // Present so the renderer is complete; laser DPM
                        // itself is deferred. See docs/LASER-DPM-DEFERRED.md.
                        z.push_str(&format!(
                            "^FO{x},{y}^BXN,{module},200^FD{}^FS\n",
                            escape_field(value)
                        ));
                    }
                }
            }

            Element::Box {
                x,
                y,
                w,
                h,
                thickness,
            } => {
                z.push_str(&format!("^FO{x},{y}^GB{w},{h},{thickness}^FS\n"));
            }
        }
    }

    z.push_str("^XZ\n");
    Ok(z)
}

/// Escape ZPL control characters in field data.
///
/// `^` and `~` start commands wherever they appear, and `\` is ZPL's own escape
/// introducer. A serial number or URL containing one of these would otherwise
/// be executed rather than printed — at best producing a corrupt label, at
/// worst a label carrying the wrong identity.
#[must_use]
pub fn escape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '^' => out.push_str("_5E"),
            '~' => out.push_str("_7E"),
            '\\' => out.push_str("_5C"),
            // A raw newline would terminate the field early.
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::binding::BindingContext;
    use chrono::{TimeZone, Utc};

    fn ctx() -> BindingContext {
        BindingContext::new()
            .with("unit.serial", "EX24A0001537")
            .with("product.model", "EX-VLV-2200")
            .with("job.number", "JC-2026-0912")
            .with(
                "unit.trace_url",
                "http://trace.plant.local/t/01JX8P2K7F3QW9ABCDEFGHJKMN",
            )
            .at(Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap())
    }

    fn demo_zpl() -> String {
        render_zpl(&LabelTemplate::demo_sticker().bind(&ctx()).unwrap()).unwrap()
    }

    #[test]
    fn the_demo_sticker_produces_the_expected_zpl_structure() {
        let z = demo_zpl();
        // Matches the reference layout in the specification.
        assert!(z.starts_with("^XA"), "must open with ^XA");
        assert!(z.trim_end().ends_with("^XZ"), "must close with ^XZ");
        assert!(z.contains("^PW400"), "50mm at 203dpi is 400 dots wide");
        assert!(z.contains("^LL200"), "25mm at 203dpi is 200 dots tall");
        assert!(z.contains("^MD10"));
    }

    #[test]
    fn every_field_is_terminated() {
        let z = demo_zpl();
        assert_eq!(
            z.matches("^FO").count(),
            z.matches("^FS").count(),
            "every field origin needs a field separator, or the printer stalls"
        );
    }

    #[test]
    fn resolved_values_reach_the_output() {
        let z = demo_zpl();
        assert!(z.contains("MODEL : EX-VLV-2200"));
        assert!(z.contains("SR NO : EX24A0001537"));
        assert!(z.contains("JOB   : JC-2026-0912"));
        assert!(z.contains("DATE  : 03-09-2026"));
        assert!(z.contains("MADE IN INDIA"));
    }

    #[test]
    fn the_qr_carries_the_full_trace_url() {
        let z = demo_zpl();
        assert!(z.contains("^BQN,2,5"));
        assert!(
            z.contains("QA,http://trace.plant.local/t/01JX8P2K7F3QW9ABCDEFGHJKMN"),
            "the QR must resolve on the plant LAN with no internet"
        );
    }

    #[test]
    fn the_serial_is_also_a_code128_so_a_1d_scanner_still_works() {
        let z = demo_zpl();
        assert!(z.contains("^BCN,34"));
        assert!(z.contains("^BY2,2.0,34"));
    }

    #[test]
    fn control_characters_in_field_data_are_escaped() {
        // A serial or URL containing ^ or ~ would otherwise be executed as a
        // command, producing a corrupt label or one with the wrong identity.
        assert_eq!(escape_field("ABC^XZ"), "ABC_5EXZ");
        assert_eq!(escape_field("A~B"), "A_7EB");
        assert_eq!(escape_field(r"A\B"), "A_5CB");
        assert_eq!(escape_field("line1\nline2"), "line1 line2");
    }

    #[test]
    fn a_hostile_serial_cannot_inject_zpl_commands() {
        let ctx = ctx().with("unit.serial", "EX1^XZ^XA^FO0,0^FDGOTCHA");
        let z = render_zpl(&LabelTemplate::demo_sticker().bind(&ctx).unwrap()).unwrap();
        // Exactly one label: the injected ^XZ/^XA must not have split it.
        assert_eq!(
            z.matches("^XA").count(),
            1,
            "injection created extra labels:\n{z}"
        );
        assert_eq!(z.matches("^XZ").count(), 1);
        assert!(
            z.contains("_5EXZ_5EXA"),
            "the payload must be escaped, not executed"
        );
    }

    #[test]
    fn rendering_an_unbound_template_is_refused() {
        // Printing a literal "{{unit.serial}}" onto a part would be worse than
        // failing: the label looks real and carries no identity.
        let err = render_zpl(&LabelTemplate::demo_sticker()).unwrap_err();
        assert!(format!("{err}").contains("unresolved binding"));
    }

    #[test]
    fn an_invalid_template_never_reaches_the_printer() {
        let mut t = LabelTemplate::demo_sticker().bind(&ctx()).unwrap();
        t.dpi = 0;
        assert!(render_zpl(&t).is_err());
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(demo_zpl(), demo_zpl());
    }
}
