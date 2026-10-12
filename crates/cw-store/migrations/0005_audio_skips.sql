CREATE TABLE audio_skips (
  id TEXT PRIMARY KEY,
  start_at TEXT NOT NULL,
  end_at TEXT NOT NULL,
  process TEXT
);

CREATE INDEX idx_audio_skips_end ON audio_skips(end_at);
