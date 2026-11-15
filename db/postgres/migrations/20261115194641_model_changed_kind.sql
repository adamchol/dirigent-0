-- grant_insert_kinds only runs as session_access changes, so the players there before it learned
-- `model_changed` (they write every kind) get it here.
INSERT INTO dirigent.insert_kinds (subject, kind)
  SELECT DISTINCT subject, 'model_changed' FROM dirigent.session_access WHERE role = 'player'
  ON CONFLICT DO NOTHING;
