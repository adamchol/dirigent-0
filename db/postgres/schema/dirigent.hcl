# dirigent's Postgres: the control plane's state (to come), and the roles that use it. Passwords
# are the deployer's.

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
