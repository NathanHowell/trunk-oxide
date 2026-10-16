CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY,
    talkgroup INTEGER NOT NULL,
    freq REAL NOT NULL,
    encrypted INTEGER NOT NULL,
    emergency INTEGER NOT NULL,
    start_time INTEGER NOT NULL,
    duration REAL,
    signal REAL,
    tag TEXT NOT NULL,
    short_name TEXT NOT NULL,
    transcription TEXT,
    tones TEXT,
    events TEXT,
    thread_id INTEGER,
    embed_message_id INTEGER
);

-- Keyed by the system's short_name, not sys_num: sys_num is assigned at startup
-- and is not stable across restarts.
CREATE TABLE IF NOT EXISTS thread_cache (
    short_name TEXT NOT NULL,
    talkgroup INTEGER NOT NULL,
    date TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (short_name, talkgroup, date)
);

CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,
    signal_counts TEXT NOT NULL,
    trunk_msg_counts TEXT NOT NULL,
    unit_counts TEXT NOT NULL,
    active_calls INTEGER
);
