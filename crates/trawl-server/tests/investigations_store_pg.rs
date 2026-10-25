// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres tests for the investigation store and the kept-evidence side of
//! the captured-result store (ADR-0059).
//!
//! Plain `#[sqlx::test]` applies `crates/trawl-server/migrations/` to each
//! per-test database. Every instant is a parameter from a fixed `T0`:
//! nothing here depends on the wall clock. Races are staged with a held
//! transaction and observed through `pg_stat_activity`, never retried.

use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use sqlx::{Connection as _, PgPool};
use tokio::sync::Barrier;
use trawl_api::ErrorCode;
use trawl_api::investigations::{
    Actor, CreateInvestigation, EntryPayload, EvidenceRef, FindingStatus, InvestigationId,
    InvestigationStatus, StepOutcome, StepOutcomeKind,
};
use trawl_api::results::{
    ColumnFacts, ResultColumn, ResultShape, ResultSource, ResultState, StoredAs,
};
use trawl_api::value::ColumnType;
use trawl_server::deadline::Deadline;
use trawl_server::store::{
    CaptureInsert, Citing, CreateOutcome, DeleteOutcome, InvestigationStore, KeptInsert, KeptStep,
    NewCapture, NewInvestigation, Overwrite, ResultsStore, RetentionRule, StoreActor, StoreError,
    Usage,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 24, 9, 0, 0).unwrap()
}

const RULE: RetentionRule = RetentionRule {
    idle: Duration::days(7),
};

/// How long a write that admits work waits to commit: far past any wait
/// these tests do not mean to bound.
fn patience() -> Deadline {
    Deadline::after(std::time::Duration::from_mins(1))
}

fn inv(n: u32) -> InvestigationId {
    InvestigationId::parse(&format!("{n:032x}")).unwrap()
}

fn result_id(n: u32) -> String {
    format!("{:032x}", 0xabc0_0000_u32 + n)
}

fn agent() -> StoreActor {
    StoreActor {
        key_id: 7,
        prefix: "AbCd_-12".into(),
        name: "agent".into(),
    }
}

fn human() -> StoreActor {
    StoreActor {
        key_id: 3,
        prefix: "Zy9x8w7v".into(),
        name: "jakub".into(),
    }
}

fn creation(title: &str) -> CreateInvestigation {
    CreateInvestigation {
        title: title.into(),
        question: "Why does checkout return 502?".into(),
        scope: "checkout, last day".into(),
    }
}

/// An investigation `n` created by `actor` at `T0`, with the default
/// clocks: evidence 7 days, ceiling 30.
fn new_investigation(n: u32, actor: StoreActor, title: &str) -> NewInvestigation {
    NewInvestigation {
        id: inv(n),
        actor,
        creation: creation(title),
        now: t0(),
        retain_until: t0() + Duration::days(7),
        retain_ceiling: t0() + Duration::days(30),
    }
}

async fn create(store: &InvestigationStore, n: u32) -> InvestigationId {
    let outcome = store
        .create_if_absent(&new_investigation(n, agent(), "502s"))
        .await
        .unwrap();
    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");
    inv(n)
}

fn note(text: &str) -> EntryPayload {
    EntryPayload::Note { text: text.into() }
}

fn step(request_id: &str) -> EntryPayload {
    EntryPayload::Step {
        executed: "_service=checkout status>=500".into(),
        bounds: None,
        request_id: request_id.into(),
        query_id: Some(4),
        purpose: Some("find the first 502".into()),
        outcome: None,
    }
}

fn success(result: &str) -> StepOutcome {
    StepOutcome {
        outcome: StepOutcomeKind::Success,
        code: None,
        result_id: Some(result.into()),
        rows: Some(12),
        duration_ms: Some(250),
    }
}

fn finding() -> EntryPayload {
    EntryPayload::Finding {
        statement: "the upstream pool is exhausted".into(),
        status: FindingStatus::Hypothesis,
        references: vec![EvidenceRef {
            result_id: result_id(1),
            row: 3,
        }],
        counterevidence: vec![],
        remaining_question: None,
    }
}

async fn head_seq(store: &InvestigationStore, id: &InvestigationId) -> u64 {
    store.get(id).await.unwrap().expect("the investigation").seq
}

/// A live temporary capture `n` of `key_id`, made `age` before `T0` and
/// expiring 15 minutes after it was made.
fn capture(n: u32, key_id: i64, age: Duration, file_bytes: u64) -> NewCapture {
    let created_at = t0() - age;
    NewCapture {
        id: result_id(n),
        key_id,
        created_at,
        expires_at: created_at + Duration::minutes(15),
        query: "_service=checkout".into(),
        executed_query: "_service=checkout | sort -_time, -_ingested".into(),
        source: ResultSource::Live,
        shape: ResultShape::Events,
        earliest: Some(created_at - Duration::hours(1)),
        latest: Some(created_at),
        columns: vec![ResultColumn {
            name: "_time".into(),
            ty: ColumnType::Timestamp,
            stored_as: StoredAs::Native,
        }],
        column_facts: vec![ColumnFacts {
            nulls: 0,
            distinct: 0,
            distinct_capped: false,
            values: None,
            min: None,
            max: None,
        }],
        captured_rows: 0,
        total_rows: 0,
        wire_bytes: 0,
        file_bytes,
        digest: (file_bytes > 0).then_some([1; 32]),
        format_version: 1,
        investigation_id: None,
    }
}

/// Record `row`: temporary, or kept inside its investigation's lock with
/// no step, the fixture form of an attached capture's kept insert.
async fn insert(results: &ResultsStore, row: &NewCapture) {
    let inserted = if row.investigation_id.is_some() {
        results.insert_kept_unlogged(row).await
    } else {
        results.insert(row).await
    };
    assert_eq!(inserted.unwrap(), CaptureInsert::Inserted);
}

async fn state_of(
    pool: &PgPool,
    id: &str,
) -> (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT state, expires_at, gone_at FROM captured_results WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Wait, bounded, until `count` sessions of this database wait on a lock
/// in the investigation row lock every write takes. A second writer queues
/// behind the first waiter rather than the holder, so this counts lock
/// waits on the statement, not the holder's blocked pids.
async fn writers_waiting(probe: &PgPool, count: i64) {
    lock_waits(probe, "SELECT status, seq FROM investigations%", count).await;
}

/// Wait, bounded, until `count` evidence prunes wait to lock the due
/// investigations.
async fn prunes_waiting(probe: &PgPool, count: i64) {
    lock_waits(
        probe,
        "SELECT id FROM investigations WHERE retain_until%",
        count,
    )
    .await;
}

/// Wait, bounded, until `count` sessions of this database wait on a lock
/// in a statement that starts like `pattern`.
async fn lock_waits(probe: &PgPool, pattern: &str, count: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE $1",
            )
            .bind(pattern)
            .fetch_one(probe)
            .await
            .unwrap();
            if waiting == count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the statements queue behind the held lock");
}

// -- the schema --------------------------------------------------------------

/// The dormant BIGINT column is now the investigation id, with a foreign
/// key to the investigations. A kept row that names no investigation is
/// refused: the plain insert records temporary rows only, and the kept
/// insert finds no investigation to lock.
#[sqlx::test]
async fn the_attachment_column_is_an_investigation_id_with_a_foreign_key(pool: PgPool) {
    let ty: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
         WHERE table_name = 'captured_results' AND column_name = 'investigation_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ty, "text");
    let referenced: Vec<String> = sqlx::query_scalar(
        "SELECT confrelid::regclass::text FROM pg_constraint
         WHERE conrelid = 'captured_results'::regclass AND contype = 'f'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(referenced, ["investigations"]);

    let results = ResultsStore::new(pool.clone());
    let orphan = NewCapture {
        investigation_id: Some(inv(99)),
        ..capture(1, 7, Duration::zero(), 100)
    };
    assert!(matches!(
        results.insert(&orphan).await,
        Err(StoreError::Validation(_))
    ));
    assert!(matches!(
        results.insert_kept_unlogged(&orphan).await,
        Err(StoreError::InvestigationNotFound)
    ));

    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let kept = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(2, 7, Duration::zero(), 100)
    };
    insert(&results, &kept).await;
    let row = results.get(&kept.id, 7).await.unwrap().expect("the row");
    assert_eq!(
        (row.state, row.expires_at, row.investigation_id.as_ref()),
        (ResultState::Kept, None, Some(&id))
    );
}

/// A step payload at the parser's 64 KiB input limit, every byte one
/// JSON escapes into six, fits the payload bound.
#[sqlx::test]
async fn the_largest_step_fits_the_payload_bound(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let worst = EntryPayload::Step {
        executed: "\u{1}".repeat(65_536),
        bounds: None,
        request_id: "r".repeat(64),
        query_id: Some(u64::MAX),
        purpose: Some("\u{1}".repeat(1024)),
        outcome: None,
    };
    let entry = store
        .begin_step(&id, &agent(), &worst, t0(), RULE, patience())
        .await
        .unwrap()
        .entry;
    assert_eq!(entry.seq, 1);
    let failed = StepOutcome {
        outcome: StepOutcomeKind::Failed,
        code: Some(ErrorCode::ExecutionError),
        result_id: None,
        rows: None,
        duration_ms: Some(u64::MAX),
    };
    assert!(
        store
            .complete_step(&id, 1, &failed, t0(), RULE)
            .await
            .unwrap()
    );
    let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
    let EntryPayload::Step { outcome, .. } = &page.entries[0].payload else {
        panic!("{:?}", page.entries[0]);
    };
    assert_eq!(
        outcome.as_ref().and_then(|outcome| outcome.code.clone()),
        Some(ErrorCode::ExecutionError)
    );
}

