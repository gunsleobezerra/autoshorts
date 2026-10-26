//! Integration tests for the SQLite persistence layer.
//!
//! These tests exercise the real `Database` against a temporary database file,
//! so migrations, foreign keys and cascade deletes are covered end to end.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use autoshorts_lib::db::Database;
use autoshorts_lib::models::CandidateDraft;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temporary directory that removes itself (and the database inside) on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "autoshorts-tests-{}-{}-{}",
            std::process::id(),
            unique,
            label
        ));
        std::fs::create_dir_all(&path).expect("creating temp directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("autoshorts.sqlite3")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn open_db(dir: &TempDir) -> Database {
    Database::open(&dir.db_path()).expect("opening test database")
}

fn draft(index: usize) -> CandidateDraft {
    CandidateDraft {
        start: index as f64 * 10.0,
        end: index as f64 * 10.0 + 30.0,
        score: 1.0 - index as f64 * 0.05,
        hook: format!("hook {index}"),
        rationale: format!("rationale {index}"),
    }
}

fn drafts(count: usize) -> Vec<CandidateDraft> {
    (0..count).map(draft).collect()
}

fn table_count(path: &Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("opening database for assertions");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
        .expect("counting rows")
}

#[test]
fn creates_and_reads_projects() {
    let dir = TempDir::new("projects");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", Some(120.0))
        .expect("creating project");

    assert_eq!(project.source_path, "/media/input.mp4");
    assert_eq!(project.status, "ingest");
    assert_eq!(project.transcription_mode, "local");
    assert_eq!(project.caption_style.as_deref(), Some("modern-box"));
    assert_eq!(project.source_duration, Some(120.0));

    let loaded = db.get_project(&project.id).expect("loading project");
    assert_eq!(loaded.id, project.id);
    assert_eq!(loaded.source_path, project.source_path);

    let all = db.list_projects().expect("listing projects");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, project.id);
}

#[test]
fn project_detail_starts_empty_and_tracks_transcript() {
    let dir = TempDir::new("detail");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    let detail = db.project_detail(&project.id).expect("loading detail");
    assert!(detail.transcript.is_none());
    assert!(detail.candidates.is_empty());
    assert!(detail.clips.is_empty());
    assert!(detail.copy.is_empty());

    db.save_transcript(&project.id, "whisper", "{\"segments\":[]}", Some("pt"))
        .expect("saving transcript");

    let detail = db.project_detail(&project.id).expect("loading detail");
    let transcript = detail.transcript.expect("transcript should exist");
    assert_eq!(transcript.engine, "whisper");
    assert_eq!(transcript.language.as_deref(), Some("pt"));
}

#[test]
fn saving_a_transcript_replaces_the_previous_one() {
    let dir = TempDir::new("transcripts");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    db.save_transcript(&project.id, "deepgram", "{\"v\":1}", Some("en"))
        .expect("saving first transcript");
    db.save_transcript(&project.id, "whisper", "{\"v\":2}", Some("es"))
        .expect("saving second transcript");

    let latest = db
        .latest_transcript(&project.id)
        .expect("loading latest transcript")
        .expect("transcript should exist");
    assert_eq!(latest.engine, "whisper");
    assert_eq!(latest.raw_json, "{\"v\":2}");
    assert_eq!(latest.language.as_deref(), Some("es"));
    assert_eq!(table_count(&dir.db_path(), "transcripts"), 1);
}

#[test]
fn replace_candidates_ranks_and_auto_selects_six() {
    let dir = TempDir::new("candidates");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    let created = db
        .replace_candidates(&project.id, &drafts(8))
        .expect("replacing candidates");
    assert_eq!(created.len(), 8);

    let candidates = db.list_candidates(&project.id).expect("listing candidates");
    assert_eq!(candidates.len(), 8);
    for (index, candidate) in candidates.iter().enumerate() {
        assert_eq!(candidate.rank, index as i64 + 1);
        assert_eq!(candidate.selected, index < 6);
        assert_eq!(candidate.project_id, project.id);
    }

    let detail = db.project_detail(&project.id).expect("loading detail");
    assert_eq!(detail.clips.len(), 8);
    assert!(detail.clips.iter().all(|clip| clip.status == "pending"));
}

#[test]
fn replace_candidates_is_idempotent() {
    let dir = TempDir::new("replace");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    db.replace_candidates(&project.id, &drafts(4))
        .expect("first replacement");
    db.replace_candidates(&project.id, &drafts(2))
        .expect("second replacement");

    assert_eq!(db.list_candidates(&project.id).unwrap().len(), 2);
    assert_eq!(table_count(&dir.db_path(), "candidates"), 2);
    assert_eq!(table_count(&dir.db_path(), "clips"), 2);
}

#[test]
fn selected_clip_count_toggles_selection_by_rank() {
    let dir = TempDir::new("selection");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    db.replace_candidates(&project.id, &drafts(5))
        .expect("replacing candidates");

    let selected = db
        .set_selected_clip_count(&project.id, 2)
        .expect("selecting two clips");
    assert_eq!(selected.iter().filter(|candidate| candidate.selected).count(), 2);
    assert!(selected[0].selected);
    assert!(selected[1].selected);
    assert!(!selected[2].selected);

    let selected = db
        .set_selected_clip_count(&project.id, 4)
        .expect("selecting four clips");
    assert_eq!(selected.iter().filter(|candidate| candidate.selected).count(), 4);
}

