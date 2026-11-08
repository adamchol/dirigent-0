-- The players holding a lease may write `files_changed` too: grant_insert_kinds only runs as
-- session_access changes, so the players there before it learned the kind get it here.
INSERT INTO dirigent.insert_kinds (subject, kind)
  SELECT DISTINCT subject, 'files_changed' FROM dirigent.session_access WHERE role = 'player'
  ON CONFLICT DO NOTHING;