// -- attribution ---------------------------------------------------------------

/// Every write snapshots the writer's prefix and name beside the key id,
/// and what leaves the store carries the prefix and name only.
#[sqlx::test]
async fn every_write_snapshots_the_prefix_and_name_and_the_wire_omits_the_key_id(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let wire = |a: &StoreActor| Actor {
        prefix: a.prefix.clone(),
        name: a.name.clone(),
    };

    // Creation: the creator is both creator and last writer.
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!((&row.created_by, &row.updated_by), (&agent(), &agent()));
    assert_eq!(row.to_wire().created_by, wire(&agent()));
    assert_eq!(row.to_wire().updated_by, wire(&agent()));
    assert_eq!(row.to_summary().created_by, wire(&agent()));

    // Append.
    let finding_entry = store
        .append(&id, &human(), &finding(), t0(), RULE)
        .await
        .unwrap()
        .entry;
    assert_eq!(finding_entry.actor, wire(&human()));

    // Overwrite.
    let titled = store
        .overwrite(
            &id,
            1,
            &agent(),
            &Overwrite::Title("pool exhaustion".into()),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(titled.actor, wire(&agent()));

    // Finding status.
    let status = store
        .set_finding_status(
            &id,
            finding_entry.seq,
            2,
            FindingStatus::Supported,
            &human(),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(status.actor, wire(&human()));

    // Step completion: the step's own actor becomes the last writer.
    let begun = store
        .begin_step(&id, &agent(), &step("req-1"), t0(), RULE, patience())
        .await
        .unwrap()
        .entry;
    assert_eq!(begun.actor, wire(&agent()));
    assert!(
        store
            .complete_step(&id, begun.seq, &success(&result_id(1)), t0(), RULE)
            .await
            .unwrap()
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(row.updated_by, agent());
    assert_eq!(row.created_by, agent());

    // Read back: every entry carries the prefix and name of its writer.
    let page = store.read_since(&id, 0, 20).await.unwrap().unwrap();
    let actors: Vec<_> = page.entries.iter().map(|e| e.actor.clone()).collect();
    assert_eq!(
        actors,
        [
            wire(&human()),
            wire(&agent()),
            wire(&human()),
            wire(&agent())
        ]
    );
    // The server keeps the key ids for ownership; they are columns, not wire.
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT actor_key_id FROM investigation_log WHERE investigation_id = $1 ORDER BY seq",
    )
    .bind(id.as_str())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ids, [3, 7, 3, 7]);
    let text = serde_json::to_string(&page.entries).unwrap()
        + &serde_json::to_string(&row.to_wire()).unwrap();
    assert!(!text.contains("key_id"), "{text}");

    // A prefix wider than a fleet prefix is refused by the schema.
    assert!(
        store
            .append(
                &id,
                &StoreActor {
                    key_id: 9,
                    prefix: "123456789".into(),
                    name: "x".into(),
                },
                &note("n"),
                t0(),
                RULE,
            )
            .await
            .is_err()
    );
}

// -- create ------------------------------------------------------------------

/// The same creator with the same request gets the existing investigation
/// and advances nothing; another request or another creator on the id is a
/// mismatch.
#[sqlx::test]
async fn create_if_absent_replays_only_the_same_creator_and_request(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let first = new_investigation(1, agent(), "502s");
    let CreateOutcome::Created(created) = store.create_if_absent(&first).await.unwrap() else {
        panic!("the first create creates");
    };
    assert_eq!(
        (
            created.seq,
            created.status,
            &created.created_by,
            &created.updated_by
        ),
        (0, InvestigationStatus::Open, &agent(), &agent())
    );
    assert_eq!(
        (created.retain_until, created.retain_ceiling),
        (t0() + Duration::days(7), t0() + Duration::days(30))
    );

    // A replay a day later, after a title overwrite: the comparison is
    // against the request that created it, and nothing slides.
    store
        .overwrite(
            &first.id,
            0,
            &human(),
            &Overwrite::Title("pool exhaustion".into()),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    let replay = NewInvestigation {
        now: t0() + Duration::days(1),
        retain_until: t0() + Duration::days(8),
        retain_ceiling: t0() + Duration::days(31),
        ..first.clone()
    };
    let CreateOutcome::Existing(existing) = store.create_if_absent(&replay).await.unwrap() else {
        panic!("the replay finds the investigation");
    };
    assert_eq!(existing.seq, 1);
    assert_eq!(existing.title, "pool exhaustion");
    assert_eq!(existing.retain_until, t0() + Duration::days(7));
    assert_eq!(existing.created_at, t0());

    let other_payload = new_investigation(1, agent(), "other title");
    assert_eq!(
        store.create_if_absent(&other_payload).await.unwrap(),
        CreateOutcome::Mismatch
    );
    let other_creator = new_investigation(1, human(), "502s");
    assert_eq!(
        store.create_if_absent(&other_creator).await.unwrap(),
        CreateOutcome::Mismatch
    );
    assert_eq!(head_seq(&store, &first.id).await, 1);
}

/// Two creates of one id race: one creates, the other replays it.
#[sqlx::test]
async fn concurrent_identical_creates_create_once(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .create_if_absent(&new_investigation(1, agent(), "502s"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut created = 0;
    let mut existing = 0;
    for racer in racers {
        match racer.await.unwrap() {
            CreateOutcome::Created(_) => created += 1,
            CreateOutcome::Existing(_) => existing += 1,
            CreateOutcome::Mismatch => panic!("an identical create is no mismatch"),
        }
    }
    assert_eq!((created, existing), (1, 1));
}

// -- appends and overwrites --------------------------------------------------

/// Two appends from two connections, queued behind a held lock on the
/// investigation, both commit: one at seq 1, the other at seq 2.
#[sqlx::test]
async fn concurrent_appends_both_commit_in_sequence(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;

    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
        .bind(id.as_str())
        .execute(&mut *held)
        .await
        .unwrap();

    let writers: Vec<_> = [agent(), human()]
        .into_iter()
        .map(|actor| {
            let store = store.clone();
            let id = id.clone();
            tokio::spawn(async move {
                store
                    .append(&id, &actor, &note(&actor.name), t0(), RULE)
                    .await
            })
        })
        .collect();
    writers_waiting(&pool, 2).await;
    held.commit().await.unwrap();

    let mut seqs = Vec::new();
    for writer in writers {
        let entry = writer.await.unwrap().expect("both appends commit").entry;
        assert_eq!(entry.updated_seq, entry.seq);
        seqs.push(entry.seq);
    }
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(head_seq(&store, &id).await, 2);
    let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
    assert_eq!(page.entries.len(), 2);
}

/// An overwrite carries the sequence it saw: a stale one fails with both
/// numbers and writes nothing; a current one advances and records its
/// entry.
#[sqlx::test]
async fn a_stale_overwrite_conflicts_with_expected_and_current(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    store
        .append(&id, &human(), &note("paged"), t0(), RULE)
        .await
        .unwrap();

    let stale = store
        .overwrite(
            &id,
            0,
            &agent(),
            &Overwrite::Status(InvestigationStatus::Closed),
            t0(),
            RULE,
        )
        .await;
    assert!(
        matches!(
            stale,
            Err(StoreError::SequenceConflict {
                expected: 0,
                current: 1
            })
        ),
        "{stale:?}"
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!((row.seq, row.status), (1, InvestigationStatus::Open));

    let closed = store
        .overwrite(
            &id,
            1,
            &agent(),
            &Overwrite::Status(InvestigationStatus::Closed),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(
        (closed.seq, closed.payload),
        (
            2,
            EntryPayload::Status {
                status: InvestigationStatus::Closed
            }
        )
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.seq, row.status, row.updated_by),
        (2, InvestigationStatus::Closed, agent())
    );

    store
        .overwrite(
            &id,
            2,
            &human(),
            &Overwrite::Status(InvestigationStatus::Open),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    store
        .overwrite(
            &id,
            3,
            &human(),
            &Overwrite::Question("What exhausts the pool?".into()),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.seq, row.question.as_str()),
        (4, "What exhausts the pool?")
    );
}

/// Only keeps, findings and notes append; the rest have their own paths.
#[sqlx::test]
async fn append_refuses_the_kinds_with_their_own_path(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    for payload in [
        step("r"),
        EntryPayload::Status {
            status: InvestigationStatus::Closed,
        },
        EntryPayload::Title { title: "t".into() },
        EntryPayload::FindingStatus {
            finding_seq: 1,
            status: FindingStatus::Refuted,
        },
    ] {
        assert!(
            matches!(
                store.append(&id, &agent(), &payload, t0(), RULE).await,
                Err(StoreError::Validation(_))
            ),
            "{payload:?}"
        );
    }
    let mut finished = step("r");
    if let EntryPayload::Step { outcome, .. } = &mut finished {
        *outcome = Some(success(&result_id(1)));
    }
    assert!(matches!(
        store
            .begin_step(&id, &agent(), &finished, t0(), RULE, patience())
            .await,
        Err(StoreError::Validation(_))
    ));
    assert_eq!(head_seq(&store, &id).await, 0);
    assert!(matches!(
        store
            .append(&inv(9), &agent(), &note("n"), t0(), RULE)
            .await,
        Err(StoreError::InvestigationNotFound)
    ));
}

/// A finding's status is overwritten by a `finding_status` entry naming
/// the finding's seq, under the same sequence check.
#[sqlx::test]
async fn a_finding_status_names_a_finding_of_this_investigation(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let other = create(&store, 2).await;
    let written = store
        .append(&id, &agent(), &finding(), t0(), RULE)
        .await
        .unwrap()
        .entry;
    store
        .append(&id, &human(), &note("n"), t0(), RULE)
        .await
        .unwrap();
    store
        .append(&other, &agent(), &finding(), t0(), RULE)
        .await
        .unwrap();

    for not_a_finding in [2, 9] {
        assert!(matches!(
            store
                .set_finding_status(
                    &id,
                    not_a_finding,
                    2,
                    FindingStatus::Supported,
                    &human(),
                    t0(),
                    RULE
                )
                .await,
            Err(StoreError::NotFound {
                resource: "finding",
                ..
            })
        ));
    }
    assert!(matches!(
        store
            .set_finding_status(
                &id,
                written.seq,
                1,
                FindingStatus::Supported,
                &human(),
                t0(),
                RULE
            )
            .await,
        Err(StoreError::SequenceConflict {
            expected: 1,
            current: 2
        })
    ));
    let entry = store
        .set_finding_status(
            &id,
            written.seq,
            2,
            FindingStatus::Supported,
            &human(),
            t0(),
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(
        (entry.seq, entry.payload),
        (
            3,
            EntryPayload::FindingStatus {
                finding_seq: 1,
                status: FindingStatus::Supported
            }
        )
    );
}

/// A closed investigation accepts nothing new but a reopen, and a step
/// admitted while it was open still completes. Another close is refused
/// before the sequence check and changes nothing: no entry, no sequence,
/// no timestamp, no retention slide.
#[sqlx::test]
async fn a_closed_investigation_refuses_new_work_and_completes_admitted_work(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let running = store
        .begin_step(&id, &agent(), &step("r1"), t0(), RULE, patience())
        .await
        .unwrap()
        .entry;
    let found = store
        .append(&id, &agent(), &finding(), t0(), RULE)
        .await
        .unwrap()
        .entry;
    store
        .overwrite(
            &id,
            2,
            &human(),
            &Overwrite::Status(InvestigationStatus::Closed),
            t0(),
            RULE,
        )
        .await
        .unwrap();

    assert!(matches!(
        store.append(&id, &agent(), &note("late"), t0(), RULE).await,
        Err(StoreError::InvestigationClosed)
    ));
    assert!(matches!(
        store
            .begin_step(&id, &agent(), &step("r2"), t0(), RULE, patience())
            .await,
        Err(StoreError::InvestigationClosed)
    ));
    assert!(matches!(
        store
            .overwrite(&id, 3, &agent(), &Overwrite::Title("t".into()), t0(), RULE)
            .await,
        Err(StoreError::InvestigationClosed)
    ));
    assert!(matches!(
        store
            .set_finding_status(
                &id,
                found.seq,
                3,
                FindingStatus::Refuted,
                &agent(),
                t0(),
                RULE
            )
            .await,
        Err(StoreError::InvestigationClosed)
    ));
    assert_eq!(head_seq(&store, &id).await, 3);

    assert!(
        store
            .complete_step(&id, running.seq, &success(&result_id(1)), t0(), RULE)
            .await
            .unwrap()
    );
    assert_eq!(head_seq(&store, &id).await, 4);

    let later = t0() + Duration::days(2);
    let before = store.get(&id).await.unwrap().expect("the investigation");
    let log = store
        .read_since(&id, 0, 100)
        .await
        .unwrap()
        .unwrap()
        .entries;
    for expected in [4, 0] {
        assert!(matches!(
            store
                .overwrite(
                    &id,
                    expected,
                    &human(),
                    &Overwrite::Status(InvestigationStatus::Closed),
                    later,
                    RULE,
                )
                .await,
            Err(StoreError::InvestigationClosed)
        ));
    }
    assert_eq!(store.get(&id).await.unwrap(), Some(before));
    assert_eq!(
        store
            .read_since(&id, 0, 100)
            .await
            .unwrap()
            .unwrap()
            .entries,
        log
    );

    let reopened = store
        .overwrite(
            &id,
            4,
            &human(),
            &Overwrite::Status(InvestigationStatus::Open),
            later,
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(reopened.seq, 5);
    let closed = store
        .overwrite(
            &id,
            5,
            &human(),
            &Overwrite::Status(InvestigationStatus::Closed),
            later,
            RULE,
        )
        .await
        .unwrap();
    assert_eq!(closed.seq, 6);
    let row = store.get(&id).await.unwrap().expect("the investigation");
    assert_eq!(row.status, InvestigationStatus::Closed);
    assert_eq!(row.retain_until, later + RULE.idle);
}

// -- reads -------------------------------------------------------------------

/// A read since a sequence pages the entries changed after it, in
/// `updated_seq` order, with `more` and the sequence to read from next.
#[sqlx::test]
async fn read_since_pages_the_interval_with_more(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    for n in 1..=5 {
        store
            .append(&id, &agent(), &note(&format!("n{n}")), t0(), RULE)
            .await
            .unwrap();
    }

    let first = store.read_since(&id, 0, 2).await.unwrap().unwrap();
    let seqs = |page: &trawl_server::store::LogPage| {
        page.entries.iter().map(|e| e.seq).collect::<Vec<_>>()
    };
    assert_eq!(
        (seqs(&first), first.through_seq, first.more),
        (vec![1, 2], 2, true)
    );
    assert_eq!(first.investigation.seq, 5);
    assert_eq!(first.entries[0].payload, note("n1"));
    assert_eq!(first.entries[0].at, "2026-10-24T09:00:00.000000Z");

    let rest = store.read_since(&id, 2, 10).await.unwrap().unwrap();
    assert_eq!(
        (seqs(&rest), rest.through_seq, rest.more),
        (vec![3, 4, 5], 5, false)
    );
    let exact = store.read_since(&id, 2, 3).await.unwrap().unwrap();
    assert_eq!(
        (seqs(&exact), exact.through_seq, exact.more),
        (vec![3, 4, 5], 5, false)
    );
    let none = store.read_since(&id, 5, 10).await.unwrap().unwrap();
    assert_eq!(
        (seqs(&none), none.through_seq, none.more),
        (vec![], 5, false)
    );

    assert_eq!(store.read_since(&inv(9), 0, 10).await.unwrap(), None);
}

/// Activity is the entries other keys changed since the caller looked.
#[sqlx::test]
async fn activity_excludes_the_callers_own_entries(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    store
        .append(&id, &agent(), &note("a1"), t0(), RULE)
        .await
        .unwrap();
    store
        .append(&id, &human(), &note("h2"), t0(), RULE)
        .await
        .unwrap();
    store
        .append(&id, &agent(), &note("a3"), t0(), RULE)
        .await
        .unwrap();
    store
        .append(&id, &human(), &note("h4"), t0(), RULE)
        .await
        .unwrap();

    let for_agent = store
        .activity(&id, 0, agent().key_id, 10)
        .await
        .unwrap()
        .unwrap();
    let texts: Vec<_> = for_agent
        .entries
        .iter()
        .map(|e| e.payload.clone())
        .collect();
    assert_eq!(texts, [note("h2"), note("h4")]);
    assert_eq!((for_agent.through_seq, for_agent.more), (4, false));

    let bounded = store
        .activity(&id, 0, agent().key_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bounded.entries[0].payload, note("h2"));
    assert_eq!((bounded.through_seq, bounded.more), (2, true));

    let for_human = store
        .activity(&id, 1, human().key_id, 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(for_human.entries[0].payload, note("a3"));
    assert_eq!((for_human.entries.len(), for_human.through_seq), (1, 4));
}

/// Open investigations come first, then closed ones, each group most
/// recently changed first.
#[sqlx::test]
async fn list_puts_open_first_then_most_recently_changed(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    for n in 1..=4 {
        create(&store, n).await;
    }
    for (n, hours) in [(1, 4), (2, 1), (3, 3), (4, 2)] {
        store
            .append(
                &inv(n),
                &agent(),
                &note("n"),
                t0() + Duration::hours(hours),
                RULE,
            )
            .await
            .unwrap();
    }
    store
        .overwrite(
            &inv(1),
            1,
            &agent(),
            &Overwrite::Status(InvestigationStatus::Closed),
            t0() + Duration::hours(5),
            RULE,
        )
        .await
        .unwrap();
    let listed: Vec<_> = store
        .list(10)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(listed, [inv(3), inv(4), inv(2), inv(1)]);
    assert_eq!(store.list(2).await.unwrap().len(), 2);
}

// -- steps -------------------------------------------------------------------

/// A step's admission waits for its investigation no later than its
/// deadline: behind a held lock it answers `WaitExpired` and writes
/// nothing, and it answers only once no session waits on the lock, since
/// the `lock_timeout` armed for the lock ended the database's wait and the
/// rollback was answered. A deadline a year away, past what `lock_timeout`
/// can hold, still admits.
#[sqlx::test]
async fn a_step_admission_waits_no_later_than_its_deadline(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
        .bind(id.as_str())
        .execute(&mut *held)
        .await
        .unwrap();

    let soon = Deadline::after(std::time::Duration::from_millis(300));
    let refused = store
        .begin_step(&id, &agent(), &step("r1"), t0(), RULE, soon)
        .await;
    assert!(
        matches!(refused, Err(StoreError::WaitExpired)),
        "{refused:?}"
    );
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(waiting, 0, "a session still waits on the lock");
    held.rollback().await.unwrap();
    assert_eq!(store.get(&id).await.unwrap().unwrap().seq, 0);

    let a_year = Deadline::after(std::time::Duration::from_hours(365 * 24));
    let begun = store
        .begin_step(&id, &agent(), &step("r2"), t0(), RULE, a_year)
        .await
        .unwrap()
        .entry;
    assert_eq!(begun.seq, 1);
}

/// A TCP proxy in front of the test's Postgres that can hold back the
/// answer to the next `BEGIN`: what the client sends reaches Postgres at
/// once, and what Postgres sends back after that `BEGIN` waits until the
/// test releases it. A client that awaits the answer parks there; one cut
/// short leaves the transaction open in Postgres.
mod begin_hold {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use sqlx::PgPool;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Notify, watch};

    pub struct Proxy {
        pub port: u16,
        shared: Arc<Shared>,
    }

    struct Shared {
        /// Hold back the answer to the next `BEGIN` the client sends.
        armed: AtomicBool,
        /// `true` while Postgres' answers are held back.
        closed: watch::Sender<bool>,
        /// Notified once an answer is held back.
        holding: Notify,
    }

    impl Proxy {
        /// A proxy to `pool`'s server on a loopback port of its own.
        pub async fn start(pool: &PgPool) -> Self {
            let options = pool.connect_options();
            let upstream = format!("{}:{}", options.get_host(), options.get_port());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let shared = Arc::new(Shared {
                armed: AtomicBool::new(false),
                closed: watch::channel(false).0,
                holding: Notify::new(),
            });
            let accepting = Arc::clone(&shared);
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let server = TcpStream::connect(&upstream).await.unwrap();
                    let (client_read, client_write) = client.into_split();
                    let (server_read, server_write) = server.into_split();
                    tokio::spawn(sent(client_read, server_write, Arc::clone(&accepting)));
                    tokio::spawn(answered(server_read, client_write, Arc::clone(&accepting)));
                }
            });
            Self { port, shared }
        }

        /// Hold back the answer to the next `BEGIN`.
        pub fn hold_next_begin(&self) {
            self.shared.armed.store(true, Ordering::SeqCst);
        }

        /// Wait until an answer is held back.
        pub async fn holding(&self) {
            self.shared.holding.notified().await;
        }

        /// Let the held answers through.
        pub fn release(&self) {
            self.shared.closed.send_replace(false);
        }
    }

    /// Client to Postgres: forward everything, and close the gate before
    /// a `BEGIN` goes out, so its answer cannot slip through.
    async fn sent(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, shared: Arc<Shared>) {
        let mut buf = vec![0_u8; 8192];
        let mut tail: Vec<u8> = Vec::new();
        loop {
            let n = match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            tail.extend_from_slice(&buf[..n]);
            if tail.windows(5).any(|w| w == b"BEGIN") && shared.armed.swap(false, Ordering::SeqCst)
            {
                shared.closed.send_replace(true);
            }
            let keep = tail.len().saturating_sub(4);
            tail.drain(..keep);
            if to.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }

    /// Postgres to client: forward everything, waiting while the gate is
    /// closed.
    async fn answered(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, shared: Arc<Shared>) {
        let mut buf = vec![0_u8; 8192];
        let mut gate = shared.closed.subscribe();
        loop {
            let n = match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if *gate.borrow_and_update() {
                shared.holding.notify_one();
                if gate.wait_for(|closed| !closed).await.is_err() {
                    return;
                }
            }
            if to.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }
}

/// A bounded write cuts short only its wait for a connection: a deadline
/// that passes while Postgres' answer to its `BEGIN` is on the way leaves
/// the `BEGIN` awaited, and the write rolls back before it answers
/// `WaitExpired`. So the connection goes back to the pool outside any
/// transaction, and what the pool runs next is its own: a statement there
/// is its own transaction, and a transaction-scoped lock it takes is gone
/// when it returns.
#[sqlx::test]
async fn a_deadline_during_a_bounded_writes_begin_returns_no_open_transaction_to_the_pool(
    pool: PgPool,
) {
    let id = create(&InvestigationStore::new(pool.clone()), 1).await;
    let proxy = begin_hold::Proxy::start(&pool).await;
    let options = (*pool.connect_options())
        .clone()
        .host("127.0.0.1")
        .port(proxy.port)
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    #[allow(clippy::disallowed_methods)] // one session, through the proxy
    let through = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .test_before_acquire(false)
        .connect_with(options)
        .await
        .unwrap();
    let store = InvestigationStore::new(through.clone());

    proxy.hold_next_begin();
    let deadline = Deadline::after(std::time::Duration::from_millis(500));
    let mut admission = {
        let id = id.clone();
        tokio::spawn(async move {
            store
                .begin_step(&id, &agent(), &step("r1"), t0(), RULE, deadline)
                .await
        })
    };
    proxy.holding().await;
    // A write that drops its `BEGIN` answers at the deadline; one that
    // awaits it answers only once the answer is let through.
    let early = tokio::time::timeout_at(
        deadline.instant() + std::time::Duration::from_secs(1),
        &mut admission,
    )
    .await;
    proxy.release();
    let answer = match early {
        Ok(joined) => joined.unwrap(),
        Err(_) => admission.await.unwrap(),
    };
    assert!(matches!(answer, Err(StoreError::WaitExpired)), "{answer:?}");

    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while through.num_idle() != 1 {
        assert!(
            std::time::Instant::now() < give_up,
            "the session never went back to the pool"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // One simple-protocol query: it starts the transaction it runs in
    // unless one is open already. (The extended protocol stamps a new
    // statement time at each message.)
    let outside: bool = sqlx::Row::get(
        &sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
            .fetch_one(&through)
            .await
            .unwrap(),
        0,
    );
    assert!(
        outside,
        "the pool's next statement ran inside the write's transaction"
    );
    sqlx::query("SELECT pg_advisory_xact_lock(310)")
        .execute(&through)
        .await
        .unwrap();
    let held: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks
         WHERE locktype = 'advisory' AND objid = 310
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held, 0, "the pool statement's lock outlived it");
    let store = InvestigationStore::new(pool.clone());
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().seq,
        0,
        "nothing written"
    );
}

/// A step's outcome is written once: the completion moves the step's
/// `updated_seq` to a new sequence, and a second completion changes
/// nothing.
#[sqlx::test]
async fn a_step_completes_once(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let begun = store
        .begin_step(&id, &agent(), &step("r1"), t0(), RULE, patience())
        .await
        .unwrap()
        .entry;
    assert_eq!((begun.seq, begun.updated_seq), (1, 1));
    store
        .append(&id, &human(), &note("n"), t0(), RULE)
        .await
        .unwrap();

    let later = t0() + Duration::minutes(1);
    assert!(
        store
            .complete_step(&id, 1, &success(&result_id(1)), later, RULE)
            .await
            .unwrap()
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.seq, row.updated_at, row.updated_by),
        (3, later, agent())
    );

    let since = store.read_since(&id, 2, 10).await.unwrap().unwrap();
    assert_eq!(since.entries.len(), 1);
    let entry = &since.entries[0];
    assert_eq!((entry.seq, entry.updated_seq), (1, 3));
    assert_eq!(entry.at, "2026-10-24T09:00:00.000000Z");
    let EntryPayload::Step {
        outcome,
        request_id,
        ..
    } = &entry.payload
    else {
        panic!("{entry:?}");
    };
    assert_eq!(
        (outcome.as_ref(), request_id.as_str()),
        (Some(&success(&result_id(1))), "r1")
    );
    let columns: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT outcome, outcome_code, result_id FROM investigation_log
         WHERE investigation_id = $1 AND seq = 1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(columns, (Some("success".into()), None, Some(result_id(1))));

    let refused = StepOutcome {
        outcome: StepOutcomeKind::Refused,
        code: Some(ErrorCode::EvidenceBudgetExceeded),
        result_id: None,
        rows: None,
        duration_ms: None,
    };
    assert!(
        !store
            .complete_step(&id, 1, &refused, later, RULE)
            .await
            .unwrap()
    );
    // A seq that is not a step, and one past the log, change nothing.
    assert!(
        !store
            .complete_step(&id, 2, &refused, later, RULE)
            .await
            .unwrap()
    );
    assert!(
        !store
            .complete_step(&id, 9, &refused, later, RULE)
            .await
            .unwrap()
    );
    assert!(
        !store
            .complete_step(&inv(9), 1, &refused, later, RULE)
            .await
            .unwrap()
    );
    assert_eq!(head_seq(&store, &id).await, 3);
}

/// Boot records `interrupted` on every running step, once, through the
/// one-shot completion.
#[sqlx::test]
async fn boot_interrupts_every_running_step_once(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let a = create(&store, 1).await;
    let b = create(&store, 2).await;
    for (id, request) in [(&a, "a1"), (&a, "a2"), (&b, "b1")] {
        store
            .begin_step(id, &agent(), &step(request), t0(), RULE, patience())
            .await
            .unwrap();
    }
    assert!(
        store
            .complete_step(&a, 1, &success(&result_id(1)), t0(), RULE)
            .await
            .unwrap()
    );

    let boot = t0() + Duration::hours(1);
    assert_eq!(store.interrupt_running_steps(boot).await.unwrap(), 2);
    assert_eq!(store.interrupt_running_steps(boot).await.unwrap(), 0);
    let outcomes: Vec<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT investigation_id, seq, outcome FROM investigation_log ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        outcomes,
        [
            (a.to_string(), 1, Some("success".into())),
            (a.to_string(), 2, Some("interrupted".into())),
            (b.to_string(), 1, Some("interrupted".into())),
        ]
    );
    assert_eq!(
        (head_seq(&store, &a).await, head_seq(&store, &b).await),
        (4, 2)
    );
}

/// An interruption is bookkeeping, not a principal's change: it writes the
/// outcome and advances the sequence, and leaves both evidence deadlines
/// where the last change put them, however late the boot runs.
#[sqlx::test]
async fn a_boot_interruption_leaves_the_evidence_clock_where_it_was(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    store
        .begin_step(&id, &agent(), &step("r1"), t0(), RULE, patience())
        .await
        .unwrap();
    let before = store.get(&id).await.unwrap().unwrap();

    let boot = t0() + Duration::days(1);
    assert_eq!(store.interrupt_running_steps(boot).await.unwrap(), 1);
    let after = store.get(&id).await.unwrap().unwrap();
    assert_eq!(after.seq, before.seq + 1);
    assert_eq!(
        (after.retain_until, after.retain_ceiling),
        (before.retain_until, before.retain_ceiling),
        "an interruption slid the evidence clock"
    );
    let outcome: Option<String> =
        sqlx::query_scalar("SELECT outcome FROM investigation_log WHERE seq = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome.as_deref(), Some("interrupted"));
}

// -- retention ---------------------------------------------------------------

/// Every change slides the evidence deadline to `now + idle`, never past
/// the ceiling and never back; an extension raises both and never
/// shortens either.
#[sqlx::test]
async fn retention_slides_with_changes_stops_at_the_ceiling_and_extends(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let id = create(&store, 1).await;
    let deadlines = |store: InvestigationStore, id: InvestigationId| async move {
        let row = store.get(&id).await.unwrap().unwrap();
        (row.retain_until - t0(), row.retain_ceiling - t0())
    };

    store
        .append(&id, &agent(), &note("d1"), t0() + Duration::days(1), RULE)
        .await
        .unwrap();
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(8), Duration::days(30))
    );
    store
        .append(&id, &agent(), &note("d25"), t0() + Duration::days(25), RULE)
        .await
        .unwrap();
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(30), Duration::days(30))
    );
    // A change stamped earlier never moves the deadline back.
    store
        .append(&id, &agent(), &note("d2"), t0() + Duration::days(2), RULE)
        .await
        .unwrap();
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(30), Duration::days(30))
    );

    let operator = StoreActor {
        key_id: 1,
        prefix: "Op3rat0r".into(),
        name: "operator".into(),
    };
    assert!(matches!(
        store
            .extend(
                &id,
                2,
                t0() + Duration::days(40),
                &operator,
                t0() + Duration::days(26)
            )
            .await,
        Err(StoreError::SequenceConflict {
            expected: 2,
            current: 3
        })
    ));
    let extended = store
        .extend(
            &id,
            3,
            t0() + Duration::days(40),
            &operator,
            t0() + Duration::days(26),
        )
        .await
        .unwrap();
    assert_eq!((extended.seq, extended.updated_by), (4, operator.clone()));
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(40), Duration::days(40))
    );
    store
        .extend(
            &id,
            4,
            t0() + Duration::days(35),
            &operator,
            t0() + Duration::days(26),
        )
        .await
        .unwrap();
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(40), Duration::days(40))
    );
    // Past the old ceiling, changes slide toward the raised one.
    store
        .append(&id, &agent(), &note("d36"), t0() + Duration::days(36), RULE)
        .await
        .unwrap();
    assert_eq!(
        deadlines(store.clone(), id.clone()).await,
        (Duration::days(40), Duration::days(40))
    );

    // Reads and refused writes slide nothing.
    let before = store.get(&id).await.unwrap().unwrap();
    store.read_since(&id, 0, 10).await.unwrap();
    let _ = store
        .overwrite(
            &id,
            0,
            &agent(),
            &Overwrite::Title("x".into()),
            t0() + Duration::days(37),
            RULE,
        )
        .await
        .unwrap_err();
    assert_eq!(store.get(&id).await.unwrap().unwrap(), before);
}

