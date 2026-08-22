-- Splits the parked note off the error column: last_error keeps the failure the operator
-- reads, last_note carries the healthy in-flight/park note (the operation id), and every
-- writer clears the column it does not set.
ALTER TABLE outbox ADD COLUMN last_note TEXT;

-- Version 1 parked the healthy in-flight note in last_error, where the split's reader
-- reports it as a failure. The note is matched by its one historical spelling; the state
-- column cannot select it (real errors share 'pending', and a kill mid-attempt leaves the
-- note under 'delivering').
UPDATE outbox SET last_error = NULL
WHERE last_error LIKE 'hindsight is still processing the episode (operation %';
