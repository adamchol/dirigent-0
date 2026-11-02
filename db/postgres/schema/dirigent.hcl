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
  primary_key { columns = [column.id] }
}

# Who may read and write a session's events (chlive's grants, `subject` being its `hello` user):
# its users, and the player holding its lease. The player's row is the lease: `expires_at` is
# null while the player is connected to control, and when it drops (or control restarts) the
# time until which the player may come back for it; past it, control deletes the row.
table "dirigent.session_access" {
  column "session_id" { type = uuid }
  column "subject" { type = text }
  column "player" {
    type    = boolean
    null    = false
    default = false
  }
  column "expires_at" { type = timestamptz }
  primary_key { columns = [column.session_id, column.subject] }
  foreign_key "session_access_session_id_fkey" {
    columns    = [column.session_id]
    references = [table.dirigent.sessions.column.id]
    on_delete  = CASCADE
  }
  check "session_access_lease_check" {
    expr = "player OR expires_at IS NULL"
  }
  # one lease per session
  index "session_access_player_idx" {
    columns = [column.session_id]
    unique  = true
    where   = "player"
  }
}

publication "dirigent_chlive" {
  tables = [table.dirigent.session_access]
}

grant {
  privileges = [SELECT, INSERT, UPDATE, DELETE]
  on         = [table.dirigent.sessions, table.dirigent.session_access]
  to         = [role.dirigent_control]
}

grant {
  privileges = [SELECT]
  on         = [table.dirigent.session_access]
  to         = [role.dirigent_chlive]
}
