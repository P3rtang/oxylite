//! The notes row — the ONE note type, shared by both sides (reviewer
//! decision, #30). The mapping lives here because the orphan rule
//! allows exactly this placement (trait + type in one crate) and
//! because the mapping is the app's business: the LIB never names a
//! row type, it only consumes `SyncRow`/`FromRow` generically. The
//! SQL generators are the single source for both appliers; the decode
//! is client-only (feature `client`).

use crate::sync_row::SyncRow;
use crate::{Table, Timestamp};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub updated_at: Timestamp,
}

/// One declaration feeds every generated statement on BOTH sides: local
/// inserts, the list query, the engine's batched LWW upserts, and the
/// server's apply path — all derive from this.
impl SyncRow for Note {
    const TABLE: Table = Table::Notes;
    const COLUMNS: &'static [&'static str] = &["id", "title", "body", "updated_at"];
    const LWW: Option<&'static str> = Some("updated_at");

    fn params(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.title.clone(),
            self.body.clone(),
            self.updated_at.canonical_text(),
        ]
    }

    fn pk(&self) -> Uuid {
        self.id
    }
}

#[cfg(feature = "client")]
mod client {
    //! Rows come back as JS objects; Note knows how to build itself.

    use super::Note;
    use crate::from_row::{FromRow, Row, RowError, str_field};
    use crate::timestamp::Timestamp;

    impl FromRow for Note {
        fn from_row(row: &Row) -> Result<Self, RowError> {
            Ok(Note {
                id: str_field(row, "id")
                    .ok_or_else(|| RowError::MissingColumn("id".into()))?
                    .parse()
                    .map_err(|_| RowError::BadUuid("id".into()))?,
                title: str_field(row, "title")
                    .ok_or_else(|| RowError::MissingColumn("title".into()))?,
                body: str_field(row, "body")
                    .ok_or_else(|| RowError::MissingColumn("body".into()))?,
                updated_at: Timestamp::parse(
                    str_field(row, "updated_at")
                        .ok_or_else(|| RowError::MissingColumn("updated_at".into()))?
                        .as_str(),
                )
                .map_err(|e| RowError::BadTimestamp("updated_at".into(), e))?,
            })
        }
    }
}

#[cfg(feature = "server")]
mod server {
    //! sqlx's own `FromRow` so server code can `query_as::<Note>` — the
    //! columns are `timestamptz`, so the LWW decodes as a real DateTime
    //! and goes through the infallible constructor (the storage ring did
    //! the validating).

    use super::Note;
    use crate::timestamp::Timestamp;

    impl sqlx::FromRow for Note {
        fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
            use sqlx::Row as _;
            Ok(Note {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                body: row.try_get("body")?,
                updated_at: Timestamp::from_datetime(row.try_get("updated_at")?),
            })
        }
    }
}
