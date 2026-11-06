# dirigent's Postgres: the control plane's state, and the roles that use it. Passwords are the
# deployer's.

schema "dirigent" {}

# The control plane: reads and writes its tables.
role "dirigent_control" {
  login = true
}

# chlive: reads the grants it enforces, through logical replication (CDC).
role "dirigent_chlive" {
  login       = true
  replication = true
}

grant {
  privileges = [USAGE]
  on         = [schema.dirigent]
  to         = [role.dirigent_control, role.dirigent_chlive]
}

# The sessions there are. `epoch` counts the players that took the session: each new holder of
# its lease gets the next one (a player resuming its own lease keeps it).
table "dirigent.sessions" {
  column "id" { type = uuid }
  column "epoch" {
    type    = bigint
    null    = false
    default = 0
  }
  # What it runs (dirigent_protocol::control::SessionSpec), with its defaults filled in; fixed.
  column "spec" {
    type    = jsonb
    null    = false
    default = sql("'{}'::jsonb")
  }
  primary_key { columns = [column.id] }
}

# Who may read and write a session's events (chlive's grants, `subject` being who a connection
# is), by `role`: its `owner` (who made it: the one user who writes its inputs, starts and stops
# it), its `viewer`s (read only), and the `player` holding its lease. The player's row is the
# lease: `expires_at` is null while the player is connected to control, and when it drops (or
# control restarts) the time until which the player may come back for it; past it, control
# deletes the row. A session with a lease is active, one without idle. `spawned_by`: the runner
# control started the player with (reserving the lease for it), and which starts another when the
# lease expires rather than being given back; null for a player started otherwise.
table "dirigent.session_access" {
  column "session_id" { type = uuid }
  column "subject" { type = text }
  column "role" {
    type = text
    null = false
  }
  column "expires_at" { type = timestamptz }
  column "spawned_by" { type = text }
  primary_key { columns = [column.session_id, column.subject] }
  foreign_key "session_access_session_id_fkey" {
    columns    = [column.session_id]
    references = [table.dirigent.sessions.column.id]
    on_delete  = CASCADE
  }
  check "session_access_role_check" {
    expr = "role IN ('owner', 'viewer', 'player')"
  }
  check "session_access_lease_check" {
    expr = "role = 'player' OR (expires_at IS NULL AND spawned_by IS NULL)"
  }
  # one lease per session
  index "session_access_player_idx" {
    columns = [column.session_id]
    unique  = true
    where   = "role = 'player'"
  }
  # one owner per session
  index "session_access_owner_idx" {
    columns = [column.session_id]
    unique  = true
    where   = "role = 'owner'"
  }
  trigger "session_access_insert_kinds" {
    after   = [INSERT, UPDATE, DELETE]
    for     = ROW
    execute = function.dirigent.grant_insert_kinds
  }
}

# The kinds of events each subject may insert (chlive's second insert grant; the first says into
# which sessions): kept by a trigger from `session_access`, since a grant reads one table keyed
# by the subject. A player may insert every kind (into the session it holds); an owner only the
# inputs, its messages and interrupts; a viewer nothing. The kinds are `dirigent_protocol::Event`'s
# (a test keeps the list complete).
table "dirigent.insert_kinds" {
  column "subject" { type = text }
  column "kind" { type = text }
  primary_key { columns = [column.subject, column.kind] }
}

function "dirigent.grant_insert_kinds" {
  returns  = trigger
  language = plpgsql
  # it writes insert_kinds, which only its owner (the migrations' role) may
  security = DEFINER
  set      = { search_path = "dirigent, pg_catalog" }
  body     = <<-SQL
    DECLARE
      who text;
      wanted text[];
      inputs text[] := ARRAY['user_message', 'user_interrupt'];
      every_kind text[] := inputs || ARRAY[
        'player_attached', 'player_detached', 'player_failed', 'workspace_ready',
        'workspace_restored', 'checkpoint', 'checkpoint_failed', 'turn_started', 'steered',
        'model_delta', 'assistant_message', 'model_cancelled', 'error', 'compacted',
        'compaction_failed', 'tool_result', 'turn_ended', 'session_update'];
    BEGIN
      FOREACH who IN ARRAY ARRAY[OLD.subject, NEW.subject] LOOP
        CONTINUE WHEN who IS NULL;
        wanted := CASE
          WHEN EXISTS (SELECT FROM session_access WHERE subject = who AND role = 'player')
            THEN every_kind
          WHEN EXISTS (SELECT FROM session_access WHERE subject = who AND role = 'owner')
            THEN inputs
          ELSE ARRAY[]::text[]
        END;
        -- as it should be, whatever another transaction did meanwhile
        INSERT INTO insert_kinds (subject, kind) SELECT who, unnest(wanted)
          ON CONFLICT DO NOTHING;
        DELETE FROM insert_kinds WHERE subject = who AND kind <> ALL (wanted);
      END LOOP;
      RETURN NULL;
    END
  SQL
}

publication "dirigent_chlive" {
  tables = [table.dirigent.session_access, table.dirigent.insert_kinds]
}

grant {
  privileges = [SELECT, INSERT, UPDATE, DELETE]
  on         = [table.dirigent.sessions, table.dirigent.session_access]
  to         = [role.dirigent_control]
}

grant {
  privileges = [SELECT]
  on         = [table.dirigent.session_access, table.dirigent.insert_kinds]
  to         = [role.dirigent_chlive]
}