// -- kept evidence -----------------------------------------------------------

/// A keep of `result`'s first row.
fn keep(result: &str) -> EntryPayload {
    EntryPayload::Keep {
        result_id: result.into(),
        rows: vec![0],
        reason: "the first 502".into(),
    }
}

/// A keep that cites the caller's own live temporary result promotes it,
/// once, in the append's transaction. Another key's, past its deadline, or
/// kept by another investigation, it promotes nothing and writes nothing;
/// one already kept by this investigation is cited as it is; an unknown
/// investigation is refused.
#[sqlx::test]
async fn promote_succeeds_once_and_only_for_the_owners_live_temporary_result(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let a = create(&store, 1).await;
    let b = create(&store, 2).await;
    let live = capture(1, 7, Duration::minutes(1), 100);
    let due = capture(2, 7, Duration::minutes(15), 100); // expires exactly at T0
    for row in [&live, &due] {
        insert(&results, row).await;
    }
    let other_key = StoreActor {
        key_id: 8,
        ..agent()
    };
    let cite = |actor: StoreActor, into: InvestigationId, result: String| {
        let store = store.clone();
        async move {
            store
                .append_citing(
                    &into,
                    &actor,
                    &keep(&result),
                    &[&result],
                    t0(),
                    RULE,
                    patience(),
                )
                .await
        }
    };

    assert_eq!(
        cite(other_key, a.clone(), live.id.clone()).await.unwrap(),
        Citing::NotCitable(live.id.clone())
    );
    assert_eq!(
        cite(agent(), a.clone(), due.id.clone()).await.unwrap(),
        Citing::NotCitable(due.id.clone())
    );
    assert!(matches!(
        cite(agent(), inv(9), live.id.clone()).await,
        Err(StoreError::InvestigationNotFound)
    ));
    assert_eq!(
        store.get(&a).await.unwrap().unwrap().seq,
        0,
        "nothing written"
    );

    let Citing::Appended(first) = cite(agent(), a.clone(), live.id.clone()).await.unwrap() else {
        panic!("the owner's live result is promoted");
    };
    assert_eq!(first.entry.seq, 1);
    let Citing::Appended(second) = cite(agent(), a.clone(), live.id.clone()).await.unwrap() else {
        panic!("a result kept by this investigation is cited as it is");
    };
    assert_eq!(second.entry.seq, 2);
    assert_eq!(
        cite(agent(), b.clone(), live.id.clone()).await.unwrap(),
        Citing::KeptElsewhere(live.id.clone())
    );
    assert_eq!(store.get(&b).await.unwrap().unwrap().seq, 0);

    let row = results.get(&live.id, 7).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.expires_at, row.investigation_id),
        (ResultState::Kept, None, Some(a))
    );
    assert_eq!(state_of(&pool, &due.id).await.0, "temporary");
}

