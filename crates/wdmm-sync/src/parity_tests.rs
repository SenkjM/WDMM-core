// Parity tests — wdmm-sync-parity-spec.md Chunk B.
// Included via include!("parity_tests.rs") inside mod tests in lib.rs.
//
// Music: 2 (rev win), 3B (tomb loses), 4 (resurrect), 5A/B (cursor),
//        6 (zero-byte), 8 (rev clock gap pin).
// Manga: 10A (images pages roundtrip), 10B (images tomb id key),
//        11 (container tomb source\0path), 12B (local wins), 12C (exact tie),
//        13 (legacy loose-image skip), 14 (full rebuild clears tombs + cursor).

// ── Music ─────────────────────────────────────────────────────────────────────

/// Scenario 2: cloud rev=80 wins; B's stale rev=30 is not uploaded.
#[tokio::test]
async fn library_sync_higher_rev_wins() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/contested.mp3";
    let id = wdmm_core::identity::track_identity(path);

    // Seed remote with track rev=50.
    let db_seed = tmp_db();
    db_seed
        .insert_track(&LibraryTrack {
            id: id.clone(),
            title: "Seed".into(),
            remote_path: path.into(),
            account_id: "nas".into(),
            rev: 50,
            ..Default::default()
        })
        .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_seed), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Device A: same key, rev=80 (newer than cloud rev=50).
    let db_a = tmp_db();
    db_a.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "A-newer".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 80,
        ..Default::default()
    })
    .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let seg_count_after_a = driver
        .files
        .lock()
        .await
        .keys()
        .filter(|k| k.contains("/sync/library/music/seg-"))
        .count();

    // Device B: same key, rev=30 (stale) — must adopt cloud rev=80, not upload.
    let db_b = tmp_db();
    db_b.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "B-stale".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 30,
        ..Default::default()
    })
    .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let t = db_b.get_track(&id).unwrap().unwrap();
    assert_eq!(t.rev, 80, "cloud rev=80 should win over B's stale rev=30");

    let seg_count_after_b = driver
        .files
        .lock()
        .await
        .keys()
        .filter(|k| k.contains("/sync/library/music/seg-"))
        .count();
    assert_eq!(
        seg_count_after_a, seg_count_after_b,
        "B must not upload a new seg for its stale rev=30"
    );
}

/// Scenario 3B: tomb rev=40 < live rev=50 → B's track survives.
#[tokio::test]
async fn library_sync_tombstone_loses_to_live_rev() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/survive.mp3";
    let id = wdmm_core::identity::track_identity(path);

    // Publish a stale tomb (rev=40) while cloud is empty so the resurrect
    // check cannot fire (no live local or adopted track to compare against).
    let db_del = tmp_db();
    db_del.upsert_deleted_track(&wdmm_storage::DeletedTrack {
        source_name: "nas".into(),
        remote_path: path.into(),
        rev: 40,
        deleted_at: "2026-10-26T01:00:00Z".into(),
        pushed: 0,
    })
    .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_del), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();
    {
        let files = driver.files.lock().await;
        assert!(
            files.keys().any(|k| k.contains("/sync/library/music/del-")),
            "stale tomb should be uploaded to cloud"
        );
    }

    // Push live track rev=50 — supersedes the stale tomb in the manifest.
    let db_a = tmp_db();
    db_a.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "Survivor".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 50,
        ..Default::default()
    })
    .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Device B syncs: merge sees tomb.rev=40 < live.rev=50 → track survives.
    let db_b = tmp_db();
    LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let track = db_b.get_track(&id).unwrap();
    assert!(track.is_some(), "track should exist on B; stale tomb must not win");
    assert_eq!(
        track.unwrap().rev, 50,
        "B must see rev=50; stale tomb rev=40 loses to live rev=50"
    );
}

/// Scenario 4: live.rev=50 > pending tomb rev=40 → resurrect suppresses tomb.
#[tokio::test]
async fn library_sync_resurrect_suppresses_stale_tomb() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/resilient.mp3";
    let id = wdmm_core::identity::track_identity(path);

    let db_a = tmp_db();
    // Live track rev=50.
    db_a.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "Resilient".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 50,
        ..Default::default()
    })
    .unwrap();
    // Stale pending tomb rev=40 (live.rev > tomb.rev → resurrect path).
    db_a.upsert_deleted_track(&wdmm_storage::DeletedTrack {
        source_name: "nas".into(),
        remote_path: path.into(),
        rev: 40,
        deleted_at: "2026-10-26T02:00:00Z".into(),
        pushed: 0,
    })
    .unwrap();

    LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let files = driver.files.lock().await;
    assert!(
        !files.keys().any(|k| k.contains("/sync/library/music/del-")),
        "stale tomb must NOT be published when live.rev=50 > tomb.rev=40"
    );
    assert!(
        files.keys().any(|k| k.contains("/sync/library/music/seg-")),
        "live track rev=50 should be uploaded as a segment"
    );
    drop(files);

    // Tomb marked pushed → no longer in the unpushed list.
    let pending = db_a.list_unpushed_deleted_tracks().unwrap();
    assert!(pending.is_empty(), "resurrect must mark the stale tomb as pushed");
}

