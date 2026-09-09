//! Job cards, and the pluggable seam that keeps them swappable.
//!
//! Trace needs a job card today, but ElectronIx MES will eventually own work
//! orders. So the source is pluggable: [`JobCardSource`] is the seam, with a
//! local implementation for v1 and MES/ERP adapters declared but unbuilt.
//!
//! **The route engine never reaches for a source.** It receives an already
//! resolved [`JobCard`]. Keeping that boundary clean is what makes the eventual
//! swap a one-adapter change rather than a refactor.

use crate::id::{Code, RowId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A release of work: build this model, this many, by this date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCard {
    /// Surrogate key.
    pub id: RowId,
    /// Owning tenant.
    pub tenant_id: RowId,
    /// Natural key, unique within the tenant.
    pub number: Code,
    /// Product revision to build.
    pub product_revision_id: RowId,
    /// Quantity ordered.
    pub qty: u32,
    /// Scheduling priority; higher is more urgent.
    pub priority: i32,
    /// Due date, if any.
    pub due_date: Option<DateTime<Utc>>,
    /// Key in the system that owns this work order once MES or ERP does.
    /// Present from migration 001 so the eventual join has somewhere to land.
    pub external_ref: Option<String>,
}

/// Errors a job card source can raise.
#[derive(Debug, thiserror::Error)]
pub enum JobCardError {
    /// No job card with that number.
    #[error("job card {0} not found")]
    NotFound(String),
    /// The adapter is declared but not implemented.
    #[error("{0} job card source is not implemented in this release")]
    NotImplemented(&'static str),
    /// The backing system failed.
    #[error("job card source failure: {0}")]
    Source(String),
}

/// Where job cards come from.
///
/// Implementations may perform I/O; this trait is only the declaration, so
/// `trace-core` itself stays free of it.
pub trait JobCardSource: std::fmt::Debug + Send + Sync {
    /// Resolve a job card by its number.
    ///
    /// # Errors
    /// [`JobCardError::NotFound`] if no such job card exists, or
    /// [`JobCardError::Source`] if the backing system failed.
    fn resolve(&self, number: &Code) -> Result<JobCard, JobCardError>;

    /// Name of this source, for diagnostics.
    fn name(&self) -> &'static str;
}

/// Job cards entered in Trace's own UI. The v1 implementation.
#[derive(Debug, Default)]
pub struct LocalJobCardSource {
    cards: Vec<JobCard>,
}

impl LocalJobCardSource {
    /// Build a source over a set of locally entered job cards.
    #[must_use]
    pub fn new(cards: Vec<JobCard>) -> Self {
        Self { cards }
    }

    /// Add a job card.
    pub fn insert(&mut self, card: JobCard) {
        self.cards.push(card);
    }
}

impl JobCardSource for LocalJobCardSource {
    fn resolve(&self, number: &Code) -> Result<JobCard, JobCardError> {
        self.cards
            .iter()
            .find(|c| &c.number == number)
            .cloned()
            .ok_or_else(|| JobCardError::NotFound(number.to_string()))
    }

    fn name(&self) -> &'static str {
        "local"
    }
}

/// Work orders owned by ElectronIx MES. Declared, not implemented.
#[derive(Debug, Default)]
pub struct MesJobCardSource;

impl JobCardSource for MesJobCardSource {
    fn resolve(&self, _number: &Code) -> Result<JobCard, JobCardError> {
        Err(JobCardError::NotImplemented("MES"))
    }

    fn name(&self) -> &'static str {
        "mes"
    }
}

/// Work orders owned by a customer ERP. Declared, not implemented.
#[derive(Debug, Default)]
pub struct ErpJobCardSource;

impl JobCardSource for ErpJobCardSource {
    fn resolve(&self, _number: &Code) -> Result<JobCard, JobCardError> {
        Err(JobCardError::NotImplemented("ERP"))
    }

    fn name(&self) -> &'static str {
        "erp"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn card(number: &str) -> JobCard {
        JobCard {
            id: 1,
            tenant_id: 1,
            number: Code::new(number).unwrap(),
            product_revision_id: 1,
            qty: 100,
            priority: 0,
            due_date: None,
            external_ref: None,
        }
    }

    #[test]
    fn local_source_resolves_and_reports_misses() {
        let src = LocalJobCardSource::new(vec![card("JC-2026-0912")]);
        assert_eq!(
            src.resolve(&Code::new("JC-2026-0912").unwrap())
                .unwrap()
                .qty,
            100
        );
        let err = src.resolve(&Code::new("JC-NOPE").unwrap()).unwrap_err();
        assert!(matches!(err, JobCardError::NotFound(_)));
    }

    #[test]
    fn unimplemented_adapters_say_so_rather_than_pretending() {
        assert!(matches!(
            MesJobCardSource
                .resolve(&Code::new("X").unwrap())
                .unwrap_err(),
            JobCardError::NotImplemented("MES")
        ));
        assert!(matches!(
            ErpJobCardSource
                .resolve(&Code::new("X").unwrap())
                .unwrap_err(),
            JobCardError::NotImplemented("ERP")
        ));
    }

    #[test]
    fn sources_are_interchangeable_behind_the_trait() {
        // The engine only ever sees a resolved JobCard, so swapping the source
        // must be a one-line change at the composition root.
        let sources: Vec<Box<dyn JobCardSource>> = vec![
            Box::new(LocalJobCardSource::new(vec![card("JC-1")])),
            Box::new(MesJobCardSource),
            Box::new(ErpJobCardSource),
        ];
        let names: Vec<_> = sources.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["local", "mes", "erp"]);
    }
}