/// A finding citing row 0 of each of `results`, in that order.
fn finding_citing(results: &[&str]) -> EntryPayload {
    EntryPayload::Finding {
        statement: "the upstream pool is exhausted".into(),
        status: FindingStatus::Hypothesis,
        references: results
            .iter()
            .map(|result| EvidenceRef {
                result_id: (*result).into(),
                row: 0,
            })
            .collect(),
        counterevidence: vec![],
        remaining_question: None,
    }
}

/// A session of its own that holds result `id`'s row lock until
/// `COMMIT`, and its backend pid.
async fn hold_result(pool: &PgPool, id: &str) -> (sqlx::PgConnection, i32) {
    let mut holder = pool.acquire().await.unwrap().detach();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut holder)
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
    sqlx::query("SELECT 1 FROM captured_results WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut holder)
        .await
        .unwrap();
    (holder, pid)
}

/// Wait, bounded, until `count` promotions wait on a result's row lock
/// that session `released` no longer holds: each has moved past the row
/// that session held and waits on another.
async fn promotions_waiting_past(probe: &PgPool, released: i32, count: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE 'UPDATE captured_results%'
                   AND cardinality(pg_blocking_pids(pid)) > 0
                   AND NOT $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(released)
            .fetch_one(probe)
            .await
            .unwrap();
            if waiting == count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the promotions queue behind the remaining lock");
}

