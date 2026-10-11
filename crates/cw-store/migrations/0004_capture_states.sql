CREATE TABLE capture_states (
  id TEXT PRIMARY KEY,              -- ULID
  state TEXT NOT NULL,
  start_at TEXT NOT NULL,
  end_at TEXT NOT NULL,             -- last instant the span covers, inclusive
  process TEXT,
  title TEXT,
  detail TEXT
);

CREATE INDEX idx_capture_states_end ON capture_states(end_at);
