use super::*;
use crate::db_discovery::repos::ReposConfig;
use crate::testing::EnvRestore;
use serial_test::serial;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const NOW_MS: u64 = 100 * DAY_MS;

fn ago(days: u64) -> u64 {
    NOW_MS - days * DAY_MS
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

struct SelectCase {
    name: &'static str,
    aliases: &'static [&'static str],
    access: Vec<(&'static str, u64)>,
    max_age_days: u64,
    cap: usize,
    ineligible: &'static [&'static str],
    expected: &'static [&'static str],
}

fn select_cases() -> Vec<SelectCase> {
    let abc = &["alpha", "beta", "gamma"];
    let abc_access = vec![("alpha", ago(3)), ("beta", ago(1)), ("gamma", ago(2))];
    vec![
        SelectCase {
            name: "most recent access first",
            aliases: abc,
            access: abc_access.clone(),
            max_age_days: 5,
            cap: 3,
            ineligible: &[],
            expected: &["beta", "gamma", "alpha"],
        },
        SelectCase {
            name: "more candidates than the cap yields exactly the cap",
            aliases: abc,
            access: abc_access.clone(),
            max_age_days: 5,
            cap: 2,
            ineligible: &[],
            expected: &["beta", "gamma"],
        },
        SelectCase {
            name: "a cap above the candidate count yields every candidate",
            aliases: abc,
            access: abc_access,
            max_age_days: 5,
            cap: 10,
            ineligible: &[],
            expected: &["beta", "gamma", "alpha"],
        },
        SelectCase {
            name: "repos past the age gate and never-used repos are skipped",
            aliases: abc,
            access: vec![("alpha", ago(1)), ("beta", ago(9))],
            max_age_days: 5,
            cap: 3,
            ineligible: &[],
            expected: &["alpha"],
        },
        SelectCase {
            name: "stale repos do not occupy cap slots",
            aliases: &["alpha", "beta", "gamma", "delta"],
            access: vec![
                ("alpha", ago(9)),
                ("beta", ago(1)),
                ("gamma", ago(2)),
                ("delta", ago(3)),
            ],
            max_age_days: 5,
            cap: 2,
            ineligible: &[],
            expected: &["beta", "gamma"],
        },
        SelectCase {
            name: "ineligible repos are skipped and do not occupy cap slots",
            aliases: abc,
            access: vec![("alpha", ago(2)), ("beta", ago(1)), ("gamma", ago(3))],
            max_age_days: 5,
            cap: 2,
            ineligible: &["beta"],
            expected: &["alpha", "gamma"],
        },
        SelectCase {
            name: "every repo ineligible selects nothing",
            aliases: abc,
            access: vec![("alpha", ago(1)), ("beta", ago(1)), ("gamma", ago(1))],
            max_age_days: 5,
            cap: 3,
            ineligible: &["alpha", "beta", "gamma"],
            expected: &[],
        },
        SelectCase {
            name: "age gate off keeps old repos, never-used repos last",
            aliases: abc,
            access: vec![("beta", ago(90)), ("gamma", ago(1))],
            max_age_days: 0,
            cap: 3,
            ineligible: &[],
            expected: &["gamma", "beta", "alpha"],
        },
        SelectCase {
            name: "ties between never-used repos break on alias",
            aliases: &["beta", "alpha"],
            access: vec![],
            max_age_days: 0,
            cap: 2,
            ineligible: &[],
            expected: &["alpha", "beta"],
        },
        SelectCase {
            name: "a zero cap selects nothing",
            aliases: abc,
            access: abc_access_recent(),
            max_age_days: 5,
            cap: 0,
            ineligible: &[],
            expected: &[],
        },
    ]
}

fn abc_access_recent() -> Vec<(&'static str, u64)> {
    vec![("alpha", ago(1)), ("beta", ago(1)), ("gamma", ago(1))]
}

#[test]
fn select_prewarm_aliases_table() {
    for case in select_cases() {
        let access: HashMap<String, u64> = case
            .access
            .iter()
            .map(|(alias, ts)| ((*alias).to_string(), *ts))
            .collect();
        let got = select_prewarm_aliases(
            owned(case.aliases),
            &access,
            NOW_MS,
            case.max_age_days,
            case.cap,
            |alias| !case.ineligible.contains(&alias),
        );
        assert_eq!(got, owned(case.expected), "{}", case.name);
    }
}

#[test]
fn eligibility_is_probed_lazily_and_never_past_the_cap() {
    let access: HashMap<String, u64> = [
        ("alpha", ago(4)),
        ("beta", ago(1)),
        ("gamma", ago(2)),
        ("delta", ago(3)),
    ]
    .into_iter()
    .map(|(alias, ts)| (alias.to_string(), ts))
    .collect();
    let mut probed = Vec::new();

    let got = select_prewarm_aliases(
        owned(&["alpha", "beta", "gamma", "delta"]),
        &access,
        NOW_MS,
        5,
        2,
        |alias| {
            probed.push(alias.to_string());
            true
        },
    );

    assert_eq!(got, owned(&["beta", "gamma"]));
    assert_eq!(
        probed,
        owned(&["beta", "gamma"]),
        "probe stopped at the cap"
    );
}

