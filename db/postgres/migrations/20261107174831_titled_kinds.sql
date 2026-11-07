-- Who may already insert into a session's log may write `titled` too: grant_insert_kinds only
-- runs as session_access changes, so the owners and players there before it learned the kind
-- get it here.
INSERT INTO dirigent.insert_kinds (subject, kind)
  SELECT DISTINCT subject, 'titled' FROM dirigent.session_access WHERE role IN ('owner', 'player')
  ON CONFLICT DO NOTHING;
