//! End-to-end tests: a real service on a temporary socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hiroi_daemon::{Client, DaemonConfig, DaemonError, run};
use hiroi_engine::EngineConfig;
use hiroi_proto::{Event, JobId, JobStatus, NewJob, Request, Response};
use hiroi_testserver::{Behavior, TestServer, content};
use tokio::task::JoinHandle;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn config(&self) -> DaemonConfig {
        let engine = EngineConfig {
            min_split_size: 64 * 1024,
            progress_interval: Duration::from_millis(50),
            checkpoint_interval: Duration::from_millis(100),
            ..EngineConfig::default()
        };
        DaemonConfig {
            socket: self.root.path().join("run").join("hiroid.sock"),
            database: self.root.path().join("data").join("hiroi.db"),
            download_dir: self.downloads(),
            engine,
        }
    }

    fn downloads(&self) -> PathBuf {
        self.root.path().join("downloads")
    }

    /// Starts the service and waits until it accepts connections.
    async fn start(&self) -> (JoinHandle<Result<(), DaemonError>>, Client) {
        let config = self.config();
        let socket = config.socket.clone();
        let service = tokio::spawn(run(config, std::future::pending()));
        for _ in 0..200 {
            if let Ok(client) = Client::connect(&socket).await {
                return (service, client);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("service did not start: {:?}", service.await);
    }
}

async fn add(client: &mut Client, job: NewJob) -> JobId {
    match client.call(Request::Add(job)).await.unwrap() {
        Response::Added { job } => job,
        other => panic!("unexpected reply {other:?}"),
    }
}

async fn job(client: &mut Client, id: JobId) -> hiroi_proto::Job {
    match client.call(Request::Get { job: id }).await.unwrap() {
        Response::Job { job } => *job,
        other => panic!("unexpected reply {other:?}"),
    }
}

async fn wait_for(client: &mut Client, id: JobId, status: JobStatus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while job(client, id).await.status != status {
        assert!(
            tokio::time::Instant::now() < deadline,
            "job {id} never became {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn jobs_run_and_report_events() {
    let server = TestServer::start(Behavior::default()).await;
    let data = content(1024 * 1024, 1);
    server.put("a.bin", data.clone());
    let fixture = Fixture::new();
    let (_service, mut client) = fixture.start().await;
    let mut events = Client::connect(&fixture.config().socket).await.unwrap();
    events.call(Request::Subscribe).await.unwrap();

    let id = add(
        &mut client,
        NewJob {
            url: server.url("a.bin"),
            ..NewJob::default()
        },
    )
    .await;

    let mut saw_progress = false;
    loop {
        match events.next_event().await.unwrap().unwrap() {
            Event::Progress { job, .. } if job == id => saw_progress = true,
            Event::Job { job } if job.id == id && job.status == JobStatus::Completed => break,
            _ => {}
        }
    }
    assert!(saw_progress);
    let Response::Jobs { jobs } = client.call(Request::List).await.unwrap() else {
        panic!()
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].path.as_deref(),
        Some(fixture.downloads().join("a.bin").as_path())
    );
    assert_eq!(read(&fixture.downloads().join("a.bin")), data);
}

#[tokio::test]
async fn the_queue_survives_a_restart() {
    let behavior = Behavior {
        bytes_per_sec: Some(256 * 1024),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    let data = content(2 * 1024 * 1024, 2);
    server.put("big.bin", data.clone());
    let fixture = Fixture::new();

    let (service, mut client) = fixture.start().await;
    let id = add(
        &mut client,
        NewJob {
            url: server.url("big.bin"),
            ..NewJob::default()
        },
    )
    .await;
    let paused = add(
        &mut client,
        NewJob {
            url: server.url("big.bin"),
            paused: true,
            file_name: Some("later.bin".into()),
            ..NewJob::default()
        },
    )
    .await;
    while job(&mut client, id).await.downloaded < 256 * 1024 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.call(Request::Shutdown).await.unwrap();
    service.await.unwrap().unwrap();

    server.set_behavior(Behavior::default());
    let (_service, mut client) = fixture.start().await;
    assert_eq!(
        job(&mut client, paused).await.status,
        JobStatus::Paused,
        "paused jobs stay paused"
    );
    wait_for(&mut client, id, JobStatus::Completed).await;
    assert_eq!(read(&fixture.downloads().join("big.bin")), data);

    client.call(Request::Resume { job: paused }).await.unwrap();
    wait_for(&mut client, paused, JobStatus::Completed).await;
    assert_eq!(read(&fixture.downloads().join("later.bin")), data);
}

#[tokio::test]
async fn secrets_never_reach_the_database() {
    let server = TestServer::start(Behavior::default()).await;
    server.put("s.bin", content(1000, 3));
    let fixture = Fixture::new();
    let (service, mut client) = fixture.start().await;

    let url = server
        .url("s.bin")
        .replace("http://", "http://alice:hunter2@");
    let job = NewJob {
        url,
        headers: vec![("Cookie".into(), "session=topsecret".into())],
        ..NewJob::default()
    };
    let id = add(&mut client, job).await;
    wait_for(&mut client, id, JobStatus::Completed).await;
    client.call(Request::Shutdown).await.unwrap();
    service.await.unwrap().unwrap();

    let data_dir = fixture.config().database.parent().unwrap().to_path_buf();
    for entry in std::fs::read_dir(data_dir).unwrap() {
        let bytes = read(&entry.unwrap().path());
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("hunter2"), "password stored");
        assert!(!text.contains("topsecret"), "cookie stored");
    }
}

#[tokio::test]
async fn only_one_service_runs_at_a_time() {
    let fixture = Fixture::new();
    let (_service, _client) = fixture.start().await;
    let second = run(fixture.config(), std::future::pending()).await;
    assert!(
        matches!(second, Err(DaemonError::AlreadyRunning(_))),
        "{second:?}"
    );
}

#[tokio::test]
async fn running_jobs_cannot_be_removed_and_errors_are_clear() {
    let behavior = Behavior {
        bytes_per_sec: Some(64 * 1024),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("r.bin", content(1024 * 1024, 4));
    let fixture = Fixture::new();
    let (_service, mut client) = fixture.start().await;

    let id = add(
        &mut client,
        NewJob {
            url: server.url("r.bin"),
            ..NewJob::default()
        },
    )
    .await;
    wait_for(&mut client, id, JobStatus::Downloading).await;
    let refused = client.call(Request::Remove { job: id }).await.unwrap_err();
    assert!(refused.to_string().contains("still running"), "{refused}");

    client.call(Request::Pause { job: id }).await.unwrap();
    wait_for(&mut client, id, JobStatus::Paused).await;
    client.call(Request::Remove { job: id }).await.unwrap();
    let missing = client.call(Request::Get { job: id }).await.unwrap_err();
    assert!(missing.to_string().contains("no job"), "{missing}");

    let bad = client
        .call(Request::Add(NewJob {
            url: "file:///etc/passwd".into(),
            ..NewJob::default()
        }))
        .await;
    assert!(bad.unwrap_err().to_string().contains("scheme"));
}

#[tokio::test]
async fn held_queues_wait_and_schedules_release_them() {
    use hiroi_daemon::schedule::Moment;
    use hiroi_proto::{EVERY_DAY, Schedule, ScheduleAction};

    let server = TestServer::start(Behavior::default()).await;
    server.put("n.bin", content(512 * 1024, 5));
    let fixture = Fixture::new();
    let (_service, mut client) = fixture.start().await;

    client
        .call(Request::StopQueue {
            queue: "night".into(),
        })
        .await
        .unwrap();
    let job_in_night = NewJob {
        url: server.url("n.bin"),
        queue: Some("night".into()),
        ..NewJob::default()
    };
    let id = add(&mut client, job_in_night).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        job(&mut client, id).await.status,
        JobStatus::Queued,
        "a held queue's job waits"
    );
    assert_eq!(server.requests(), 0, "and sends no request");

    // A window that covers the current minute starts the queue.
    let now = Moment::now();
    let rule = Schedule {
        id: 0,
        days: EVERY_DAY,
        from_minute: now.minute,
        to_minute: Some((now.minute + 2) % (24 * 60)),
        action: ScheduleAction::RunQueue {
            queue: "night".into(),
        },
    };
    client.call(Request::AddSchedule(rule)).await.unwrap();
    wait_for(&mut client, id, JobStatus::Completed).await;

    let Response::Queues { queues } = client.call(Request::Queues).await.unwrap() else {
        panic!()
    };
    let night = queues.iter().find(|queue| queue.name == "night").unwrap();
    assert!(night.running);
    assert_eq!((night.jobs, night.pending), (1, 0));
}

#[tokio::test]
async fn holding_a_queue_pauses_its_jobs_and_survives_restarts() {
    let behavior = Behavior {
        bytes_per_sec: Some(128 * 1024),
        ..Behavior::default()
    };
    let server = TestServer::start(behavior).await;
    server.put("h.bin", content(1024 * 1024, 6));
    let fixture = Fixture::new();
    let (service, mut client) = fixture.start().await;

    let id = add(
        &mut client,
        NewJob {
            url: server.url("h.bin"),
            ..NewJob::default()
        },
    )
    .await;
    wait_for(&mut client, id, JobStatus::Downloading).await;
    client
        .call(Request::StopQueue {
            queue: "main".into(),
        })
        .await
        .unwrap();
    wait_for(&mut client, id, JobStatus::Queued).await;

    client.call(Request::Shutdown).await.unwrap();
    service.await.unwrap().unwrap();
    server.set_behavior(Behavior::default());
    let (_service, mut client) = fixture.start().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        job(&mut client, id).await.status,
        JobStatus::Queued,
        "still held after a restart"
    );

    client
        .call(Request::StartQueue {
            queue: "main".into(),
        })
        .await
        .unwrap();
    wait_for(&mut client, id, JobStatus::Completed).await;
}
