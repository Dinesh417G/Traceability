//! Connection pool and the tenant-scoped transaction handle.

use crate::error::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Transaction};
use std::time::Duration;

/// A connection pool plus the operations that do not belong to one tenant.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect with settings suited to an edge box: a small pool, because one
    /// plant's stations do not need dozens of connections, and a bounded
    /// acquire timeout so a stuck query surfaces rather than hanging a shift.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] if the pool cannot be created.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .test_before_acquire(true)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Borrow the underlying pool, for callers that genuinely need it.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply all embedded migrations.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Migration`] if a migration fails.
    pub async fn migrate(&self) -> Result<()> {
        crate::MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Open a tenant-scoped transaction.
    ///
    /// This is the only way to reach the repositories, which is deliberate:
    /// there is no code path that queries traceability data without a tenant
    /// context, so RLS cannot be bypassed by forgetting to set one.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] if the transaction cannot start.
    pub async fn tenant(&self, tenant_id: i64) -> Result<TenantTx<'_>> {
        let mut tx = self.pool.begin().await?;
        // SET LOCAL is scoped to this transaction and is reset on commit or
        // rollback, so a pooled connection can never leak one tenant's context
        // into the next request that borrows it.
        sqlx::query("SELECT set_config('app.tenant_id', $1::text, true)")
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
        Ok(TenantTx { tx, tenant_id })
    }

    /// Close the pool.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// A transaction with a tenant context already established.
///
/// Every repository method hangs off this type. Dropping it without calling
/// [`TenantTx::commit`] rolls back, which is the safe default for a station
/// that loses power mid-operation.
#[derive(Debug)]
pub struct TenantTx<'a> {
    pub(crate) tx: Transaction<'a, Postgres>,
    pub(crate) tenant_id: i64,
}

impl TenantTx<'_> {
    /// The tenant this transaction is scoped to.
    #[must_use]
    pub fn tenant_id(&self) -> i64 {
        self.tenant_id
    }

    /// Commit.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] if the commit fails.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }

    /// Roll back explicitly.
    ///
    /// # Errors
    /// Returns [`crate::StoreError::Database`] if the rollback fails.
    pub async fn rollback(self) -> Result<()> {
        self.tx.rollback().await?;
        Ok(())
    }

    /// Borrow the underlying executor for bespoke queries.
    pub fn executor(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }
}