/// Two findings cite the caller's live temporary results `a` and `b` into
/// two investigations, one as `[a, b]` and the other as `[b, a]`. Each
/// append locks the results it promotes in one order, so the second
/// queues behind the first instead of closing a cycle with it: the first
/// commits both promotions and the second is refused as kept elsewhere,
/// never a deadlock victim.
///
/// The held rows choose the interleaving a caller-order lock would
/// deadlock in: the `[a, b]` writer takes `a` and waits on `b` while the
/// `[b, a]` writer waits to take `b` first.
#[sqlx::test]
async fn citations_in_opposite_orders_lock_their_results_in_one_order(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let first = create(&store, 1).await;
    let second = create(&store, 2).await;
    let a = capture(1, 7, Duration::minutes(1), 100);
    let b = capture(2, 7, Duration::minutes(1), 100);
    for row in [&a, &b] {
        insert(&results, row).await;
    }
    let cite = |into: InvestigationId, order: [String; 2]| {
        let store = store.clone();
        async move {
            let order: Vec<&str> = order.iter().map(String::as_str).collect();
            store
                .append_citing(
                    &into,
                    &agent(),
                    &finding_citing(&order),
                    &order,
                    t0(),
                    RULE,
                    patience(),
                )
                .await
        }
    };

    let (mut holds_a, a_holder) = hold_result(&pool, &a.id).await;
    let (mut holds_b, _) = hold_result(&pool, &b.id).await;
    let forward = tokio::spawn(cite(first.clone(), [a.id.clone(), b.id.clone()]));
    lock_waits(&pool, "UPDATE captured_results%", 1).await;
    let reversed = tokio::spawn(cite(second.clone(), [b.id.clone(), a.id.clone()]));
    lock_waits(&pool, "UPDATE captured_results%", 2).await;
    sqlx::query("COMMIT").execute(&mut holds_a).await.unwrap();
    promotions_waiting_past(&pool, a_holder, 2).await;
    sqlx::query("COMMIT").execute(&mut holds_b).await.unwrap();

    let forward = forward.await.unwrap();
    let reversed = reversed.await.unwrap();
    assert!(
        matches!(forward, Ok(Citing::Appended(_))),
        "the first writer commits: {forward:?}"
    );
    assert_eq!(
        reversed.expect("the second writer is refused, not a deadlock victim"),
        Citing::KeptElsewhere(a.id.clone())
    );
    for result in [&a.id, &b.id] {
        let row = results.get(result, 7).await.unwrap().unwrap();
        assert_eq!(
            (row.state, row.investigation_id),
            (ResultState::Kept, Some(first.clone()))
        );
    }
    assert_eq!(head_seq(&store, &second).await, 0, "nothing written");
}