/// Scenario 5A: second incremental with the same manifest does not re-pull.
#[tokio::test]
async fn library_sync_cursor_skips_consumed_parts() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/cursor-track.mp3";
    let id = wdmm_core::identity::track_identity(path);

    // A pushes one track.
    let db_a = tmp_db();
    db_a.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "CursorTrack".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 100,
        ..Default::default()
    })
    .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // B first sync → pulls track, cursor saved with parts[seg]=100.
    let db_b = tmp_db();
    let engine_b = LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None);
    let out1 = engine_b.sync_incremental().await.unwrap();
    assert!(
        out1.iter().any(|o| o.action == SyncAction::Pulled),
        "B should pull on first sync"
    );

    // B second sync, same manifest → cursor skips the already-consumed seg.
    let out2 = engine_b.sync_incremental().await.unwrap();
    let pulled2 = out2.iter().filter(|o| o.action == SyncAction::Pulled).count();
    assert_eq!(pulled2, 0, "cursor must skip the already-consumed seg on re-sync");
}

/// Scenario 5B: sync_full bumps base.upTo → B re-reads base shards.
#[tokio::test]
async fn library_sync_base_up_to_clears_cursor() {
    let driver = Arc::new(MockDriver::new());
    let path1 = "/music/base-track1.mp3";
    let id1 = wdmm_core::identity::track_identity(path1);
    let path2 = "/music/base-track2.mp3";
    let id2 = wdmm_core::identity::track_identity(path2);

    // A pushes track1 (rev=10); B pulls it (cursor.base_up_to=0, parts={seg}).
    let db_a = tmp_db();
    db_a.insert_track(&LibraryTrack {
        id: id1.clone(),
        title: "T1".into(),
        remote_path: path1.into(),
        account_id: "nas".into(),
        rev: 10,
        ..Default::default()
    })
    .unwrap();
    let engine_a = LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None);
    engine_a.sync_incremental().await.unwrap();

    let db_b = tmp_db();
    let engine_b = LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None);
    engine_b.sync_incremental().await.unwrap();
    assert!(db_b.get_track(&id1).unwrap().is_some(), "B should have track1 from first pull");

    // A adds track2 (rev=20), runs sync_full → base.upTo=20, new lib-* shards.
    db_a.insert_track(&LibraryTrack {
        id: id2.clone(),
        title: "T2".into(),
        remote_path: path2.into(),
        account_id: "nas".into(),
        rev: 20,
        ..Default::default()
    })
    .unwrap();
    engine_a.sync_full().await.unwrap();

    // B syncs: base.upTo=20 > cursor.base_up_to=0 → re-reads base shards → adopts track2.
    let out3 = engine_b.sync_incremental().await.unwrap();
    let pulled3 = out3.iter().filter(|o| o.action == SyncAction::Pulled).count();
    assert!(
        pulled3 >= 1,
        "B must re-pull from base shards after sync_full bumped base.upTo"
    );
    assert!(db_b.get_track(&id2).unwrap().is_some(), "B should have track2 from base rebuild");
}

/// Scenario 6: nothing to upload → cursor saved, no new driver files written.
#[tokio::test]
async fn library_sync_zero_byte_pass_saves_cursor() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/synced.mp3";
    let id = wdmm_core::identity::track_identity(path);

    // Setup: A pushes, B pulls → fully in sync.
    let db_a = tmp_db();
    db_a.insert_track(&LibraryTrack {
        id: id.clone(),
        title: "Synced".into(),
        remote_path: path.into(),
        account_id: "nas".into(),
        rev: 10,
        ..Default::default()
    })
    .unwrap();
    let engine_a = LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None);
    engine_a.sync_incremental().await.unwrap();

    let db_b = tmp_db();
    LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Zero-byte pass: A has nothing new to push.
    let file_count_before = driver.files.lock().await.len();
    engine_a.sync_incremental().await.unwrap();
    let file_count_after = driver.files.lock().await.len();

    assert_eq!(
        file_count_before, file_count_after,
        "zero-byte pass must not write new files to the driver"
    );

    // Cursor still persisted after the no-op pass.
    let state = db_a.get_sync_state("library:/sync").unwrap();
    assert!(state.is_some(), "cursor must be saved even on a zero-byte pass");
}