#[test]
#[serial]
fn prewarm_env_switch_only_turns_off_on_an_exact_zero() {
    let cases: &[(Option<&str>, bool)] = &[
        (None, true),
        (Some("0"), false),
        (Some("1"), true),
        (Some(""), true),
        (Some("off"), true),
    ];
    for (raw, expected) in cases {
        let _env = match raw {
            Some(value) => EnvRestore::set(&[(SCIP_RESIDENT_PREWARM_ENV, value)]),
            None => EnvRestore::remove(&[SCIP_RESIDENT_PREWARM_ENV]),
        };
        assert_eq!(
            prewarm_enabled(),
            *expected,
            "{SCIP_RESIDENT_PREWARM_ENV}={raw:?}"
        );
    }
}

/// Fake load: records each path, tracks overlap, and fails for `fail_on`.
#[derive(Clone, Default)]
struct FakeSpawn {
    calls: Arc<Mutex<Vec<PathBuf>>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    fail_on: Option<PathBuf>,
}

impl FakeSpawn {
    fn spawn_fn(&self) -> PrewarmFn {
        let fake = self.clone();
        Arc::new(move |path: &Path| {
            fake.calls.lock().unwrap().push(path.to_path_buf());
            let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            fake.max_in_flight.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(15));
            fake.in_flight.fetch_sub(1, Ordering::SeqCst);
            if fake.fail_on.as_deref() == Some(path) {
                anyhow::bail!("fake load failed");
            }
            Ok(())
        })
    }

    fn called(&self) -> Vec<PathBuf> {
        self.calls.lock().unwrap().clone()
    }
}

fn targets(aliases: &[&str]) -> Vec<(String, PathBuf)> {
    aliases
        .iter()
        .map(|a| ((*a).to_string(), PathBuf::from(format!("C:/repos/{a}"))))
        .collect()
}

#[tokio::test]
async fn run_prewarm_loads_in_order_one_at_a_time() {
    let fake = FakeSpawn::default();

    let loaded = run_prewarm(
        targets(&["a", "b", "c"]),
        &CancellationToken::new(),
        fake.spawn_fn(),
    )
    .await;

    assert_eq!(loaded, owned(&["a", "b", "c"]));
    assert_eq!(
        fake.called(),
        vec![
            PathBuf::from("C:/repos/a"),
            PathBuf::from("C:/repos/b"),
            PathBuf::from("C:/repos/c")
        ]
    );
    assert_eq!(fake.max_in_flight.load(Ordering::SeqCst), 1, "sequential");
}

#[tokio::test]
async fn run_prewarm_skips_a_failed_load_and_continues() {
    let fake = FakeSpawn {
        fail_on: Some(PathBuf::from("C:/repos/b")),
        ..FakeSpawn::default()
    };

    let loaded = run_prewarm(
        targets(&["a", "b", "c"]),
        &CancellationToken::new(),
        fake.spawn_fn(),
    )
    .await;

    assert_eq!(loaded, owned(&["a", "c"]));
    assert_eq!(fake.called().len(), 3);
}

#[tokio::test]
async fn run_prewarm_with_a_cancelled_token_loads_nothing() {
    let fake = FakeSpawn::default();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let loaded = run_prewarm(targets(&["a", "b"]), &cancel, fake.spawn_fn()).await;

    assert!(loaded.is_empty());
    assert!(fake.called().is_empty());
}

#[tokio::test]
async fn run_prewarm_cancelled_mid_load_returns_without_starting_the_next_repo() {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let started = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let spawn: PrewarmFn = {
        let (release_rx, started, calls) = (release_rx.clone(), started.clone(), calls.clone());
        Arc::new(move |path: &Path| {
            calls.lock().unwrap().push(path.to_path_buf());
            started.store(true, Ordering::SeqCst);
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            Ok(())
        })
    };
    let cancel = CancellationToken::new();
    let canceller = {
        let (cancel, started) = (cancel.clone(), started.clone());
        async move {
            while !started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel();
        }
    };

    let (loaded, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run_prewarm(targets(&["a", "b"]), &cancel, spawn), canceller)
    })
    .await
    .expect("a cancelled prewarm must return without waiting for the running load");
    release_tx.send(()).unwrap();

    assert!(loaded.is_empty(), "the interrupted load is not reported");
    assert_eq!(calls.lock().unwrap().len(), 1, "b must never start");
}