#[test]
fn clip_updates_keep_existing_paths_when_none_is_passed() {
    let dir = TempDir::new("clips");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");
    let candidate = db
        .replace_candidates(&project.id, &drafts(1))
        .expect("replacing candidates")
        .remove(0);

    db.update_clip_for_candidate(
        &candidate.id,
        "rendered",
        Some("/out/clip.mp4"),
        Some("/out/clip.ass"),
        None,
    )
    .expect("updating clip");

    db.update_clip_for_candidate(&candidate.id, "failed", None, None, Some("ffmpeg died"))
        .expect("updating clip status");

    let detail = db.project_detail(&project.id).expect("loading detail");
    let clip = &detail.clips[0];
    assert_eq!(clip.status, "failed");
    assert_eq!(clip.output_path.as_deref(), Some("/out/clip.mp4"));
    assert_eq!(clip.caption_ass_path.as_deref(), Some("/out/clip.ass"));
    assert_eq!(clip.render_log.as_deref(), Some("ffmpeg died"));
}

#[test]
fn reopening_the_database_resets_interrupted_cutting_clips() {
    let dir = TempDir::new("reopen");
    let project_id;
    let candidate_id;

    {
        let db = open_db(&dir);
        let project = db
            .create_project("/media/input.mp4", "local", "modern-box", None)
            .expect("creating project");
        let candidate = db
            .replace_candidates(&project.id, &drafts(1))
            .expect("replacing candidates")
            .remove(0);

        db.update_clip_for_candidate(&candidate.id, "cutting", None, None, None)
            .expect("marking clip as cutting");

        project_id = project.id;
        candidate_id = candidate.id;
    }

    let db = open_db(&dir);
    let detail = db.project_detail(&project_id).expect("loading detail");
    assert_eq!(detail.clips.len(), 1);
    assert_eq!(detail.clips[0].candidate_id, candidate_id);
    assert_eq!(detail.clips[0].status, "pending");
}

#[test]
fn get_candidate_with_project_joins_both_rows() {
    let dir = TempDir::new("join");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", Some(90.0))
        .expect("creating project");
    let candidate = db
        .replace_candidates(&project.id, &drafts(1))
        .expect("replacing candidates")
        .remove(0);

    let (loaded_candidate, loaded_project) = db
        .get_candidate_with_project(&candidate.id)
        .expect("loading candidate with project");
    assert_eq!(loaded_candidate.id, candidate.id);
    assert_eq!(loaded_project.id, project.id);
    assert_eq!(loaded_project.source_duration, Some(90.0));
}

#[test]
fn rename_and_status_updates_are_persisted() {
    let dir = TempDir::new("updates");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");

    db.rename_project(&project.id, "My Episode")
        .expect("renaming project");
    db.update_project_status(&project.id, "transcribing", Some(300.5))
        .expect("updating status");

    let loaded = db.get_project(&project.id).expect("loading project");
    assert_eq!(loaded.name.as_deref(), Some("My Episode"));
    assert_eq!(loaded.status, "transcribing");
    assert_eq!(loaded.source_duration, Some(300.5));

    db.update_project_status(&project.id, "ready", None)
        .expect("updating status without duration");
    let loaded = db.get_project(&project.id).expect("loading project");
    assert_eq!(loaded.status, "ready");
    assert_eq!(loaded.source_duration, Some(300.5));
}

#[test]
fn delete_project_cascades_candidates_clips_and_transcripts() {
    let dir = TempDir::new("delete");
    let db = open_db(&dir);

    let project = db
        .create_project("/media/input.mp4", "local", "modern-box", None)
        .expect("creating project");
    db.save_transcript(&project.id, "whisper", "{}", None)
        .expect("saving transcript");
    db.replace_candidates(&project.id, &drafts(3))
        .expect("replacing candidates");

    db.delete_project(&project.id).expect("deleting project");

    assert!(db.get_project(&project.id).is_err());
    assert!(db.list_projects().unwrap().is_empty());
    assert!(db.latest_transcript(&project.id).unwrap().is_none());
    assert!(db.list_candidates(&project.id).unwrap().is_empty());

    let path = dir.db_path();
    assert_eq!(table_count(&path, "projects"), 0);
    assert_eq!(table_count(&path, "transcripts"), 0);
    assert_eq!(table_count(&path, "candidates"), 0);
    assert_eq!(table_count(&path, "clips"), 0);
}

#[test]
fn opening_the_same_database_twice_reuses_the_schema() {
    let dir = TempDir::new("schema");
    let project_id = {
        let db = open_db(&dir);
        db.create_project("/media/input.mp4", "local", "modern-box", None)
            .expect("creating project")
            .id
    };

    let db = open_db(&dir);
    let projects = db.list_projects().expect("listing projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, project_id);
}