/// Scenario 8: adopted cloud rev=9e9; wall-clock nowMs naturally > 9e9.
/// Documents that Core has no observe_rev call post-adoption (spec scenario 8 gap pin).
#[tokio::test]
async fn library_sync_rev_clock_gap_pin() {
    let driver = Arc::new(MockDriver::new());
    let path = "/music/big-rev.mp3";
    let id = wdmm_core::identity::track_identity(path);
    let big_rev: i64 = 9_000_000_000;

    // Seed remote with a track carrying a very large rev.
    let db_seed = tmp_db();
    db_seed
        .insert_track(&LibraryTrack {
            id: id.clone(),
            title: "BigRev".into(),
            remote_path: path.into(),
            account_id: "nas".into(),
            rev: big_rev,
            ..Default::default()
        })
        .unwrap();
    LibrarySyncEngine::new(Arc::clone(&db_seed), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // B adopts the cloud track.
    let db_b = tmp_db();
    LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let t = db_b.get_track(&id).unwrap().unwrap();
    assert_eq!(t.rev, big_rev, "cloud track with rev=9e9 must be adopted intact");

    // Wall-clock epoch-ms exceeds 9e9 → new local tracks naturally get higher revs.
    // (Core currently has no observe_rev call post-adoption; this assertion documents
    //  the gap: correctness holds only because nowMs >> 9e9 at current wall-clock scale.)
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        now_ms > big_rev,
        "wall-clock epoch-ms ({now_ms}) must exceed 9e9; new local tracks get higher revs naturally"
    );
}

// ── Manga ─────────────────────────────────────────────────────────────────────

/// Scenario 10A: non-legacy images manga with pages roundtrips through tag-18 codec.
#[tokio::test]
async fn manga_sync_images_pages_roundtrip() {
    use wdmm_storage::{manga_id_for_remote, MangaPage};
    let driver = Arc::new(MockDriver::new());
    let source = "nas";
    let path = "/m/ch1";

    // id ≠ manga_id_for_remote → NOT a legacy row; adopt path reads pages.
    let custom_id = "img-001".to_string();
    assert_ne!(custom_id, manga_id_for_remote(source, path));

    let m = wdmm_storage::Manga {
        id: custom_id.clone(),
        source_name: source.into(),
        remote_path: path.into(),
        title: "Images Ch1".into(),
        format: "images".into(),
        rev: 5,
        page_count: 2,
        author: None, series: None, volume: None, chapter_number: None,
        cover_relpath: None, remote_missing: 0, page_turn: None,
        updated_at: "2026-10-26T00:00:00Z".into(),
    };
    let db_a = tmp_db();
    db_a.upsert_manga(&m).unwrap();
    db_a.replace_pages(
        &custom_id,
        &[
            MangaPage { manga_id: custom_id.clone(), page_index: 0, rel_path: "001.jpg".into() },
            MangaPage { manga_id: custom_id.clone(), page_index: 1, rel_path: "002.jpg".into() },
        ],
    )
    .unwrap();

    MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let db_b = tmp_db();
    MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    assert_eq!(db_b.list_mangas().unwrap().len(), 1, "images manga should be adopted by B");
    let pages = db_b.list_pages(&custom_id).unwrap();
    let rel: Vec<&str> = pages.iter().map(|p| p.rel_path.as_str()).collect();
    assert_eq!(rel, vec!["001.jpg", "002.jpg"], "pages must roundtrip via tag-18 JSON codec");
}

