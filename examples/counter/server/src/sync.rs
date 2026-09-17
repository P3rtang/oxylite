//! The app's table hookup — ONE arm per table; the row type IS the
//! hookup (statements are SyncRow-generated in counter-shared). The
//! match is the compile guarantee: a new `CounterTable` variant breaks
//! the build until its arm exists.

use counter_shared::{Counter, CounterTable, Op};
use oxylite::server::{self, OpApply, SyncError};

#[derive(Clone, Copy)]
pub struct Apply;

impl OpApply<CounterTable> for Apply {
    async fn apply(&self, op: &Op, tx: &mut sqlx::postgres::PgConnection) -> Result<(), SyncError> {
        match op.table {
            CounterTable::Counters => server::apply_one::<Counter>(op, tx).await?,
        }
        Ok(())
    }
}