/// `repos`: `(alias, has_solution, cached C# status)`. Every repo carries a
/// cached status, so the solution check and the status check each decide a
/// row on their own.
fn fixture(repos: &[(&str, bool, CSharpIndexStatus)]) -> (tempfile::TempDir, Arc<ServeState>) {
    let tmp = tempfile::tempdir().unwrap();
    let config_file = tmp.path().join("repos.json");
    let mut config = ReposConfig::default();
    for (alias, has_solution, _) in repos {
        let repo_path = tmp.path().join(alias);
        std::fs::create_dir(&repo_path).unwrap();
        if *has_solution {
            std::fs::write(
                repo_path.join("test.sln"),
                b"Microsoft Visual Studio Solution File",
            )
            .unwrap();
        }
        config
            .register_with_alias(repo_path, Some((*alias).to_string()))
            .unwrap();
    }
    config.save_to(&config_file).unwrap();
    let state = Arc::new(ServeState::new(config, Some(config_file)));
    for (alias, _, status) in repos {
        state
            .csharp_index_status
            .insert((*alias).to_string(), *status);
    }
    (tmp, state)
}

/// Aliases (repo directory names) the fake load was asked to prewarm.
fn loaded_aliases(fake: &FakeSpawn) -> Vec<String> {
    fake.called()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
#[serial]
async fn run_resident_prewarm_loads_only_ready_csharp_repos() {
    let _age_off = EnvRestore::set(&[(crate::constants::WARMUP_MAX_AGE_DAYS_ENV, "0")]);
    let _switch = EnvRestore::remove(&[SCIP_RESIDENT_PREWARM_ENV]);
    let (_tmp, state) = fixture(&[
        ("pw_ready", true, CSharpIndexStatus::Ready),
        ("pw_error", true, CSharpIndexStatus::Error),
        ("pw_indexing", true, CSharpIndexStatus::Indexing),
        ("pw_partial", true, CSharpIndexStatus::Partial),
        ("pw_no_solution", false, CSharpIndexStatus::Ready),
    ]);
    let fake = FakeSpawn::default();

    let loaded = state
        .run_resident_prewarm_with(&CancellationToken::new(), 8, fake.spawn_fn())
        .await;

    assert_eq!(loaded, owned(&["pw_ready"]));
    assert_eq!(loaded_aliases(&fake), owned(&["pw_ready"]));
}

#[tokio::test]
#[serial]
async fn run_resident_prewarm_loads_exactly_the_cap_when_more_repos_qualify() {
    let _age_off = EnvRestore::set(&[(crate::constants::WARMUP_MAX_AGE_DAYS_ENV, "0")]);
    let _switch = EnvRestore::remove(&[SCIP_RESIDENT_PREWARM_ENV]);
    let (_tmp, state) = fixture(&[
        ("pw_a", true, CSharpIndexStatus::Ready),
        ("pw_b", true, CSharpIndexStatus::Ready),
        ("pw_c", true, CSharpIndexStatus::Ready),
    ]);
    let fake = FakeSpawn::default();

    let loaded = state
        .run_resident_prewarm_with(&CancellationToken::new(), 2, fake.spawn_fn())
        .await;

    assert_eq!(loaded.len(), 2);
    assert_eq!(fake.called().len(), 2);
}

#[tokio::test]
#[serial]
async fn run_resident_prewarm_skips_a_repo_past_the_age_gate() {
    let _age = EnvRestore::set(&[(crate::constants::WARMUP_MAX_AGE_DAYS_ENV, "5")]);
    let _switch = EnvRestore::remove(&[SCIP_RESIDENT_PREWARM_ENV]);
    let (_tmp, state) = fixture(&[("pw_unused", true, CSharpIndexStatus::Ready)]);
    let fake = FakeSpawn::default();

    let loaded = state
        .run_resident_prewarm_with(&CancellationToken::new(), 8, fake.spawn_fn())
        .await;

    assert!(
        loaded.is_empty(),
        "no recorded use means older than any age"
    );
    assert!(fake.called().is_empty());
}

#[tokio::test]
#[serial]
async fn run_resident_prewarm_env_switch_decides_whether_anything_loads() {
    let _age_off = EnvRestore::set(&[(crate::constants::WARMUP_MAX_AGE_DAYS_ENV, "0")]);
    let cases: &[(Option<&str>, usize)] = &[(None, 1), (Some("0"), 0), (Some("1"), 1)];
    for (raw, expected_loads) in cases {
        let _switch = match raw {
            Some(value) => EnvRestore::set(&[(SCIP_RESIDENT_PREWARM_ENV, value)]),
            None => EnvRestore::remove(&[SCIP_RESIDENT_PREWARM_ENV]),
        };
        let (_tmp, state) = fixture(&[("pw_switch", true, CSharpIndexStatus::Ready)]);
        let fake = FakeSpawn::default();

        state
            .run_resident_prewarm_with(&CancellationToken::new(), 8, fake.spawn_fn())
            .await;

        assert_eq!(
            fake.called().len(),
            *expected_loads,
            "{SCIP_RESIDENT_PREWARM_ENV}={raw:?}"
        );
    }
}