/// Scenario 10B: images manga deletion → tomb sync_key == manga id (not source\0path).
#[tokio::test]
async fn manga_sync_images_tomb_uses_id_as_sync_key() {
    let driver = Arc::new(MockDriver::new());
    let source = "nas";
    let path = "/m/ch1-tomb";
    let custom_id = "img-002".to_string();

    let m = wdmm_storage::Manga {
        id: custom_id.clone(),
        source_name: source.into(),
        remote_path: path.into(),
        title: "Images Ch1 Tomb".into(),
        format: "images".into(),
        rev: 5,
        page_count: 1,
        author: None, series: None, volume: None, chapter_number: None,
        cover_relpath: None, remote_missing: 0, page_turn: None,
        updated_at: "2026-10-26T00:00:00Z".into(),
    };

    // A pushes the manga; B pulls it.
    let db_a = tmp_db();
    db_a.upsert_manga(&m).unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    let db_b = tmp_db();
    MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();
    assert_eq!(db_b.list_mangas().unwrap().len(), 1);

    // A deletes: manga_sync_key("img-002","images",...) = "img-002" (images → id).
    db_a.delete_manga(&custom_id).unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();
    {
        let files = driver.files.lock().await;
        assert!(
            files.keys().any(|k| k.contains("/sync/library/manga/del-")),
            "images tomb del-*.wdmm must be published"
        );
    }

    // B adopts deletion: tomb sync_key="img-002" matches B's manga id.
    MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();
    assert_eq!(
        db_b.list_mangas().unwrap().len(), 0,
        "images manga must be deleted on B via tomb sync_key = id"
    );
}

/// Scenario 11: container (cbz) tomb uses sync_key = source\0path.
#[tokio::test]
async fn manga_sync_container_tomb_uses_source_path_key() {
    let driver = Arc::new(MockDriver::new());
    let source = "nas";
    let path = "/m/vol2.cbz";

    // A has the cbz manga, deletes it → tomb sync_key = "nas\0/m/vol2.cbz".
    let db_a = tmp_db();
    let m_a = mk_manga(99, source, path, "cbz", 8);
    db_a.upsert_manga(&m_a).unwrap();
    db_a.delete_manga(&m_a.id).unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();
    {
        let files = driver.files.lock().await;
        assert!(
            files.keys().any(|k| k.contains("/sync/library/manga/del-")),
            "container cbz tomb must be published"
        );
    }

    // B has the same cbz (same source+path → same sync_key, rev=8 too).
    let db_b = tmp_db();
    let m_b = mk_manga(99, source, path, "cbz", 8);
    db_b.upsert_manga(&m_b).unwrap();
    assert_eq!(db_b.list_mangas().unwrap().len(), 1);

    // B syncs: cloud tomb_rev=8 >= local.rev=8 → delete.
    MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    assert_eq!(
        db_b.list_mangas().unwrap().len(), 0,
        "container cbz must be deleted via tomb sync_key = source\\0path"
    );
}

/// Scenario 12B: local progress (page=7, T+2h) wins over cloud (page=3, T).
#[tokio::test]
async fn manga_progress_local_wins() {
    let driver = Arc::new(MockDriver::new());
    let m = mk_manga(1, "nas", "/m/prog.cbz", "cbz", 1);
    let mid = m.id.clone();

    // Seed cloud with progress {page=3, T}.
    let db_seed = tmp_db();
    db_seed.upsert_manga(&m).unwrap();
    db_seed.upsert_progress(&wdmm_storage::MangaProgress {
        manga_id: mid.clone(),
        page_index: 3,
        updated_at: "2026-10-25T10:00:00Z".into(),
    })
    .unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_seed), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Local: page=7, T+2h (newer timestamp) → local must win.
    let db = tmp_db();
    db.upsert_manga(&m).unwrap();
    db.upsert_progress(&wdmm_storage::MangaProgress {
        manga_id: mid.clone(),
        page_index: 7,
        updated_at: "2026-10-25T12:00:00Z".into(),
    })
    .unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Local DB page_index unchanged — local was not overwritten by cloud.
    let p = db.get_progress(&mid).unwrap().unwrap();
    assert_eq!(p.page_index, 7, "local page=7 (newer timestamp) must win over cloud page=3");

    // progress.json reflects local's value (re-uploaded).
    // Format: ProgressDoc { entries: [{ mangaId, page, updatedAt }] }
    #[derive(serde::Deserialize)]
    struct PEntry {
        #[serde(rename = "mangaId")]
        manga_id: String,
        page: i64,
    }
    #[derive(serde::Deserialize)]
    struct PDoc {
        entries: Vec<PEntry>,
    }
    let prog_bytes = driver
        .get_content("/sync/library/manga/progress.json")
        .await
        .unwrap();
    let doc: PDoc = serde_json::from_slice(&prog_bytes).unwrap();
    let ours = doc.entries.iter().find(|e| e.manga_id == mid).expect("progress.json has our manga");
    assert_eq!(ours.page, 7, "progress.json must reflect local's page=7 after local win");
}

