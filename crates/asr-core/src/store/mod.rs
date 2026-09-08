//! SQLite persistence. See the plan doc's data model section for the full
//! schema rationale (e.g. why `speakers` is project-wide, not per-project).
//!
//! This module is the one place `asr-core` is allowed to know "SQLite"
//! specifically — everything above it should treat storage as an
//! implementation detail so a future cloud build can swap it out (per the
//! plan's Storage/Database trait retreat-path note). We keep it concrete for
//! now rather than introducing a trait with exactly one implementation.

use rusqlite::Connection;

use crate::error::Result;

pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );

        CREATE TABLE IF NOT EXISTS media (
            id          TEXT PRIMARY KEY,
            project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            path        TEXT NOT NULL,
            duration_ms INTEGER,
            sample_rate INTEGER,
            channels    INTEGER,
            sha256      TEXT
        );

        -- Speakers are project-wide, not per-project: identity persists
        -- across meetings, which is the whole point of enrollment.
        CREATE TABLE IF NOT EXISTS speakers (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            is_enrolled INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS speaker_embeddings (
            id              TEXT PRIMARY KEY,
            speaker_id      TEXT NOT NULL REFERENCES speakers(id) ON DELETE CASCADE,
            vector          BLOB NOT NULL,
            source_media_id TEXT REFERENCES media(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS segments (
            id             TEXT PRIMARY KEY,
            media_id       TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            start_ms       INTEGER NOT NULL,
            end_ms         INTEGER NOT NULL,
            speaker_id     TEXT REFERENCES speakers(id) ON DELETE SET NULL,
            text           TEXT NOT NULL DEFAULT '',
            original_text  TEXT NOT NULL DEFAULT '',
            asr_confidence REAL,
            edited_at      TEXT
        );

        CREATE TABLE IF NOT EXISTS words (
            id         TEXT PRIMARY KEY,
            segment_id TEXT NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
            start_ms   INTEGER NOT NULL,
            end_ms     INTEGER NOT NULL,
            text       TEXT NOT NULL,
            confidence REAL
        );

        CREATE TABLE IF NOT EXISTS terms (
            id           TEXT PRIMARY KEY,
            project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            surface      TEXT NOT NULL,
            reading_kana TEXT,
            category     TEXT,
            weight       REAL NOT NULL DEFAULT 1.0,
            enabled      INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS term_aliases (
            id           TEXT PRIMARY KEY,
            term_id      TEXT NOT NULL REFERENCES terms(id) ON DELETE CASCADE,
            surface      TEXT NOT NULL,
            reading_kana TEXT
        );

        CREATE TABLE IF NOT EXISTS corrections (
            id         TEXT PRIMARY KEY,
            media_id   TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            before     TEXT NOT NULL,
            after      TEXT NOT NULL,
            accepted   INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );

        CREATE INDEX IF NOT EXISTS idx_segments_media ON segments(media_id, start_ms);
        CREATE INDEX IF NOT EXISTS idx_words_segment ON words(segment_id, start_ms);
        CREATE INDEX IF NOT EXISTS idx_speaker_embeddings_speaker ON speaker_embeddings(speaker_id);
        CREATE INDEX IF NOT EXISTS idx_terms_project ON terms(project_id);
        "#,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let conn = open_in_memory().unwrap();
        migrate(&conn).unwrap(); // running twice must not error
        conn.execute(
            "INSERT INTO projects (id, name) VALUES ('p1', 'Test Project')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
