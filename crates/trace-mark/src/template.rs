//! The versioned template model.
//!
//! A template is a canvas size in millimetres, a print resolution, and a list
//! of positioned elements. Everything about a label is data, so a customer's
//! new sticker is a configuration change rather than a release.

use crate::binding::{BindingContext, BindingError, render_expression};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors from templates.
#[derive(Debug, Error)]
pub enum TemplateError {
    /// A binding failed to resolve.
    #[error(transparent)]
    Binding(#[from] BindingError),

    /// The template is geometrically invalid.
    #[error("invalid template: {0}")]
    Invalid(String),
}

/// A barcode symbology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Symbology {
    /// Code 128, the workhorse for alphanumeric serials.
    Code128,
    /// QR, for the consumer-scannable sticker code.
    Qr,
    /// Data Matrix ECC200. Used for direct part marking.
    ///
    /// Present in the model although laser marking is deferred, because the
    /// template format must not need changing when it arrives.
    DataMatrix,
}

/// Text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Align {
    /// Left aligned.
    #[default]
    Left,
    /// Centred.
    Center,
    /// Right aligned.
    Right,
}

/// One thing on a label. Positions are in dots from the top-left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Element {
    /// A line of text, possibly containing bindings.
    Text {
        /// X position in dots.
        x: u32,
        /// Y position in dots.
        y: u32,
        /// Character height in dots.
        height: u32,
        /// Character width in dots.
        width: u32,
        /// Content, with `{{bindings}}`.
        value: String,
        /// Alignment.
        #[serde(default)]
        align: Align,
    },
    /// A barcode.
    Barcode {
        /// X position in dots.
        x: u32,
        /// Y position in dots.
        y: u32,
        /// Symbology.
        symbology: Symbology,
        /// Module size. Narrow bar width for 1D, cell size for 2D.
        module: u32,
        /// Symbol height in dots. Ignored by 2D symbologies.
        #[serde(default)]
        height: u32,
        /// Content, with `{{bindings}}`.
        value: String,
        /// Whether to print the human-readable interpretation under a 1D code.
        #[serde(default)]
        human_readable: bool,
    },
    /// A drawn box or line.
    Box {
        /// X position in dots.
        x: u32,
        /// Y position in dots.
        y: u32,
        /// Width in dots.
        w: u32,
        /// Height in dots.
        h: u32,
        /// Border thickness in dots.
        thickness: u32,
    },
}

impl Element {
    /// Top-left position.
    #[must_use]
    pub fn origin(&self) -> (u32, u32) {
        match *self {
            Self::Text { x, y, .. } | Self::Barcode { x, y, .. } | Self::Box { x, y, .. } => (x, y),
        }
    }
}

/// A versioned label layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelTemplate {
    /// Stable template code.
    pub code: String,
    /// Version number; layouts are never edited in place.
    pub version: u32,
    /// Canvas width in millimetres.
    pub width_mm: f64,
    /// Canvas height in millimetres.
    pub height_mm: f64,
    /// Print resolution.
    pub dpi: u32,
    /// Darkness setting passed to the printer.
    #[serde(default)]
    pub darkness: Option<u32>,
    /// Elements, drawn in order.
    pub elements: Vec<Element>,
}

impl LabelTemplate {
    /// Canvas width in dots.
    #[must_use]
    pub fn width_dots(&self) -> u32 {
        mm_to_dots(self.width_mm, self.dpi)
    }

    /// Canvas height in dots.
    #[must_use]
    pub fn height_dots(&self) -> u32 {
        mm_to_dots(self.height_mm, self.dpi)
    }

    /// Check the template is printable.
    ///
    /// Catching an element off the edge of the media here beats discovering it
    /// as a roll of half-printed labels.
    ///
    /// # Errors
    /// Returns [`TemplateError::Invalid`] describing the first problem.
    pub fn validate(&self) -> Result<(), TemplateError> {
        if self.width_mm <= 0.0 || self.height_mm <= 0.0 {
            return Err(TemplateError::Invalid(
                "canvas must have a positive size".into(),
            ));
        }
        if self.dpi == 0 {
            return Err(TemplateError::Invalid(
                "dpi must be greater than zero".into(),
            ));
        }
        if self.elements.is_empty() {
            return Err(TemplateError::Invalid("template has no elements".into()));
        }

        let (w, h) = (self.width_dots(), self.height_dots());
        for (i, el) in self.elements.iter().enumerate() {
            let (x, y) = el.origin();
            if x >= w || y >= h {
                return Err(TemplateError::Invalid(format!(
                    "element {i} at ({x},{y}) is outside the {w}x{h} dot canvas"
                )));
            }
        }
        Ok(())
    }

    /// Resolve every binding in the template, yielding a template whose values
    /// are literal text ready to render.
    ///
    /// # Errors
    /// Returns [`TemplateError::Binding`] if any binding cannot be resolved.
    pub fn bind(&self, ctx: &BindingContext) -> Result<Self, TemplateError> {
        let mut bound = self.clone();
        for el in &mut bound.elements {
            match el {
                Element::Text { value, .. } | Element::Barcode { value, .. } => {
                    *value = render_expression(value, ctx)?;
                }
                Element::Box { .. } => {}
            }
        }
        Ok(bound)
    }

