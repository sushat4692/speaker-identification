//! SQLite persistence for `projects` / `media` / `segments` / `speakers`
//! (schema lives in `crate::store`) -- the "project history" feature: list
//! past transcription runs, reopen one, and autosave edits back to it.
//! Kept as its own submodule for the same reason `terms/store.rs` is: it's
//! specific to project/transcript persistence, not the general
//! connection/schema plumbing `store::mod` covers.
//!
//! Scope deliberately kept narrow (see docs/plan.md's "(1d) プロジェクト
//! 履歴" section for the full rationale):
//! - `speakers` rows are minted per-project, not shared across projects --
//!   real cross-project speaker identity (enrollment) isn't wired into the
//!   desktop app's GUI yet, so this doesn't attempt to solve that.
//! - `original_text`/`edited_at`/`asr_confidence` aren't tracked -- every
//!   save mirrors `text` into `original_text` and leaves the other two
//!   NULL. True edit-history tracking is separate, not-yet-started future
//!   work (matches the still-unused `corrections` table).
//! - `save_segments` always deletes and reinserts all of a media's segments
//!   rather than diffing -- there's no stable per-segment id to diff
//!   against (see the point above), and a full replace is simpler and
//!   correct at the segment counts this app deals with (tens, not
//!   thousands).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::pipeline::TranscriptSegment;

/// One row for the project-list screen.
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub media_path: String,
    pub segment_count: i64,
}

