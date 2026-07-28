-- Schema version 1.
--
-- Applied exactly once, guarded by PRAGMA user_version (see db.rs). There is no IF NOT EXISTS
-- anywhere on purpose: a second run of this file means the version marker is broken, and that must
-- fail loudly rather than quietly agree with whatever is already in the file.

CREATE TABLE observations (
  id TEXT PRIMARY KEY,              -- ULID
  source TEXT NOT NULL,
  observed_at TEXT NOT NULL,        -- RFC 3339, UTC
  duration_ms INTEGER,
  schema_version INTEGER NOT NULL,  -- the payload's schema, not this file's
  payload TEXT NOT NULL             -- JSON
);

CREATE INDEX idx_obs_time ON observations(observed_at);

CREATE TABLE episodes (
  id TEXT PRIMARY KEY,              -- ULID
  source TEXT NOT NULL,
  start_at TEXT NOT NULL,
  end_at TEXT NOT NULL,
  document_id TEXT NOT NULL UNIQUE, -- Hindsight document_id; the UNIQUE is what stops a rescan
                                    -- from delivering the same window twice
  content TEXT NOT NULL,            -- the delivered body, snapshotted at close so a retry sends
                                    -- the same bytes rather than rebuilding them
  metadata_json TEXT NOT NULL,      -- the retain metadata, snapshotted for the same reason
  created_at TEXT NOT NULL
);

CREATE TABLE images (               -- the truth about which image files exist and how big they
                                    -- are; the observation's payload JSON is never rewritten
  observation_id TEXT PRIMARY KEY REFERENCES observations(id),
  relative_path TEXT NOT NULL UNIQUE,
  byte_size INTEGER NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE outbox (
  episode_id TEXT PRIMARY KEY REFERENCES episodes(id),
  state TEXT NOT NULL,              -- pending / delivering / delivered / failed
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error TEXT
);

CREATE TABLE control_events (       -- append-only audit trail: pause, resume, blacklist skips
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  at TEXT NOT NULL,
  detail TEXT
);

CREATE TABLE control_state (        -- current state, and the one that is authoritative;
                                    -- control_events is history
  key TEXT PRIMARY KEY,             -- 'pause_until' | 'pause_indefinite' | 'last_tick_at'
                                    -- | 'last_capture_at' | 'last_delivery_at'
  value TEXT
);
