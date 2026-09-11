//! SQLite persistence for the `terms` / `term_aliases` tables (schema
//! lives in `crate::store`). Kept as its own submodule — specific to
//! terminology, not the general project/media/segment storage `store`
//! covers.

use rusqlite::{params, Connection};

use super::{Term, TermAlias};
use crate::error::Result;

/// Load all enabled terms for a project, ordered by weight descending —
/// the order `crate::asr::truncate_terms` expects, so the
/// highest-priority terms are what survive a provider's keyterm cap.
pub fn load_terms(conn: &Connection, project_id: &str) -> Result<Vec<Term>> {
    let mut stmt = conn.prepare(
        "SELECT surface, reading_kana, category, weight, enabled \
         FROM terms WHERE project_id = ?1 AND enabled = 1 ORDER BY weight DESC",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok(Term {
            surface: row.get(0)?,
            reading_kana: row.get(1)?,
            category: row.get(2)?,
            weight: row.get(3)?,
            enabled: row.get::<_, i64>(4)? != 0,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// Load all aliases for a project's *enabled* terms, joined to their
/// canonical surface — exactly the shape `apply_alias_corrections` needs.
pub fn load_aliases(conn: &Connection, project_id: &str) -> Result<Vec<TermAlias>> {
    let mut stmt = conn.prepare(
        "SELECT t.surface, a.surface \
         FROM term_aliases a JOIN terms t ON t.id = a.term_id \
         WHERE t.project_id = ?1 AND t.enabled = 1",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok(TermAlias { term_surface: row.get(0)?, alias_surface: row.get(1)? })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// Register a new term, returning its generated id (pass to
/// [`add_alias`] to attach known surface variants to it).
pub fn add_term(conn: &Connection, project_id: &str, term: &Term) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO terms (id, project_id, surface, reading_kana, category, weight, enabled) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            project_id,
            term.surface,
            term.reading_kana,
            term.category,
            term.weight,
            term.enabled as i64
        ],
    )?;
    Ok(id)
}

/// Register a known alternate surface form for an existing term — e.g. an
/// English-script rendering an ASR provider might produce (see the plan
/// doc's "クラウドゲート問題"). No reading is stored for the alias itself:
/// an alias is a known-*wrong* literal string being replaced outright
/// (`apply_alias_corrections` matches on surface only), not something
/// with a "correct" reading of its own.
pub fn add_alias(conn: &Connection, term_id: &str, alias_surface: &str) -> Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO term_aliases (id, term_id, surface) VALUES (?1, ?2, ?3)",
        params![id, term_id, alias_surface],
    )?;
    Ok(())
}

/// A term row together with its `terms.id` -- unlike [`Term`] (which
/// `load_terms` returns for the ASR-facing read path), the terms panel
/// needs the id to address a specific row for `update_term`/`delete_term`/
/// `set_enabled`.
pub struct TermRow {
    pub id: String,
    pub term: Term,
}

/// Load every term for a project, enabled or not (unlike [`load_terms`],
/// which is the ASR-facing read path and filters to `enabled = 1` only) --
/// the terms panel needs to show and let the user re-enable a disabled
/// term, not just hide it. Ordered by weight descending, matching
/// `load_terms`.
pub fn load_all_terms(conn: &Connection, project_id: &str) -> Result<Vec<TermRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, surface, reading_kana, category, weight, enabled \
         FROM terms WHERE project_id = ?1 ORDER BY weight DESC",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok(TermRow {
            id: row.get(0)?,
            term: Term {
                surface: row.get(1)?,
                reading_kana: row.get(2)?,
                category: row.get(3)?,
                weight: row.get(4)?,
                enabled: row.get::<_, i64>(5)? != 0,
            },
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// Overwrites every field of an existing term row (surface/reading_kana/
/// category/weight/enabled) -- the terms panel's save path for a row whose
/// `Textarea`-backed fields the user edited.
pub fn update_term(conn: &Connection, id: &str, term: &Term) -> Result<()> {
    conn.execute(
        "UPDATE terms SET surface = ?2, reading_kana = ?3, category = ?4, weight = ?5, enabled = ?6 \
         WHERE id = ?1",
        params![id, term.surface, term.reading_kana, term.category, term.weight, term.enabled as i64],
    )?;
    Ok(())
}

/// Deletes a term row outright. `term_aliases` rows referencing it are
/// removed automatically (`ON DELETE CASCADE` in the schema).
pub fn delete_term(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM terms WHERE id = ?1", params![id])?;
    Ok(())
}

/// Flips a term's enabled flag without touching its other fields --
/// structural change, so the terms panel applies this immediately rather
/// than waiting for the next autosave tick (matching the app's existing
/// "structural changes are eager" convention, e.g. speaker add/merge).
pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<()> {
    conn.execute("UPDATE terms SET enabled = ?2 WHERE id = ?1", params![id, enabled as i64])?;
    Ok(())
}

/// An alias row together with its own id and the term it belongs to --
/// mirrors [`TermRow`] (not yet consumed by any UI; added for API
/// completeness alongside the term CRUD above, since a future alias-editing
/// UI would need addressable rows the same way).
pub struct AliasRow {
    pub id: String,
    pub term_id: String,
    pub alias_surface: String,
}

/// Load every alias for a project's terms, enabled or not (unlike
/// [`load_aliases`], which filters to the term's `enabled = 1`).
pub fn load_all_aliases(conn: &Connection, project_id: &str) -> Result<Vec<AliasRow>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.term_id, a.surface \
         FROM term_aliases a JOIN terms t ON t.id = a.term_id \
         WHERE t.project_id = ?1",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok(AliasRow { id: row.get(0)?, term_id: row.get(1)?, alias_surface: row.get(2)? })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn delete_alias(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM term_aliases WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    #[test]
    fn round_trips_term_and_alias_through_sqlite() {
        let conn = store::open_in_memory().unwrap();
        conn.execute("INSERT INTO projects (id, name) VALUES ('p1', 'Test')", [])
            .unwrap();

        let term_id = add_term(
            &conn,
            "p1",
            &Term {
                surface: "クラウドゲート".to_string(),
                reading_kana: Some("クラウドゲート".to_string()),
                category: None,
                weight: 2.0,
                enabled: true,
            },
        )
        .unwrap();
        add_alias(&conn, &term_id, "Cloud Gate").unwrap();

        let terms = load_terms(&conn, "p1").unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].surface, "クラウドゲート");

        let aliases = load_aliases(&conn, "p1").unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].term_surface, "クラウドゲート");
        assert_eq!(aliases[0].alias_surface, "Cloud Gate");
    }

    #[test]
    fn disabled_terms_and_their_aliases_are_excluded() {
        let conn = store::open_in_memory().unwrap();
        conn.execute("INSERT INTO projects (id, name) VALUES ('p1', 'Test')", [])
            .unwrap();

        let term_id = add_term(
            &conn,
            "p1",
            &Term {
                surface: "無効化された用語".to_string(),
                reading_kana: None,
                category: None,
                weight: 1.0,
                enabled: false,
            },
        )
        .unwrap();
        add_alias(&conn, &term_id, "disabled alias").unwrap();

        assert!(load_terms(&conn, "p1").unwrap().is_empty());
        assert!(load_aliases(&conn, "p1").unwrap().is_empty());
    }

    #[test]
    fn weight_orders_terms_descending() {
        let conn = store::open_in_memory().unwrap();
        conn.execute("INSERT INTO projects (id, name) VALUES ('p1', 'Test')", [])
            .unwrap();

        add_term(
            &conn,
            "p1",
            &Term { surface: "低優先度".to_string(), reading_kana: None, category: None, weight: 0.5, enabled: true },
        )
        .unwrap();
        add_term(
            &conn,
            "p1",
            &Term { surface: "高優先度".to_string(), reading_kana: None, category: None, weight: 5.0, enabled: true },
        )
        .unwrap();

        let terms = load_terms(&conn, "p1").unwrap();
        assert_eq!(terms[0].surface, "高優先度");
        assert_eq!(terms[1].surface, "低優先度");
    }

    #[test]
    fn terms_panel_crud_round_trips() {
        let conn = store::open_in_memory().unwrap();
        conn.execute("INSERT INTO projects (id, name) VALUES ('p1', 'Test')", [])
            .unwrap();

        let term = Term {
            surface: "見出し語".to_string(),
            reading_kana: Some("ミダシゴ".to_string()),
            category: None,
            weight: 1.0,
            enabled: true,
        };
        let id = add_term(&conn, "p1", &term).unwrap();

        // load_all_terms includes disabled rows -- confirm before disabling.
        let rows = load_all_terms(&conn, "p1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].term.surface, "見出し語");

        set_enabled(&conn, &id, false).unwrap();
        let rows = load_all_terms(&conn, "p1").unwrap();
        assert!(!rows[0].term.enabled);
        // load_terms (the ASR-facing read path) excludes it now.
        assert!(load_terms(&conn, "p1").unwrap().is_empty());

        let updated =
            Term { surface: "更新後".to_string(), reading_kana: None, category: None, weight: 3.0, enabled: true };
        update_term(&conn, &id, &updated).unwrap();
        let rows = load_all_terms(&conn, "p1").unwrap();
        assert_eq!(rows[0].term.surface, "更新後");
        assert_eq!(rows[0].term.weight, 3.0);
        assert!(rows[0].term.enabled);

        add_alias(&conn, &id, "alias").unwrap();
        assert_eq!(load_all_aliases(&conn, "p1").unwrap().len(), 1);

        delete_term(&conn, &id).unwrap();
        assert!(load_all_terms(&conn, "p1").unwrap().is_empty());
        // ON DELETE CASCADE takes the alias with it.
        assert!(load_all_aliases(&conn, "p1").unwrap().is_empty());
    }
}
