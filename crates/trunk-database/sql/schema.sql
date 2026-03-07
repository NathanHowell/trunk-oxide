CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY,
    talkgroup INTEGER NOT NULL,
    sys_num INTEGER NOT NULL,
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

CREATE TABLE IF NOT EXISTS thread_cache (
    sys_num INTEGER NOT NULL,
    talkgroup INTEGER NOT NULL,
    date TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (sys_num, talkgroup, date)
);

CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,
    signal_counts TEXT NOT NULL,
    trunk_msg_counts TEXT NOT NULL,
    unit_counts TEXT NOT NULL,
    active_calls INTEGER
);
