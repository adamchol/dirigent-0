# Configuration

Every value of the Helm chart (`chart/dirigent/values.yaml`, which has the same notes as comments).
Values left out keep their defaults. For a first install, see [Installing](installing.md).

The chart renders no Secrets: control makes its own at start ([Operations](operations.md#secrets-to-keep)),
and the ones you provide are named below. One install per namespace: everything is named
`dirigent-*`.

## images

The chart's own images: `<registry>/dirigent-<name>:<tag>`, for `control`, `web`, `tools` (its Jobs
and init containers), `player-vm` (the VMs' NixOS disk) and `player` (the player, on a disk of its
own).

| Value | Default | |
|---|---|---|
| `images.registry` | `ghcr.io/adamchol` | |
| `images.tag` | `""` | The chart's appVersion when empty. |
| `images.pullPolicy` | `IfNotPresent` | |
| `images.pullSecrets` | `[]` | Names of docker-registry Secrets. VMs' disks use the first. The ingestion has its own (`ingestion.imagePullSecrets`, as `[{name: …}]`). |

## auth

| Value | Default | |
|---|---|---|
| `auth.mode` | `oidc` | `oidc`: Google sign-in, users are `user:<email>`. `declared`: no sign-in, callers say who they are (development only). |
| `auth.google.clientIds` | `[]` | OAuth client ids whose ID tokens are accepted; the first is the web's. Required with `oidc`. |
| `auth.google.requiredClaims` | `{}` | Claims a token must have, e.g. `{hd: example.com}`. `email_verified` is always required. |
| `auth.google.cli.clientId`, `clientSecret` | `""` | The CLI's "Desktop app" client. Without it, `dirigent login` can't sign in. |

Web sign-ins last 30 days unused (`control.env: {DIRIGENT_SESSION_DAYS: "…"}` to change).

## github

An optional GitHub App for private repositories ([setup](installing.md#github-app)).

| Value | Default | |
|---|---|---|
| `github.app.slug` | `""` | Its name in `github.com/apps/<slug>`; empty, no App. |
| `github.app.clientId` | `""` | |
| `github.app.secret` | `""` | A Secret with `clientSecret` and `privateKey`. |

## postgres

A CloudNativePG `Cluster` named `dirigent-postgres`, with the `dirigent` database and control's and
chlive's roles (chlive reads two tables through logical replication).

| Value | Default |
|---|---|
| `postgres.instances` | `1` |
| `postgres.storage` | `10Gi` |
| `postgres.storageClass` | `""` (the cluster's default) |
| `postgres.image` | `""` (CloudNativePG's PostgreSQL 17 image) |

## clickhouse

Yours, required: its HTTP(S) interface. The admin user runs the migrations and makes the users
dirigent needs, with passwords the chart generates.

| Value | Default | |
|---|---|---|
| `clickhouse.host` | `""` | Required. |
| `clickhouse.port` | `8443` | |
| `clickhouse.secure` | `true` | HTTPS. |
| `clickhouse.adminSecret` | `""` | Required: a Secret with `username` and `password`. |
| `clickhouse.cluster` | `""` | Adds `ON CLUSTER` to the users and grants, if access is replicated. |

## kafka

Yours, required. These are chlive's settings; the ingestion has its own (below).

| Value | Default | |
|---|---|---|
| `kafka.bootstrapServers` | `""` | Required. |
| `kafka.properties` | `{}` | librdkafka properties that aren't secret, e.g. `security.protocol: SASL_SSL`. |
| `kafka.existingSecret` | `""` | A Secret with the secret ones, as a properties file (`sasl.username=…` lines). |
| `kafka.propertiesKey` | `librdkafka.properties` | Its key in that Secret. |

One Secret can serve chlive and the ingestion: `librdkafka.properties` for chlive, `client.properties`
for Kafka Connect (`ingestion.kafka.existingSecret`).

## checkpoints

The workspaces' checkpoints, a kopia repository in S3; the players' Nix cache (`nix.cache`) is in
the same S3.

| Value | Default | |
|---|---|---|
| `checkpoints.s3.endpoint` | `""` | `host:port`. Required unless Garage is on. |
| `checkpoints.s3.bucket` | `dirigent-workspaces` | Made by the chart's hook if missing. |
| `checkpoints.s3.tls` | `true` | |
| `checkpoints.s3.existingSecret` | `""` | A Secret with `accessKey` and `secretKey`. Required unless Garage is on. |
| `checkpoints.garage.enabled` | `false` | Run a single-node Garage (`dirigent-garage`) with the buckets and a key of its own. |
| `checkpoints.garage.image` | `docker.io/dxflrs/garage:v2.5.0` | |
| `checkpoints.garage.storage` | `50Gi` | |
| `checkpoints.garage.storageClass` | `""` | |

The repository keeps files of any size, leaves out what `.gitignore` excludes, and never expires
snapshots (the log refers to them); control runs its maintenance.

## player

The players: one KubeVirt VM each. See [Machines and systems](machines-and-systems.md) for how these
fit together.

| Value | Default | |
|---|---|---|
| `player.cpu` | `2` | The default machine's cores. |
| `player.memory` | `4Gi` | Its memory, whole `Gi`. |
| `player.workspaceSize` | `20Gi` | Its workspace disk (a fresh disk per VM), whole `Gi`. |
| `player.limits.cpu`, `.memory`, `.workspaceSize` | `null` | The most a session's machine may have; `null`, no limit. |
| `player.defaultsPerUser` | `false` | Each user sets their own default machine in Settings. |
| `player.warm.count` | `0` | VMs booted ahead of time (per active user with `defaultsPerUser`). |
| `player.warm.activeWithin` | `604800` | With `defaultsPerUser`: users who made a session within this many seconds get warm VMs. |
| `player.stopGrace` | `3600` | Seconds a player stopped from outside (a drained node, a deleted VM) has. |
| `player.checkpointMargin` | `300` | Of those, kept for the checkpoint and handover: the turn may run until `stopGrace - checkpointMargin`. Must be less than `stopGrace`. |
| `player.mock` | `false` | The mock model: no API calls (tests). |
| `player.env` | `{}` | More environment variables for the players, e.g. `{RUST_LOG: dirigent_player=debug}`. They're written into the VM's cloud-init data, which isn't a Secret: no secrets here. |
| `player.nodeSelector` | `{}` | Where the VMs (and build VMs) run. |
| `player.nixos.files` | `{}` | The VMs' own NixOS configuration ([details](machines-and-systems.md#the-clusters-own-system)); must have `configuration.nix`. |
| `player.nixos.build.cpu` | `4` | The VM that builds it. |
| `player.nixos.build.memory` | `8Gi` | |
| `player.nixos.build.timeout` | `3600` | Seconds. |

## nix

| Value | Default | |
|---|---|---|
| `nix.cache.bucket` | `dirigent-nix` | The S3 bucket of the Nix cache control's builds go to and the VMs fetch from, signed with a key the chart makes. |
| `nix.cache.region` | `us-east-1` | |
| `nix.shared.enabled` | `false` | A Nix store all VMs share ([details](machines-and-systems.md#the-shared-nix-store)). |
| `nix.shared.storageClass` | `""` | |
| `nix.shared.size` | `200Gi` | |
| `nix.shared.accessMode` | `ReadWriteMany` | `ReadWriteOnce` works only on a one-node cluster. |
| `nix.shared.every` | `3600` | At most one new generation this often (seconds). |
| `nix.shared.keep` | `3` | Generations kept besides those in use. |
| `nix.shared.promote` | `upstream` | What goes in: `upstream` (only paths cache.nixos.org signed) or `all` (everything sessions build or fetch: shared between all users' sessions, for mutually trusting users only). |

## control

| Value | Default | |
|---|---|---|
| `control.resources` | `{requests: {cpu: 100m, memory: 128Mi}}` | |
| `control.startup` | `600` | Seconds a started player has to take its lease (VM boot and image pulls included) before control gives up on it and starts another. |
| `control.env` | `{}` | More environment variables, e.g. `{RUST_LOG: control=debug}`. See [Control](../reference/control.md) for those it reads. |

## chlive

| Value | Default |
|---|---|
| `chlive.image` | `ghcr.io/dialohq/chlive`, tag pinned by the chart |
| `chlive.resources` | `{requests: {cpu: 250m, memory: 512Mi}}` |
| `chlive.cacheSize` | `10Gi` (an `emptyDir`'s limit) |

## web

| Value | Default | |
|---|---|---|
| `web.ingress.enabled` | `false` | An Ingress `dirigent-web` for `/`. |
| `web.ingress.className` | `""` | |
| `web.ingress.host` | `""` | Required when enabled. |
| `web.ingress.tls` | `[]` | As in an Ingress' `spec.tls`. |
| `web.ingress.annotations` | `{}` | E.g. your controller's WebSocket timeouts, for `/live`. |
| `web.httpRoute.enabled` | `false` | A Gateway API HTTPRoute instead. |
| `web.httpRoute.parentRefs` | `[]` | Required when enabled, e.g. `[{name: gateway, namespace: gateway-system}]`. |
| `web.httpRoute.hostnames` | `[]` | All, when empty. |
| `web.httpRoute.timeouts` | `{}` | The `/` rule's; `/live` is a long-lived WebSocket. |
| `web.httpRoute.annotations` | `{}` | |

## hosts

Users' own machines running `dirigent host` ([Hosts](../guides/hosts.md)).

| Value | Default | |
|---|---|---|
| `hosts.expose` | `false` | Route control's players' service (gRPC: the `Lease` and `Host` calls) at the web's address, on the HTTPRoute or as an Ingress `dirigent-players` (with `web.ingress`). |
| `hosts.ingress.annotations` | `{}` | That Ingress' annotations: it must reach control over gRPC, e.g. `nginx.ingress.kubernetes.io/backend-protocol: GRPC`. |

## networkPolicy

| Value | Default | |
|---|---|---|
| `networkPolicy.enabled` | `true` | Players' VMs get no ingress, and egress only to DNS, control, chlive, Garage (if on) and addresses outside `clusterCIDRs`. |
| `networkPolicy.clusterCIDRs` | `[10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16]` | Private ranges players can't reach. An S3 endpoint inside them (other than the chart's Garage) is blocked for players: narrow the list, or disable the policy. |

## ingestion

The `durable-clickhouse-sink` subchart: Kafka Connect with an exactly-once ClickHouse sink, its pause
server (which chlive uses to snapshot tables consistently) and a recovery controller. It doesn't see
the values above; give it its own.

| Value | Default | |
|---|---|---|
| `ingestion.enabled` | `true` | Off, run the pipeline yourself (below). |
| `ingestion.pauseServerUrl` | `""` | Required with `enabled: false`: your pause server. |
| `ingestion.kafka.bootstrapServers` | `""` | Required. |
| `ingestion.kafka.existingSecret` | `""` | Kafka Connect's secret properties (`client.properties`). |
| `ingestion.kafka.replicationFactor`, `internalTopicReplicationFactor` | `3` | `1` for a single broker. |
| `ingestion.clickhouse.host`, `port`, `secure` | `""`, `8443`, `true` | Required: the same ClickHouse. |
| `ingestion.pauseServer.keeperAddress` | `""` | Required: ClickHouse Keeper's `host:port`. |
| `ingestion.connect.replicas` | `2` | |
| `ingestion.connect.errors.deadLetterQueue.replicationFactor` | `3` | `1` for a single broker. |
| `ingestion.pipelines` | one, `session-events` | Topic and table `session_events` in database `dirigent`, with the transforms below. |

The pipeline adds each message's partition, offset and timestamp to its row, the timestamp as UTC
text (`yyyy-MM-dd'T'HH:mm:ss.SSS'Z'`), and inserts with
`async_insert=0,insert_deduplicate=1,date_time_input_format=best_effort`. Its topic keeps messages
90 days (set a pipeline's `retentionMs` to change it).

With `ingestion.enabled: false`, run an equivalent pipeline: topic and table `session_events`,
database `dirigent`, the same transforms, and `date_time_input_format=best_effort` on ClickHouse
before 26.x.
