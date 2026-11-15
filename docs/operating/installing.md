# Installing on Kubernetes

dirigent installs as one Helm chart per namespace. This page covers what the cluster needs, the
services you bring, sign-in, and a first install. [Configuration](configuration.md) lists every
value.

## What you need

### On the cluster

| | Why | Notes |
|---|---|---|
| **KubeVirt** | Each player runs in a VM | Tested with 1.9.1. Nodes running players need hardware virtualization (`/dev/kvm`). |
| **CloudNativePG** | The chart's Postgres | Tested with 1.30.1. |
| An Ingress controller or a Gateway API implementation | To reach the web | Optional; any way to reach the `dirigent-web` Service works. |

The chart refuses to render without the KubeVirt and CloudNativePG APIs. Two optional features need
more ([Machines and systems](machines-and-systems.md#the-shared-nix-store)): the shared Nix store
needs KubeVirt's `EnableVirtioFsStorageVolumes` feature gate, Kubernetes 1.36
(`MutatingAdmissionPolicy`) and a `ReadWriteMany` storage class.

### Services you bring

| Service | Used for | Notes |
|---|---|---|
| **ClickHouse** (HTTP interface) with **Keeper** | The log's table; the ingestion's exactly-once state (KeeperMap) | An admin user that can create users, roles and grants. ClickHouse needs `keeper_map_path_prefix` configured. Versions before 26.x need dirigent 0.1.9 or later. |
| **Kafka** | The log's topic | Must support transactions (`read_committed`). Redpanda works. |
| **S3** | Workspace checkpoints (kopia) and the players' Nix cache | Or set `checkpoints.garage.enabled` and the chart runs a single-node Garage. |
| **Google OAuth clients** | Sign-in (web and CLI) | Below. Not needed with `auth.mode: declared` (development only). |
| A **GitHub App** | Private repositories | Optional. Below. |

### What the chart runs

control, chlive, the web, a CloudNativePG `Cluster` (`dirigent-postgres`), the ingestion (Kafka
Connect with the exactly-once ClickHouse sink, its pause server and recovery controller, from the
`durable-clickhouse-sink` subchart), optionally Garage, and the players' VMs as control starts them.

## Sign-in: Google OAuth clients

Make two OAuth clients in a Google Cloud project (APIs & Services → Credentials):

1. **Web application**, for the web's sign-in button. Authorized JavaScript origins:
   `https://dirigent.example.com`. Its client id goes first in `auth.google.clientIds`.
2. **Desktop app**, for `dirigent login`. Its id and secret go in `auth.google.cli`. (A desktop
   app's secret isn't secret: control hands it to the CLI.)

Users are `user:<their email>`. **Any verified Google account can sign in** and make sessions (paid
with their own keys). To limit it to your Google Workspace domain, require its `hd` claim:

```yaml
auth:
  google:
    requiredClaims: {hd: example.com}
```

## A first install

### Values

```yaml
# my-values.yaml
auth:
  google:
    clientIds: ["1234-web.apps.googleusercontent.com"]
    requiredClaims: {hd: example.com}
    cli:
      clientId: "1234-desktop.apps.googleusercontent.com"
      clientSecret: "GOCSPX-…"

clickhouse:
  host: clickhouse.data.svc        # its HTTP(S) interface
  port: 8443
  secure: true
  adminSecret: clickhouse-admin    # a Secret with `username` and `password`

kafka:
  bootstrapServers: kafka.data.svc:9092

checkpoints:
  garage:
    enabled: true                  # or s3.endpoint + s3.existingSecret (accessKey, secretKey)

# the ingestion is a subchart: it takes its own addresses
ingestion:
  kafka:
    bootstrapServers: kafka.data.svc:9092
  clickhouse:
    host: clickhouse.data.svc
  pauseServer:
    keeperAddress: clickhouse-keeper.data.svc:9181

web:
  ingress:
    enabled: true
    className: nginx
    host: dirigent.example.com
    tls:
      - hosts: [dirigent.example.com]
        secretName: dirigent-tls
```

With a single Kafka broker, also lower the ingestion's replication factors (they default to 3):

```yaml
ingestion:
  kafka:
    replicationFactor: 1
    internalTopicReplicationFactor: 1
  connect:
    errors:
      deadLetterQueue:
        replicationFactor: 1
```

The Kafka and ClickHouse sections accept credentials: see [Configuration](configuration.md#kafka).

### Install

```sh
kubectl create namespace dirigent
kubectl -n dirigent create secret generic clickhouse-admin \
  --from-literal=username=admin --from-literal=password=…

helm upgrade --install dirigent oci://ghcr.io/adamchol/charts/dirigent --version 0.1.10 \
  -n dirigent -f my-values.yaml --timeout 20m --wait --wait-for-jobs
```

On the first install:

1. The ingestion's hook makes the `session_events` topic (90-day retention).
2. control's init containers make the chart's Secrets (keys, passwords; see
   [Operations](operations.md#secrets-to-keep)) and run the migrations on Postgres and ClickHouse,
   including the ClickHouse users dirigent's parts use.
3. chlive waits for control, then starts.
4. A hook Job creates the S3 buckets and the kopia repository; another registers the ClickHouse sink
   connector.

`--wait-for-jobs` waits for all of it. It takes a few minutes; the first player VM also pulls the
VM image (a few GB) when the first session starts.

### Check it

```sh
kubectl -n dirigent get pods
kubectl -n dirigent logs deploy/dirigent-control | grep "control started"
```

Open `https://dirigent.example.com`, sign in, add your Anthropic API key in the account menu, and
make a session. The first one takes longest (the VM image pull and boot); `control.startup` (600
seconds) bounds how long a player may take to come up.

## Exposing the web

The web Service (`dirigent-web`, port 80) serves the UI, proxies `/api` to control and `/live` (a
WebSocket) to chlive. Expose it with:

- **An Ingress**: `web.ingress` (above). Make sure your controller doesn't time out long-lived
  WebSockets on `/live` (add its annotation through `web.ingress.annotations`).
- **A Gateway API HTTPRoute**: `web.httpRoute.enabled`, with `parentRefs` to your Gateway,
  `hostnames`, and `timeouts` if your Gateway cuts long requests.
- Anything else that reaches the Service (a Tailscale operator, a port-forward).

## Letting users' hosts in

Users' own machines running [`dirigent host`](../guides/hosts.md) need control's players' service
(gRPC, port 8091), which isn't exposed by default:

- `hosts.expose: true` routes its two paths (`/dirigent.control.v1.Players/Lease` and `/Host`) at
  the web's address: as rules on the HTTPRoute, or as a second Ingress (`dirigent-players`) on the
  web's host, whose annotations (`hosts.ingress.annotations`) must make your controller speak gRPC
  to it, e.g. `nginx.ingress.kubernetes.io/backend-protocol: GRPC`. Hosts then need only the web's
  address.
- Or reach the `dirigent-control` Service's `grpc` port privately (for instance annotate it for the
  Tailscale operator), and have hosts set `players` to that address.

Callers of that service must sign in as a user (hosts) or hold a lease token control gave them
(players), so exposing it doesn't let anyone take a session.

## GitHub App

Optional; without it, users can still give their sessions a `GITHUB_TOKEN` secret.

1. Make an App (github.com/settings/apps/new, or under an organization):
   - **Callback URL**: `https://dirigent.example.com/api/links/github/callback`, with **Request
     user authorization (OAuth) during installation** on.
   - **Webhook**: off.
   - **Repository permissions**: Contents and Pull requests, read and write.
   - **Where it can be installed**: your account only, or any account.
2. Generate a client secret and a private key, and keep them in a Secret:

   ```sh
   kubectl -n dirigent create secret generic dirigent-github-app \
     --from-literal=clientSecret=… --from-file=privateKey=<app>.private-key.pem
   ```

3. Set the values:

   ```yaml
   github:
     app:
       slug: my-dirigent          # github.com/apps/<slug>
       clientId: Iv23li…
       secret: dirigent-github-app
   ```

control reads them at start (a rotated key needs a control restart). Users connect the App in the
web's account menu; each session then gets a token for its own repository, renewed hourly.

## Next

- [Configuration](configuration.md): machines, limits, warm VMs, the players' NixOS system, and the
  rest.
- [Operations](operations.md): upgrades, logs, backups, troubleshooting.