/// Run a bulk expiry at `now` beside a promotion that waits on a held
/// result, then release the hold. The expiry either finishes on its own or
/// queues on a lock behind the promotion; either way the hold is released
/// next, and both answers are returned with whether the expiry finished
/// before the release.
async fn expiry_beside_a_waiting_promotion(
    pool: &PgPool,
    results: &ResultsStore,
    mut holder: sqlx::PgConnection,
    cite: tokio::task::JoinHandle<Result<Citing, StoreError>>,
    now: DateTime<Utc>,
) -> (
    bool,
    Result<Vec<String>, StoreError>,
    Result<Citing, StoreError>,
) {
    lock_waits(pool, "UPDATE captured_results%", 1).await;
    let mut expiry = tokio::spawn({
        let results = results.clone();
        async move { results.expire_due(now).await }
    });
    let finished = tokio::select! {
        expired = &mut expiry => Some(expired.unwrap()),
        () = lock_waits(pool, "%captured_results%", 2) => None,
    };
    let before_release = finished.is_some();
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
    let expired = match finished {
        Some(expired) => expired,
        None => expiry.await.unwrap(),
    };
    let expired = expired.map(|files| files.into_iter().map(|file| file.id).collect());
    (before_release, expired, cite.await.unwrap())
}

/// The append citing `[a, b]` into `into` at `T0`, spawned.
fn spawn_cite(
    store: &InvestigationStore,
    into: &InvestigationId,
    a: &str,
    b: &str,
) -> tokio::task::JoinHandle<Result<Citing, StoreError>> {
    let store = store.clone();
    let into = into.clone();
    let order = [a.to_owned(), b.to_owned()];
    tokio::spawn(async move {
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        store
            .append_citing(
                &into,
                &agent(),
                &finding_citing(&order),
                &order,
                t0(),
                RULE,
                patience(),
            )
            .await
    })
}

/// Bulk expiry locks its due results in id order, the order a promotion
/// locks the results it cites, and skips any row another transaction
/// holds, so it never waits on a promotion that waits on it: neither is a
/// deadlock victim.
///
/// The held row chooses the interleaving an expiry that locks in scan
/// order deadlocks in: a promotion of `[a, b]` queues on `a`, and the
/// expiry reaches `b` first (made first, so first on the heap, and due
/// first) and takes it before it reaches `a`. Released, the promotion
/// takes `a` and waits on `b` while the expiry waits on `a`. Here the
/// expiry skips `a`, retires `b` and commits; the promotion then finds
/// `b` gone, as it would after any expiry that came first, and writes
/// nothing. `a` stays due and the next pass retires it.
#[sqlx::test]
async fn bulk_expiry_never_deadlocks_a_sorted_promotion(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let into = create(&store, 1).await;
    let b = capture(2, 7, Duration::minutes(2), 100);
    let a = capture(1, 7, Duration::minutes(1), 100);
    for row in [&b, &a] {
        insert(&results, row).await;
    }
    let due = t0() + Duration::minutes(15);

    let (holds_a, _) = hold_result(&pool, &a.id).await;
    let cite = spawn_cite(&store, &into, &a.id, &b.id);
    let (_, expired, cited) =
        expiry_beside_a_waiting_promotion(&pool, &results, holds_a, cite, due).await;

    assert_eq!(
        expired.expect("the expiry is no deadlock victim"),
        std::slice::from_ref(&b.id)
    );
    assert_eq!(
        cited.expect("the promotion is no deadlock victim"),
        Citing::NotCitable(b.id.clone())
    );
    assert_eq!(head_seq(&store, &into).await, 0, "nothing written");
    let next: Vec<String> = results
        .expire_due(due)
        .await
        .unwrap()
        .into_iter()
        .map(|file| file.id)
        .collect();
    assert_eq!(
        next,
        std::slice::from_ref(&a.id),
        "the skipped row goes at the next pass"
    );
}

/// A row a promotion holds is becoming kept: bulk expiry skips it without
/// waiting and retires the other due rows, and the promotion commits it
/// kept. The promotion of `[a, b]` holds `a` and waits on a held `b`.
#[sqlx::test]
async fn bulk_expiry_skips_the_rows_a_promotion_holds(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let into = create(&store, 1).await;
    let c = capture(3, 7, Duration::minutes(3), 100);
    let b = capture(2, 7, Duration::minutes(2), 100);
    let a = capture(1, 7, Duration::minutes(1), 100);
    for row in [&c, &b, &a] {
        insert(&results, row).await;
    }
    let due = t0() + Duration::minutes(15);

    let (holds_b, _) = hold_result(&pool, &b.id).await;
    let cite = spawn_cite(&store, &into, &a.id, &b.id);
    let (before_release, expired, cited) =
        expiry_beside_a_waiting_promotion(&pool, &results, holds_b, cite, due).await;

    assert!(before_release, "the expiry waits on no promotion");
    assert_eq!(expired.unwrap(), std::slice::from_ref(&c.id));
    assert!(
        matches!(cited, Ok(Citing::Appended(_))),
        "the promotion commits: {cited:?}"
    );
    for result in [&a.id, &b.id] {
        let row = results.get(result, 7).await.unwrap().unwrap();
        assert_eq!(
            (row.state, row.investigation_id),
            (ResultState::Kept, Some(into.clone()))
        );
    }
    assert!(results.expire_due(due).await.unwrap().is_empty());
}

/// A temporary result marked `file_missing` is not promoted, even by an
/// append that classified it before the mark: the promotion's own
/// statement refuses it under the lock, and nothing is written.
#[sqlx::test]
async fn a_result_whose_file_is_missing_is_not_promoted(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let missing = capture(1, 7, Duration::minutes(1), 100);
    insert(&results, &missing).await;
    assert!(results.mark_file_missing(&missing.id).await.unwrap());

    let cited = store
        .append_citing(
            &id,
            &agent(),
            &keep(&missing.id),
            &[&missing.id],
            t0(),
            RULE,
            patience(),
        )
        .await
        .unwrap();
    assert_eq!(cited, Citing::NotCitable(missing.id.clone()));
    assert_eq!(head_seq(&store, &id).await, 0, "nothing written");
    let row = results.get(&missing.id, 7).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.investigation_id, row.file_missing),
        (ResultState::Temporary, None, true)
    );
}

