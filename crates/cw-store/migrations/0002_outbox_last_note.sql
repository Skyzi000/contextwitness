-- Splits the parked note off the error column: last_error keeps the failure the operator
-- reads, last_note carries the healthy in-flight/park note (the operation id), and every
-- writer clears the column it does not set.
ALTER TABLE outbox ADD COLUMN last_note TEXT;
