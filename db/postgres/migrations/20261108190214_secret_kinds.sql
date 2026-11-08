-- grant_insert_kinds only runs as session_access changes, so the subjects there before it learned
-- the kinds get them here: owners answer a dirigent's requests for secrets (`secret_answered`),
-- players write every kind (`secrets_changed` too).
INSERT INTO dirigent.insert_kinds (subject, kind)
  SELECT DISTINCT subject, 'secret_answered' FROM dirigent.session_access
  WHERE role IN ('owner', 'player')
  ON CONFLICT DO NOTHING;
INSERT INTO dirigent.insert_kinds (subject, kind)
  SELECT DISTINCT subject, 'secrets_changed' FROM dirigent.session_access WHERE role = 'player'
  ON CONFLICT DO NOTHING;