/// Kept rows leave the capture quota: usage, the installation usage, the
/// owner's list, the boot key walk and the quota prune ignore them, while
/// boot reconciliation still expects their files.
#[sqlx::test]
async fn kept_rows_leave_the_quota_and_stay_live_for_reconciliation(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let kept = capture(1, 7, Duration::minutes(1), 1000);
    let temporary = capture(2, 7, Duration::minutes(2), 10);
    let attached = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(3, 9, Duration::zero(), 500)
    };
    for row in [&kept, &temporary, &attached] {
        insert(&results, row).await;
    }
    assert!(
        results
            .promote_unlogged(&kept.id, 7, &id, t0())
            .await
            .unwrap()
    );

    assert_eq!(
        results.usage(7, t0()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 10
        }
    );
    assert_eq!(results.usage(9, t0()).await.unwrap(), Usage::default());
    assert_eq!(
        results.installation_usage(t0()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 10
        }
    );
    let listed: Vec<_> = results
        .list(7, t0())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(listed, [temporary.id.as_str()]);
    assert_eq!(results.live_keys(t0()).await.unwrap(), [7]);
    let pruned = results.prune_oldest(7, 0, 0, t0()).await.unwrap();
    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0].id, temporary.id);
    assert!(
        results
            .prune_oldest_installation(0, 0, t0())
            .await
            .unwrap()
            .is_empty()
    );
    let mut live = results.live_ids().await.unwrap();
    live.sort();
    assert_eq!(live, [(kept.id.clone(), 1000), (attached.id.clone(), 500)]);
    // Past every temporary deadline, both kept rows are still expected.
    assert!(
        results
            .expire_due(t0() + Duration::days(1))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(results.live_ids().await.unwrap().len(), 2);
}

/// Evidence usage counts each kept result's file once, per investigation
/// and across the installation, and nothing temporary or pruned.
#[sqlx::test]
async fn evidence_usage_counts_each_kept_file_once(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let a = create(&store, 1).await;
    let b = create(&store, 2).await;
    let rows = [
        (1, Some(&a), 100),
        (2, Some(&a), 200),
        (3, Some(&b), 400),
        (4, None, 800),
    ];
    for (n, investigation, bytes) in rows {
        insert(
            &results,
            &NewCapture {
                investigation_id: investigation.cloned(),
                ..capture(n, 7, Duration::zero(), bytes)
            },
        )
        .await;
    }
    assert_eq!(store.evidence_usage(&a).await.unwrap(), 300);
    assert_eq!(store.evidence_usage(&b).await.unwrap(), 400);
    assert_eq!(store.evidence_usage(&inv(9)).await.unwrap(), 0);
    assert_eq!(store.installation_evidence_usage().await.unwrap(), 700);

    // At both deadlines, every kept file is pruned and counts no more.
    let pruned = results
        .prune_evidence(t0() + Duration::days(7))
        .await
        .unwrap();
    let mut ids: Vec<_> = pruned.iter().map(|file| file.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, [result_id(1), result_id(2), result_id(3)]);
    assert_eq!(store.installation_evidence_usage().await.unwrap(), 0);
}

/// The read door ends one investigation's evidence under its lock: not
/// while its deadline is ahead, even when the caller read an older one (a
/// write slid it since), then once, with the files to unlink; again finds
/// nothing left to unlink, and an unknown investigation answers `None`.
#[sqlx::test]
async fn a_read_ends_one_investigation_s_evidence_only_once_its_deadline_passed(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let other = create(&store, 2).await;
    let row = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    let beside = NewCapture {
        investigation_id: Some(other.clone()),
        ..capture(2, 7, Duration::zero(), 100)
    };
    insert(&results, &row).await;
    insert(&results, &beside).await;
    let first = t0() + Duration::days(7);
    store
        .append(&id, &agent(), &note("n"), t0() + Duration::days(1), RULE)
        .await
        .unwrap();

    assert_eq!(store.end_evidence_if_due(&id, first).await.unwrap(), None);
    assert_eq!(state_of(&pool, &row.id).await.0, "kept");

    let slid = t0() + Duration::days(8);
    let files = store.end_evidence_if_due(&id, slid).await.unwrap().unwrap();
    assert_eq!(
        files
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(row.id.as_str(), 100)]
    );
    assert_eq!(
        state_of(&pool, &row.id).await,
        ("pruned".into(), Some(slid), Some(slid))
    );
    assert_eq!(state_of(&pool, &beside.id).await.0, "kept");
    assert_eq!(
        store.end_evidence_if_due(&id, slid).await.unwrap(),
        Some(vec![])
    );
    assert_eq!(
        store.end_evidence_if_due(&inv(9), slid).await.unwrap(),
        None
    );
}

/// The evidence prune retires the kept rows of investigations past their
/// deadline, at that deadline; the tombstone sweep leaves them while the
/// investigation exists, and deleting it prunes the rest and lets every
/// row age out.
#[sqlx::test]
async fn pruned_evidence_stays_with_its_investigation_until_deleted(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let stale = create(&store, 1).await;
    let fresh = create(&store, 2).await;
    store
        .append(&fresh, &agent(), &note("n"), t0() + Duration::days(3), RULE)
        .await
        .unwrap();
    let old = NewCapture {
        investigation_id: Some(stale.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    let young = NewCapture {
        investigation_id: Some(fresh.clone()),
        ..capture(2, 7, Duration::zero(), 200)
    };
    let tombstone = capture(3, 7, Duration::minutes(20), 10);
    for row in [&old, &young, &tombstone] {
        insert(&results, row).await;
    }
    assert!(results.mark_expired(&tombstone.id, t0()).await.unwrap());

    let deadline = t0() + Duration::days(7);
    assert!(
        results
            .prune_evidence(deadline - Duration::seconds(1))
            .await
            .unwrap()
            .is_empty()
    );
    let pruned = results.prune_evidence(deadline).await.unwrap();
    assert_eq!(
        pruned
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(old.id.as_str(), 100)]
    );
    assert_eq!(
        state_of(&pool, &old.id).await,
        ("pruned".into(), Some(deadline), Some(deadline))
    );
    assert!(results.prune_evidence(deadline).await.unwrap().is_empty());
    assert_eq!(state_of(&pool, &young.id).await.0, "kept");

    // A year on, only the unattached tombstone goes.
    let much_later = t0() + Duration::days(365);
    assert_eq!(results.delete_tombstones(much_later).await.unwrap(), 1);
    assert_eq!(state_of(&pool, &old.id).await.0, "pruned");
    let row = results.get(&old.id, 7).await.unwrap().unwrap();
    assert_eq!(row.investigation_id.as_ref(), Some(&stale));

    // A running step holds the delete off; nothing changes.
    store
        .begin_step(&fresh, &agent(), &step("r"), t0(), RULE, patience())
        .await
        .unwrap();
    assert_eq!(
        store.delete(&fresh, much_later).await.unwrap(),
        DeleteOutcome::StepsRunning
    );
    assert_eq!(state_of(&pool, &young.id).await.0, "kept");
    store
        .complete_step(&fresh, 2, &success(&young.id), t0(), RULE)
        .await
        .unwrap();

    let DeleteOutcome::Deleted(files) = store.delete(&fresh, much_later).await.unwrap() else {
        panic!("the delete runs");
    };
    assert_eq!(
        files
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(young.id.as_str(), 200)]
    );
    assert_eq!(
        state_of(&pool, &young.id).await,
        ("pruned".into(), Some(much_later), Some(much_later))
    );
    assert_eq!(store.get(&fresh).await.unwrap(), None);
    let log: i64 =
        sqlx::query_scalar("SELECT count(*) FROM investigation_log WHERE investigation_id = $1")
            .bind(fresh.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(log, 0);
    assert_eq!(
        store.delete(&fresh, much_later).await.unwrap(),
        DeleteOutcome::NotFound
    );

    // Detached by the delete, the row ages out like any tombstone.
    assert_eq!(
        results.delete_tombstones(much_later).await.unwrap(),
        1,
        "the deleted investigation's row is gone at its own gone_at"
    );
    assert_eq!(
        store.delete(&stale, much_later).await.unwrap(),
        DeleteOutcome::Deleted(vec![])
    );
    assert_eq!(results.delete_tombstones(much_later).await.unwrap(), 1);
}

/// An extension that holds the investigation when the evidence prune
/// arrives wins: the prune waits for the row, reads the deadline the
/// extension committed, and keeps that investigation's evidence, while it
/// still prunes another investigation that is due. The extension is made
/// a second before the deadline: one made at it is itself due, and the
/// due-write rule prunes first.
#[sqlx::test]
async fn an_extension_committed_while_the_prune_waits_keeps_the_evidence(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let extended = create(&store, 1).await;
    let due = create(&store, 2).await;
    let kept = NewCapture {
        investigation_id: Some(extended.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    let other = NewCapture {
        investigation_id: Some(due.clone()),
        ..capture(2, 7, Duration::zero(), 200)
    };
    insert(&results, &kept).await;
    insert(&results, &other).await;
    let deadline = t0() + Duration::days(7);
    let later = t0() + Duration::days(60);

    // The extension takes the row first and queues behind a held lock; the
    // prune queues behind the extension.
    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
        .bind(extended.as_str())
        .execute(&mut *held)
        .await
        .unwrap();
    let extending = tokio::spawn({
        let store = store.clone();
        let extended = extended.clone();
        let before = deadline - Duration::seconds(1);
        async move { store.extend(&extended, 0, later, &human(), before).await }
    });
    writers_waiting(&pool, 1).await;
    let pruning = tokio::spawn({
        let results = results.clone();
        async move { results.prune_evidence(deadline).await }
    });
    prunes_waiting(&pool, 1).await;
    held.commit().await.unwrap();

    let row = extending.await.unwrap().expect("the extension commits");
    assert_eq!(row.retain_until, later);
    let pruned = pruning.await.unwrap().expect("the prune commits");
    assert_eq!(
        pruned
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(other.id.as_str(), 200)]
    );
    assert_eq!(state_of(&pool, &kept.id).await, ("kept".into(), None, None));
    assert_eq!(
        state_of(&pool, &other.id).await,
        ("pruned".into(), Some(deadline), Some(deadline))
    );
}

/// A prune that holds the investigation when an extension arrives wins:
/// the extension waits for the row and commits its later deadline over
/// evidence already pruned, which stays pruned at the deadline it passed.
#[sqlx::test]
async fn an_extension_behind_the_prune_arrives_too_late(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let kept = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    insert(&results, &kept).await;
    let deadline = t0() + Duration::days(7);
    let later = t0() + Duration::days(60);

    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    let pruned = ResultsStore::prune_evidence_in(&mut held, deadline)
        .await
        .unwrap();
    assert_eq!(
        pruned
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(kept.id.as_str(), 100)]
    );
    let extending = tokio::spawn({
        let store = store.clone();
        let id = id.clone();
        async move { store.extend(&id, 0, later, &human(), deadline).await }
    });
    writers_waiting(&pool, 1).await;
    held.commit().await.unwrap();

    let row = extending.await.unwrap().expect("the extension commits");
    assert_eq!(row.retain_until, later);
    assert_eq!(
        state_of(&pool, &kept.id).await,
        ("pruned".into(), Some(deadline), Some(deadline))
    );
    assert!(results.prune_evidence(later).await.unwrap().is_empty());
}

// -- the due-write rule and attaching races ----------------------------------

/// The deadline every fixture investigation's evidence clock starts at.
fn deadline() -> DateTime<Utc> {
    t0() + Duration::days(7)
}

/// How many kept rows belong to an investigation whose deadline is at or
/// before `at`: evidence that should be gone.
async fn kept_past_deadline(pool: &PgPool, at: DateTime<Utc>) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM captured_results c JOIN investigations i
           ON c.investigation_id = i.id
         WHERE c.state = 'kept' AND i.retain_until <= $1",
    )
    .bind(at)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A write to an investigation past its deadline prunes its kept evidence