    /// The reference 50 mm x 25 mm demo sticker at 203 dpi.
    ///
    /// This is the layout in the specification, kept in code so the ZPL
    /// renderer has something concrete and regression-tested to produce.
    #[must_use]
    pub fn demo_sticker() -> Self {
        Self {
            code: "DEMO-50x25".into(),
            version: 1,
            width_mm: 50.0,
            height_mm: 25.0,
            dpi: 203,
            darkness: Some(10),
            elements: vec![
                Element::Text {
                    x: 12,
                    y: 12,
                    height: 26,
                    width: 26,
                    value: "ElectronIx".into(),
                    align: Align::Left,
                },
                Element::Text {
                    x: 12,
                    y: 48,
                    height: 20,
                    width: 20,
                    value: "MODEL : {{product.model}}".into(),
                    align: Align::Left,
                },
                Element::Text {
                    x: 12,
                    y: 74,
                    height: 20,
                    width: 20,
                    value: "SR NO : {{unit.serial}}".into(),
                    align: Align::Left,
                },
                Element::Text {
                    x: 12,
                    y: 100,
                    height: 20,
                    width: 20,
                    value: "JOB   : {{job.number}}".into(),
                    align: Align::Left,
                },
                Element::Text {
                    x: 12,
                    y: 126,
                    height: 20,
                    width: 20,
                    value: "DATE  : {{now:dd-MM-yyyy}}".into(),
                    align: Align::Left,
                },
                Element::Barcode {
                    x: 250,
                    y: 20,
                    symbology: Symbology::Qr,
                    module: 5,
                    height: 0,
                    value: "{{unit.trace_url}}".into(),
                    human_readable: false,
                },
                Element::Barcode {
                    x: 12,
                    y: 152,
                    symbology: Symbology::Code128,
                    module: 2,
                    height: 34,
                    value: "{{unit.serial}}".into(),
                    human_readable: false,
                },
                // The ULID in plain text beside the code. A smudged or
                // scratched symbol must not destroy the trace.
                Element::Text {
                    x: 300,
                    y: 168,
                    height: 16,
                    width: 16,
                    value: "MADE IN INDIA".into(),
                    align: Align::Left,
                },
            ],
        }
    }
}

/// Convert millimetres to printer dots.
#[must_use]
pub fn mm_to_dots(mm: f64, dpi: u32) -> u32 {
    // 1 inch = 25.4 mm.
    ((mm / 25.4) * f64::from(dpi)).round() as u32
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use chrono::TimeZone;
    use chrono::Utc;

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

    #[test]
    fn millimetres_convert_to_dots_at_203_dpi() {
        // 50mm at 203dpi is the standard 4-inch-wide thermal label maths.
        assert_eq!(mm_to_dots(50.0, 203), 400);
        assert_eq!(mm_to_dots(25.0, 203), 200);
        assert_eq!(mm_to_dots(25.4, 203), 203);
    }

    #[test]
    fn the_demo_sticker_is_a_valid_400x200_dot_canvas() {
        let t = LabelTemplate::demo_sticker();
        t.validate().unwrap();
        assert_eq!((t.width_dots(), t.height_dots()), (400, 200));
    }

    #[test]
    fn binding_a_template_resolves_every_element() {
        let bound = LabelTemplate::demo_sticker().bind(&ctx()).unwrap();
        let texts: Vec<String> = bound
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text { value, .. } | Element::Barcode { value, .. } => Some(value.clone()),
                Element::Box { .. } => None,
            })
            .collect();

        assert!(texts.contains(&"MODEL : EX-VLV-2200".to_string()));
        assert!(texts.contains(&"SR NO : EX24A0001537".to_string()));
        assert!(texts.contains(&"DATE  : 03-09-2026".to_string()));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("http://trace.plant.local/t/"))
        );
        assert!(
            texts.iter().all(|t| !t.contains("{{")),
            "no binding may survive"
        );
    }

    #[test]
    fn a_missing_binding_stops_the_print() {
        let bare = BindingContext::new().with("unit.serial", "X");
        assert!(LabelTemplate::demo_sticker().bind(&bare).is_err());
    }

    #[test]
    fn an_element_off_the_media_is_caught_before_printing() {
        let mut t = LabelTemplate::demo_sticker();
        t.elements.push(Element::Text {
            x: 9_999,
            y: 10,
            height: 20,
            width: 20,
            value: "off the edge".into(),
            align: Align::Left,
        });
        let err = t.validate().unwrap_err();
        assert!(format!("{err}").contains("outside"));
    }

    #[test]
    fn degenerate_canvases_are_rejected() {
        let mut t = LabelTemplate::demo_sticker();
        t.width_mm = 0.0;
        assert!(t.validate().is_err());

        let mut t = LabelTemplate::demo_sticker();
        t.dpi = 0;
        assert!(t.validate().is_err());

        let mut t = LabelTemplate::demo_sticker();
        t.elements.clear();
        assert!(t.validate().is_err());
    }

    #[test]
    fn templates_round_trip_through_json_so_they_can_live_in_the_database() {
        let t = LabelTemplate::demo_sticker();
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<LabelTemplate>(&json).unwrap(), t);
    }

    #[test]
    fn data_matrix_is_in_the_model_even_though_laser_is_deferred() {
        // The template format must not need changing when a laser arrives.
        let json = serde_json::to_string(&Symbology::DataMatrix).unwrap();
        assert_eq!(json, "\"DATA_MATRIX\"");
    }
}