/// Newest first. Assumes one media file per project -- the current
/// desktop app model (a project is created from exactly one upload).
pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectSummary>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.created_at, m.path, \
                (SELECT COUNT(*) FROM segments s WHERE s.media_id = m.id) \
         FROM projects p JOIN media m ON m.project_id = p.id \
         ORDER BY p.created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProjectSummary {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            media_path: row.get(3)?,
            segment_count: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// Creates a new project + its one media row. `duration_ms`/`sample_rate`/
/// `channels`/`sha256` are left NULL -- the desktop app doesn't track them
/// today, and inventing values for them would be worse than leaving them
/// absent.
pub fn create_project(conn: &Connection, name: &str, media_path: &Path) -> Result<(String, String)> {
    let project_id = uuid::Uuid::new_v4().to_string();
    let media_id = uuid::Uuid::new_v4().to_string();
    conn.execute("INSERT INTO projects (id, name) VALUES (?1, ?2)", params![project_id, name])?;
    conn.execute(
        "INSERT INTO media (id, project_id, path) VALUES (?1, ?2, ?3)",
        params![media_id, project_id, media_path.to_string_lossy()],
    )?;
    Ok((project_id, media_id))
}

pub fn get_media_path(conn: &Connection, project_id: &str) -> Result<PathBuf> {
    let path: String = conn.query_row(
        "SELECT path FROM media WHERE project_id = ?1 LIMIT 1",
        params![project_id],
        |row| row.get(0),
    )?;
    Ok(PathBuf::from(path))
}

/// Mints one fresh `speakers` row per (project-local index, display name)
/// pair, returning the index -> `speakers.id` map the caller should hold
/// for the rest of the session (pass it to `sync_speaker_names` and
/// `save_segments`). Called once, right after `create_project`.
pub fn create_speakers(conn: &Connection, names: &HashMap<i32, String>) -> Result<HashMap<i32, String>> {
    let mut ids = HashMap::with_capacity(names.len());
    for (&index, name) in names {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute("INSERT INTO speakers (id, name) VALUES (?1, ?2)", params![id, name])?;
        ids.insert(index, id);
    }
    Ok(ids)
}

/// Updates each known speaker's stored name from the current in-memory
/// names. Indices in `names` that aren't in `speaker_db_ids` (shouldn't
/// happen -- see docs/plan.md's note that new speakers can't be added
/// today) are silently skipped rather than erroring.
pub fn sync_speaker_names(
    conn: &Connection,
    speaker_db_ids: &HashMap<i32, String>,
    names: &HashMap<i32, String>,
) -> Result<()> {
    for (index, id) in speaker_db_ids {
        if let Some(name) = names.get(index) {
            conn.execute("UPDATE speakers SET name = ?1 WHERE id = ?2", params![name, id])?;
        }
    }
    Ok(())
}

/// Removes a speaker row that's no longer referenced by any segment --
/// called after a speaker merge, once every segment on the merged-away
/// speaker has been reassigned in memory (so by the time this runs and
/// `save_segments` follows, nothing points at it anymore).
pub fn delete_speaker(conn: &Connection, speaker_id: &str) -> Result<()> {
    conn.execute("DELETE FROM speakers WHERE id = ?1", params![speaker_id])?;
    Ok(())
}

/// Replaces all of `media_id`'s segments with `segments`, in one
/// transaction. `speaker_db_ids` maps each in-memory `TranscriptSegment.speaker`
/// index to the `speakers.id` minted by `create_speakers`.
pub fn save_segments(
    conn: &mut Connection,
    media_id: &str,
    segments: &[TranscriptSegment],
    speaker_db_ids: &HashMap<i32, String>,
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM segments WHERE media_id = ?1", params![media_id])?;
    for seg in segments {
        let id = uuid::Uuid::new_v4().to_string();
        let speaker_id = speaker_db_ids.get(&seg.speaker).cloned();
        tx.execute(
            "INSERT INTO segments (id, media_id, start_ms, end_ms, speaker_id, text, original_text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, media_id, seg.start_ms as i64, seg.end_ms as i64, speaker_id, seg.text, seg.text],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// `(media_id, media_path, segments, speaker_names, speaker_db_ids)` --
/// factored into an alias per clippy's `type_complexity`.
pub type LoadedProject = (String, PathBuf, Vec<TranscriptSegment>, HashMap<i32, String>, HashMap<i32, String>);

/// Reopens a project: media path, its segments (ordered by start time), and
/// freshly-assigned session-local speaker indices. The `i32` index in the
/// returned `TranscriptSegment`s and maps is *not* persisted anywhere --
/// it's reconstructed here in first-appearance order (0, 1, 2, ...) purely
/// because `TranscriptSegment.speaker` needs some `i32` to hold, and the
/// real persisted identity is the `speakers.id` string, threaded back out
/// via the returned index -> id map so this session's edits can be saved
/// back correctly.
pub fn load_project(conn: &Connection, project_id: &str) -> Result<LoadedProject> {
    let media_id: String = conn.query_row(
        "SELECT id FROM media WHERE project_id = ?1 LIMIT 1",
        params![project_id],
        |row| row.get(0),
    )?;
    let media_path = get_media_path(conn, project_id)?;

    let mut stmt = conn.prepare(
        "SELECT s.start_ms, s.end_ms, s.text, s.speaker_id, sp.name \
         FROM segments s LEFT JOIN speakers sp ON sp.id = s.speaker_id \
         WHERE s.media_id = ?1 ORDER BY s.start_ms",
    )?;
    let rows = stmt.query_map(params![media_id], |row| {
        Ok((
            row.get::<_, i64>(0)? as u64,
            row.get::<_, i64>(1)? as u64,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;

    let mut segments = Vec::new();
    let mut speaker_db_ids: HashMap<i32, String> = HashMap::new();
    let mut speaker_names: HashMap<i32, String> = HashMap::new();
    let mut seen: HashMap<String, i32> = HashMap::new();
    let mut next_index = 0i32;

    for row in rows {
        let (start_ms, end_ms, text, speaker_id, speaker_name) = row?;
        // `speaker_id` should always be `Some` in practice (every distinct
        // speaker gets a row at project-creation time, and a merge
        // reassigns segments before it's ever deleted) -- `-1` is a
        // defensive bucket for the orphaned case, not an expected path.
        let index = match &speaker_id {
            Some(id) => {
                if let Some(&ix) = seen.get(id) {
                    ix
                } else {
                    let ix = next_index;
                    next_index += 1;
                    seen.insert(id.clone(), ix);
                    ix
                }
            }
            None => -1,
        };
        if let Some(id) = speaker_id {
            speaker_db_ids.entry(index).or_insert(id);
            speaker_names.entry(index).or_insert_with(|| speaker_name.unwrap_or_default());
        }
        segments.push(TranscriptSegment { speaker: index, start_ms, end_ms, text });
    }

    Ok((media_id, media_path, segments, speaker_names, speaker_db_ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_in_memory;

    #[test]
    fn round_trips_a_project() {
        let mut conn = open_in_memory().unwrap();
        let (project_id, media_id) =
            create_project(&conn, "test project", Path::new("/tmp/clip.mp4")).unwrap();

        let mut names = HashMap::new();
        names.insert(0, "田中".to_string());
        names.insert(1, "佐藤".to_string());
        let speaker_db_ids = create_speakers(&conn, &names).unwrap();

        let segments = vec![
            TranscriptSegment { speaker: 0, start_ms: 0, end_ms: 1000, text: "こんにちは".to_string() },
            TranscriptSegment { speaker: 1, start_ms: 1000, end_ms: 2000, text: "よろしく".to_string() },
        ];
        save_segments(&mut conn, &media_id, &segments, &speaker_db_ids).unwrap();

        let summaries = list_projects(&conn).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].segment_count, 2);

        let (loaded_media_id, media_path, loaded_segments, speaker_names, loaded_db_ids) =
            load_project(&conn, &project_id).unwrap();
        assert_eq!(loaded_media_id, media_id);
        assert_eq!(media_path, Path::new("/tmp/clip.mp4"));
        assert_eq!(loaded_segments.len(), 2);
        assert_eq!(loaded_segments[0].text, "こんにちは");
        // Names round-trip even though the local index reassignment isn't
        // guaranteed to reuse the original 0/1 labeling.
        let ix0 = loaded_segments[0].speaker;
        let ix1 = loaded_segments[1].speaker;
        assert_eq!(speaker_names[&ix0], "田中");
        assert_eq!(speaker_names[&ix1], "佐藤");
        assert_ne!(loaded_db_ids[&ix0], loaded_db_ids[&ix1]);

        // Autosave path: rename a speaker, resave, reload.
        let mut renamed = names.clone();
        renamed.insert(0, "田中太郎".to_string());
        sync_speaker_names(&conn, &speaker_db_ids, &renamed).unwrap();
        let (_, _, _, speaker_names2, _) = load_project(&conn, &project_id).unwrap();
        assert!(speaker_names2.values().any(|n| n == "田中太郎"));
    }
}