/// first, exactly as the sweep would, and answers the files; only then
/// does it write and slide the clock.
#[sqlx::test]
async fn a_note_on_a_due_investigation_prunes_its_evidence_before_it_slides(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let old = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    insert(&results, &old).await;

    let later = deadline() + Duration::days(1);
    let appended = store
        .append(&id, &agent(), &note("late"), later, RULE)
        .await
        .unwrap();
    assert_eq!(
        appended
            .pruned
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(old.id.as_str(), 100)]
    );
    assert_eq!(
        state_of(&pool, &old.id).await,
        ("pruned".into(), Some(deadline()), Some(deadline()))
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(row.retain_until, later + Duration::days(7));
    assert!(results.prune_evidence(later).await.unwrap().is_empty());
}

/// A promotion that arrives while the evidence prune holds its due
/// investigation waits for it, then attaches into the window its own
/// write slides: the old evidence stays pruned at the deadline it passed,
/// exactly the files the prune returned, and the promoted result is kept
/// under the new deadline. Nothing kept is left past its deadline.
#[sqlx::test]
async fn a_promotion_behind_the_prune_attaches_into_the_window_it_slides(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    let old = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    let fresh = capture(2, 7, -Duration::days(7), 200); // live at the deadline
    insert(&results, &old).await;
    insert(&results, &fresh).await;

    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    let pruned = ResultsStore::prune_evidence_in(&mut held, deadline())
        .await
        .unwrap();
    assert_eq!(
        pruned
            .iter()
            .map(|f| (f.id.as_str(), f.file_bytes))
            .collect::<Vec<_>>(),
        [(old.id.as_str(), 100)]
    );
    let promoting = tokio::spawn({
        let store = store.clone();
        let (id, fresh) = (id.clone(), fresh.id.clone());
        async move {
            store
                .append_citing(
                    &id,
                    &agent(),
                    &keep(&fresh),
                    &[&fresh],
                    deadline(),
                    RULE,
                    patience(),
                )
                .await
        }
    });
    writers_waiting(&pool, 1).await;
    held.commit().await.unwrap();

    let Citing::Appended(appended) = promoting.await.unwrap().unwrap() else {
        panic!("the promotion commits");
    };
    assert!(
        appended.pruned.is_empty(),
        "the prune took the old evidence"
    );
    assert_eq!(
        state_of(&pool, &old.id).await,
        ("pruned".into(), Some(deadline()), Some(deadline()))
    );
    assert_eq!(
        state_of(&pool, &fresh.id).await,
        ("kept".into(), None, None)
    );
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(row.retain_until, deadline() + Duration::days(7));
    assert_eq!(kept_past_deadline(&pool, deadline()).await, 0);
}

/// An attached capture's kept insert behind the prune likewise waits, then
/// keeps its row, fills its step and slides the window; the old evidence
/// stays pruned.
#[sqlx::test]
async fn a_kept_insert_behind_the_prune_attaches_into_the_window_it_slides(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let id = create(&store, 1).await;
    store
        .begin_step(&id, &agent(), &step("r1"), t0(), RULE, patience())
        .await
        .unwrap();
    let old = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    insert(&results, &old).await;
    let new = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(2, 7, -Duration::days(7), 300) // created at the deadline
    };

    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    let pruned = ResultsStore::prune_evidence_in(&mut held, deadline())
        .await
        .unwrap();
    assert_eq!(pruned.len(), 1);
    let inserting = tokio::spawn({
        let results = results.clone();
        let new = new.clone();
        async move {
            let outcome = success(&new.id);
            let step = KeptStep {
                seq: 1,
                outcome: &outcome,
                retention: RULE,
            };
            results.insert_kept(&new, step).await
        }
    });
    writers_waiting(&pool, 1).await;
    held.commit().await.unwrap();

    assert_eq!(
        inserting.await.unwrap().unwrap(),
        KeptInsert::Inserted { pruned: vec![] }
    );
    assert_eq!(
        state_of(&pool, &old.id).await,
        ("pruned".into(), Some(deadline()), Some(deadline()))
    );
    assert_eq!(state_of(&pool, &new.id).await, ("kept".into(), None, None));
    let row = store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.seq, row.retain_until),
        (2, deadline() + Duration::days(7))
    );
    let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
    let EntryPayload::Step { outcome, .. } = &page.entries[0].payload else {
        panic!("the step");
    };
    assert_eq!(
        outcome.as_ref().map(|o| o.outcome),
        Some(StepOutcomeKind::Success)
    );
    assert_eq!(kept_past_deadline(&pool, deadline()).await, 0);
}

/// The reverse order: a promotion that holds the investigation when the
/// prune arrives commits first, and the prune that waited for it prunes
/// the promoted result too. The investigation's deadline is pinned at its
/// ceiling, so the promotion's write cannot slide it; one that can slide
/// it keeps its evidence, as an extension does
/// ([`an_extension_committed_while_the_prune_waits_keeps_the_evidence`]).
#[sqlx::test]
async fn a_promotion_ahead_of_the_prune_at_its_ceiling_is_pruned_with_the_rest(pool: PgPool) {
    let store = InvestigationStore::new(pool.clone());
    let results = ResultsStore::new(pool.clone());
    let pinned = NewInvestigation {
        retain_until: deadline(),
        retain_ceiling: deadline(),
        ..new_investigation(1, agent(), "502s")
    };
    assert!(matches!(
        store.create_if_absent(&pinned).await.unwrap(),
        CreateOutcome::Created(_)
    ));
    let id = inv(1);
    let old = NewCapture {
        investigation_id: Some(id.clone()),
        ..capture(1, 7, Duration::zero(), 100)
    };
    let fresh = capture(2, 7, -Duration::days(7), 200);
    insert(&results, &old).await;
    insert(&results, &fresh).await;

    let mut holder = pool.acquire().await.unwrap().detach();
    let mut held = holder.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
        .bind(id.as_str())
        .execute(&mut *held)
        .await
        .unwrap();
    let before = deadline() - Duration::seconds(1);
    let promoting = tokio::spawn({
        let store = store.clone();
        let (id, fresh) = (id.clone(), fresh.id.clone());
        async move {
            store
                .append_citing(
                    &id,
                    &agent(),
                    &keep(&fresh),
                    &[&fresh],
                    before,
                    RULE,
                    patience(),
                )
                .await
        }
    });
    writers_waiting(&pool, 1).await;
    let pruning = tokio::spawn({
        let results = results.clone();
        async move { results.prune_evidence(deadline()).await }
    });
    prunes_waiting(&pool, 1).await;
    held.commit().await.unwrap();

    let Citing::Appended(appended) = promoting.await.unwrap().unwrap() else {
        panic!("the promotion commits first");
    };
    assert!(appended.pruned.is_empty());
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().retain_until,
        deadline(),
        "the ceiling holds the deadline"
    );
    let mut pruned: Vec<_> = pruning
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|f| (f.id, f.file_bytes))
        .collect();
    pruned.sort();
    assert_eq!(pruned, [(old.id.clone(), 100), (fresh.id.clone(), 200)]);
    assert_eq!(
        state_of(&pool, &fresh.id).await,
        ("pruned".into(), Some(deadline()), Some(deadline()))
    );
    assert_eq!(kept_past_deadline(&pool, deadline()).await, 0);
}
