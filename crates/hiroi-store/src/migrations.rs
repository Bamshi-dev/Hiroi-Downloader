//! Schema versions. `PRAGMA user_version` records how many migrations
//! ran; each migration runs once, in order, inside a transaction.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: jobs and settings.
    "CREATE TABLE jobs (
        id          INTEGER PRIMARY KEY,
        url         TEXT    NOT NULL,
        mirrors     TEXT    NOT NULL,
        dir         TEXT    NOT NULL,
        options     TEXT    NOT NULL,
        queue       TEXT    NOT NULL,
        status      TEXT    NOT NULL,
        error       TEXT,
        file_name   TEXT,
        path        TEXT,
        total_size  INTEGER,
        downloaded  INTEGER NOT NULL DEFAULT 0,
        added_at    INTEGER NOT NULL,
        finished_at INTEGER
    );
    CREATE INDEX jobs_by_status ON jobs (status);
        CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );",
    // 2: queues that are held, and scheduler rules.
    "CREATE TABLE queues (
        name    TEXT PRIMARY KEY,
        stopped INTEGER NOT NULL
    );
    CREATE TABLE schedules (
        id          INTEGER PRIMARY KEY,
        days        INTEGER NOT NULL,
        from_minute INTEGER NOT NULL,
        to_minute   INTEGER,
        action      TEXT    NOT NULL
    );",
];

pub(crate) fn apply(connection: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let done = usize::try_from(current).unwrap_or(0);
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(done) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", (index + 1) as i64)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_twice_changes_nothing() {
        let mut connection = Connection::open_in_memory().unwrap();
        apply(&mut connection).unwrap();
        apply(&mut connection).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
    }
}
