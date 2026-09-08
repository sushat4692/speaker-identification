//! Cross-recording speaker identification (enrollment).
//!
//! `sherpa_onnx::SpeakerEmbeddingManager` is in-memory only — it does not
//! persist anything on its own. Per the plan, we load it from SQLite at
//! startup and write through on every enrollment change, so a speaker
//! enrolled in one recording is recognized in the next.

use rusqlite::Connection;
use sherpa_onnx::SpeakerEmbeddingManager;

use crate::error::{CoreError, Result};

pub struct SpeakerRegistry {
    manager: SpeakerEmbeddingManager,
    embedding_dim: i32,
}

/// A name recognized (or not) against the registry, with the match score
/// when one was found.
#[derive(Debug, Clone)]
pub struct SpeakerMatch {
    pub name: Option<String>,
    pub score: Option<f32>,
}

impl SpeakerRegistry {
    /// Create an empty registry for the given embedding dimension (must
    /// match `SpeakerEmbeddingExtractor::dim()` for whichever embedding
    /// model produced the vectors you'll feed in).
    pub fn new(embedding_dim: i32) -> Result<Self> {
        let manager = SpeakerEmbeddingManager::create(embedding_dim).ok_or_else(|| {
            CoreError::Other(anyhow::anyhow!("failed to create SpeakerEmbeddingManager"))
        })?;
        Ok(Self { manager, embedding_dim })
    }

    /// Load all previously enrolled speakers from the `speakers` /
    /// `speaker_embeddings` tables (see the plan's data model) into a fresh
    /// in-memory manager.
    pub fn load_from_db(conn: &Connection, embedding_dim: i32) -> Result<Self> {
        let registry = Self::new(embedding_dim)?;

        let mut stmt = conn.prepare(
            "SELECT s.name, e.vector \
             FROM speaker_embeddings e \
             JOIN speakers s ON s.id = e.speaker_id \
             WHERE s.is_enrolled = 1",
        )?;

        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((name, blob))
        })?;

        for row in rows {
            let (name, blob) = row?;
            let vector = bytes_to_f32(&blob);
            if vector.len() as i32 != embedding_dim {
                tracing::warn!(
                    name,
                    got = vector.len(),
                    expected = embedding_dim,
                    "skipping embedding with mismatched dimension (model changed?)"
                );
                continue;
            }
            registry.manager.add(&name, &vector);
        }

        Ok(registry)
    }

    /// Search for the closest enrolled speaker. Returns `None` for the name
    /// when nothing clears `threshold` — the caller should present the
    /// segment as an unnamed `話者N` and let the user enroll it, per the
    /// plan's UI design.
    pub fn identify(&self, embedding: &[f32], threshold: f32) -> SpeakerMatch {
        match self.manager.search(embedding, threshold) {
            Some(name) => {
                // The manager's `search` doesn't return the score alongside
                // the name; `get_best_matches` does. Re-query for it so
                // callers can show confidence in the UI.
                let best = self
                    .manager
                    .get_best_matches(embedding, threshold, 1i32)
                    .into_iter()
                    .next();
                SpeakerMatch { name: Some(name), score: best.map(|m| m.score) }
            }
            None => SpeakerMatch { name: None, score: None },
        }
    }

    /// Enroll (or add another sample for) a named speaker, and persist the
    /// sample immediately so it survives past this process.
    pub fn enroll(&self, conn: &Connection, speaker_id: &str, name: &str, embedding: &[f32]) -> Result<()> {
        if embedding.len() as i32 != self.embedding_dim {
            return Err(CoreError::Other(anyhow::anyhow!(
                "embedding dimension {} does not match registry dimension {}",
                embedding.len(),
                self.embedding_dim
            )));
        }

        conn.execute(
            "INSERT INTO speakers (id, name, is_enrolled) VALUES (?1, ?2, 1) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, is_enrolled = 1",
            rusqlite::params![speaker_id, name],
        )?;

        let embedding_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO speaker_embeddings (id, speaker_id, vector, source_media_id) \
             VALUES (?1, ?2, ?3, NULL)",
            rusqlite::params![embedding_id, speaker_id, f32_to_bytes(embedding)],
        )?;

        // Keep the in-memory manager in sync so this process's subsequent
        // `identify()` calls see the new sample without a reload.
        self.manager.add(name, embedding);

        Ok(())
    }

    pub fn embedding_dim(&self) -> i32 {
        self.embedding_dim
    }
}

fn f32_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