/// Scenario 12C: exact timestamp tie → no DB overwrite, progress.json content unchanged.
#[tokio::test]
async fn manga_progress_exact_tie_no_upload() {
    let driver = Arc::new(MockDriver::new());
    let m = mk_manga(1, "nas", "/m/tie.cbz", "cbz", 1);
    let mid = m.id.clone();
    let ts = "2026-10-25T10:00:00Z";

    // Seed cloud with progress {page=3, T}.
    let db_seed = tmp_db();
    db_seed.upsert_manga(&m).unwrap();
    db_seed.upsert_progress(&wdmm_storage::MangaProgress {
        manga_id: mid.clone(),
        page_index: 3,
        updated_at: ts.into(),
    })
    .unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_seed), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // Local: exact same {page=3, T} — strict-`>` tie → neither apply nor upload.
    let db = tmp_db();
    db.upsert_manga(&m).unwrap();
    db.upsert_progress(&wdmm_storage::MangaProgress {
        manga_id: mid.clone(),
        page_index: 3,
        updated_at: ts.into(),
    })
    .unwrap();

    let prog_before = driver
        .get_content("/sync/library/manga/progress.json")
        .await
        .unwrap();

    MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // DB not overwritten: page_index and updated_at unchanged.
    let p = db.get_progress(&mid).unwrap().unwrap();
    assert_eq!(p.page_index, 3, "tie must not change local page_index");
    assert_eq!(p.updated_at, ts, "tie must not change local updated_at");

    // Cloud progress.json content unchanged (no re-upload on tie).
    // Note: bytes-equal cannot distinguish "no upload" from "upload same bytes";
    // MockDriver has no per-path write counter. Combined with 12B (local wins+uploads)
    // this confirms the tie branch neither applies nor re-publishes.
    let prog_after = driver
        .get_content("/sync/library/manga/progress.json")
        .await
        .unwrap();
    assert_eq!(prog_before, prog_after, "exact tie must not change progress.json content");
}

/// Scenario 13: legacy loose-image row (id == manga_id_for_remote) skipped on adopt.
#[tokio::test]
async fn manga_sync_legacy_loose_image_skip() {
    use wdmm_storage::manga_id_for_remote;
    let driver = Arc::new(MockDriver::new());
    let source = "nas";
    let path = "/m/legacy";

    // A: legacy row — id == manga_id_for_remote(source, path).
    // mk_manga sets id = manga_id_for_remote, so this is already legacy.
    let db_a = tmp_db();
    let legacy = mk_manga(1, source, path, "images", 3);
    assert_eq!(legacy.id, manga_id_for_remote(source, path), "mk_manga must produce a legacy row");
    db_a.upsert_manga(&legacy).unwrap();
    MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    // B: empty DB — syncs and must NOT adopt the legacy row.
    let db_b = tmp_db();
    MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
        .sync_incremental()
        .await
        .unwrap();

    assert!(
        db_b.list_mangas().unwrap().is_empty(),
        "legacy loose-image row (id == manga_id_for_remote) must be skipped on adopt"
    );
}

/// Scenario 14: sync_full clears all pending deleted_mangas and the manga cursor.
#[tokio::test]
async fn manga_sync_full_rebuild_clears_tombs_and_cursor() {
    let driver = Arc::new(MockDriver::new());
    let db = tmp_db();

    // Seed three mangas.
    for i in 1u32..=3 {
        db.upsert_manga(&mk_manga(i, "nas", &format!("/m/{i}.cbz"), "cbz", i as i64))
            .unwrap();
    }
    let engine = MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None);
    engine.sync_incremental().await.unwrap();

    // Insert an unpushed deletion to confirm sync_full wipes it.
    let extra = mk_manga(99, "nas", "/m/extra.cbz", "cbz", 99);
    db.upsert_manga(&extra).unwrap();
    db.delete_manga(&extra.id).unwrap();
    assert!(
        !db.list_unpushed_deleted_mangas().unwrap().is_empty(),
        "pre-condition: pending deletion must exist before sync_full"
    );

    // Cursor must exist after the incremental sync.
    assert!(
        db.get_sync_state("manga:/sync").unwrap().is_some(),
        "pre-condition: manga cursor must exist before sync_full"
    );

    engine.sync_full().await.unwrap();

    // sync_full calls clear_all_deleted_mangas.
    assert!(
        db.list_unpushed_deleted_mangas().unwrap().is_empty(),
        "sync_full must clear all pending deleted_mangas"
    );

    // sync_full calls clear_sync_state(Some(\"manga:\")).
    assert!(
        db.get_sync_state("manga:/sync").unwrap().is_none(),
        "sync_full must clear the manga cursor state"
    );
}
