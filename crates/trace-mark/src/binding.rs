//! The binding engine: `{{unit.uid}}` and friends.
//!
//! Expressions are resolved at print time against a context assembled from the
//! unit, its product, its job card and its measurements. Deliberately tiny:
//! a label template is configuration written by an integrator, not a program,
//! and a template language with control flow would be a liability on a device
//! that prints identity onto physical objects.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use thiserror::Error;

/// Errors from resolving a binding.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BindingError {
    /// The expression named something the context does not have.
    ///
    /// An error rather than an empty string: a label that silently prints a
    /// blank serial is worse than one that refuses to print.
    #[error("unknown binding {0:?}")]
    UnknownBinding(String),

    /// An opening `{{` with no closing `}}`.
    #[error("unterminated binding in template text: {0:?}")]
    Unterminated(String),
}

/// Values available to a template at print time.
#[derive(Debug, Clone, Default)]
pub struct BindingContext {
    values: BTreeMap<String, String>,
    now: Option<DateTime<Utc>>,
}

impl BindingContext {
    /// Empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a name, e.g. `unit.uid`.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Set the instant `{{now:...}}` formats.
    ///
    /// Injected rather than read from the clock so a rendered label is
    /// reproducible in a test and in an audit.
    #[must_use]
    pub fn at(mut self, now: DateTime<Utc>) -> Self {
        self.now = Some(now);
        self
    }

    /// Look a binding up.
    fn resolve(&self, key: &str) -> Option<String> {
        if let Some(fmt) = key.strip_prefix("now:") {
            let now = self.now.unwrap_or_else(Utc::now);
            return Some(format_datetime(now, fmt));
        }
        self.values.get(key).cloned()
    }

    /// Every bound name, for the template designer's autocomplete.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }
}

/// Translate the subset of `dd-MM-yyyy` style patterns integrators expect.
///
/// A deliberately small vocabulary rather than a full date DSL: these are the
/// tokens that appear on manufacturing labels.
fn format_datetime(now: DateTime<Utc>, pattern: &str) -> String {
    // Longest tokens first, so `yyyy` is not eaten by `yy`.
    let mut out = pattern.to_owned();
    for (token, replacement) in [
        ("yyyy", now.format("%Y").to_string()),
        ("MM", now.format("%m").to_string()),
        ("dd", now.format("%d").to_string()),
        ("HH", now.format("%H").to_string()),
        ("mm", now.format("%M").to_string()),
        ("ss", now.format("%S").to_string()),
        ("yy", now.format("%y").to_string()),
    ] {
        out = out.replace(token, &replacement);
    }
    out
}

/// Resolve every `{{binding}}` in a string.
///
/// # Errors
/// Returns [`BindingError::UnknownBinding`] for a name the context lacks, or
/// [`BindingError::Unterminated`] for a malformed template.
pub fn render_expression(template: &str, ctx: &BindingContext) -> Result<String, BindingError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(BindingError::Unterminated(template.to_owned()));
        };
        let key = after[..end].trim();
        let value = ctx
            .resolve(key)
            .ok_or_else(|| BindingError::UnknownBinding(key.to_owned()))?;
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> BindingContext {
        BindingContext::new()
            .with("unit.uid", "01JX8P2K7F3QW9ABCDEFGHJKMN")
            .with("unit.serial", "EX24A0001537")
            .with("product.model", "EX-VLV-2200")
            .with("job.number", "JC-2026-0912")
            .with("measurement.final_torque", "12.1")
            .at(Utc.with_ymd_and_hms(2026, 9, 3, 14, 30, 5).unwrap())
    }

    #[test]
    fn bindings_are_substituted() {
        assert_eq!(
            render_expression("SR NO : {{unit.serial}}", &ctx()).unwrap(),
            "SR NO : EX24A0001537"
        );
    }

    #[test]
    fn multiple_bindings_in_one_string() {
        assert_eq!(
            render_expression("{{product.model}}/{{unit.serial}}", &ctx()).unwrap(),
            "EX-VLV-2200/EX24A0001537"
        );
    }

    #[test]
    fn text_with_no_bindings_passes_through() {
        assert_eq!(
            render_expression("MADE IN INDIA", &ctx()).unwrap(),
            "MADE IN INDIA"
        );
    }

    #[test]
    fn whitespace_inside_the_braces_is_tolerated() {
        assert_eq!(
            render_expression("{{ unit.serial }}", &ctx()).unwrap(),
            "EX24A0001537"
        );
    }

    #[test]
    fn date_patterns_format_as_integrators_expect() {
        assert_eq!(
            render_expression("{{now:dd-MM-yyyy}}", &ctx()).unwrap(),
            "03-09-2026"
        );
        assert_eq!(
            render_expression("{{now:yyyy-MM-dd}}", &ctx()).unwrap(),
            "2026-09-03"
        );
        assert_eq!(
            render_expression("{{now:HH:mm:ss}}", &ctx()).unwrap(),
            "14:30:05"
        );
    }

    #[test]
    fn a_four_digit_year_is_not_eaten_by_the_two_digit_token() {
        // The classic bug: replacing `yy` first turns `yyyy` into `2626`.
        assert_eq!(render_expression("{{now:yyyy}}", &ctx()).unwrap(), "2026");
        assert_eq!(render_expression("{{now:yy}}", &ctx()).unwrap(), "26");
    }

    #[test]
    fn an_unknown_binding_refuses_to_print_rather_than_printing_a_blank() {
        // A label with a silently empty serial is worse than no label: it looks
        // valid and destroys the trace for that unit.
        let err = render_expression("{{unit.nonexistent}}", &ctx()).unwrap_err();
        assert_eq!(err, BindingError::UnknownBinding("unit.nonexistent".into()));
    }

    #[test]
    fn an_unterminated_binding_is_rejected() {
        assert!(matches!(
            render_expression("{{unit.serial", &ctx()).unwrap_err(),
            BindingError::Unterminated(_)
        ));
    }

    #[test]
    fn the_rendered_time_is_injected_so_labels_are_reproducible() {
        // Two renders of the same context must be byte-identical, which they
        // would not be if the engine read the wall clock.
        let c = ctx();
        assert_eq!(
            render_expression("{{now:HH:mm:ss}}", &c).unwrap(),
            render_expression("{{now:HH:mm:ss}}", &c).unwrap()
        );
    }
}
