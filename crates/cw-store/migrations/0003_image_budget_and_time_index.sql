CREATE INDEX idx_images_time ON images(created_at, observation_id);

CREATE TABLE image_budget (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  -- An overflowing + or - in the triggers below lands on a REAL rather than failing, and that
  -- value would go on reading back as a total; the type check is what refuses it.
  total_bytes INTEGER NOT NULL
    CHECK (typeof(total_bytes) = 'integer')
    CHECK (total_bytes >= 0)
);

INSERT INTO image_budget (id, total_bytes) SELECT 1, coalesce(sum(byte_size), 0) FROM images;

-- Without the row, every trigger below updates nothing and reports success.
CREATE TRIGGER image_budget_no_delete BEFORE DELETE ON image_budget BEGIN
  SELECT raise(ABORT, 'the image budget row is not deletable');
END;

CREATE TRIGGER images_budget_insert AFTER INSERT ON images BEGIN
  UPDATE image_budget SET total_bytes = total_bytes + new.byte_size WHERE id = 1;
END;

CREATE TRIGGER images_budget_delete AFTER DELETE ON images BEGIN
  UPDATE image_budget SET total_bytes = total_bytes - old.byte_size WHERE id = 1;
END;

CREATE TRIGGER images_budget_update AFTER UPDATE OF byte_size ON images BEGIN
  UPDATE image_budget SET total_bytes = total_bytes + new.byte_size - old.byte_size WHERE id = 1;
END;
