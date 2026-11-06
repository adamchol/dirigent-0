{{- /* The schema migrations (openmig.toml, db/), as Postgres' superuser and ClickHouse's admin,
then ClickHouse's users for chlive and the ingestion (what scripts/clickhouse-dev-access.sql
does in dev): before control starts, every time (they're no-ops when done). */}}
{{- define "dirigent.migrate" }}
        - name: migrate
          image: {{ include "dirigent.image" (list . "migrations") }}
          imagePullPolicy: {{ .Values.images.pullPolicy }}
          command: ["bash", "-euo", "pipefail", "-c"]
          args:
            - |
              export DIRIGENT_POSTGRES_URL="postgres://postgres:${PG_PASSWORD}@dirigent-postgres-rw:5432/dirigent"
              openmig -d postgres deploy --wait-for-db 10m
              scheme=http; [ "$CH_SECURE" = true ] && scheme=https
              export DIRIGENT_CLICKHOUSE_URL="$scheme://${CH_USER}:${CH_PASSWORD}@${CH_HOST}:${CH_PORT}"
              openmig -d clickhouse deploy --wait-for-db 10m
              on=""; [ -n "$CH_CLUSTER" ] && on="ON CLUSTER '$CH_CLUSTER'"
              sql() { curl -fsS --user "$CH_USER:$CH_PASSWORD" "$scheme://$CH_HOST:$CH_PORT/" --data-binary "$1" >/dev/null; }
              sql "CREATE USER OR REPLACE chlive_ro $on IDENTIFIED WITH sha256_password BY '$CHLIVE_RO' SETTINGS readonly = 2, max_execution_time = 60"
              sql "GRANT $on dirigent_chlive_read TO chlive_ro"
              sql "GRANT $on SELECT ON system.tables TO chlive_ro"
              sql "CREATE USER OR REPLACE chlive_snap $on IDENTIFIED WITH sha256_password BY '$CHLIVE_SNAP'"
              sql "GRANT $on dirigent_chlive_snapshot TO chlive_snap"
              sql "GRANT $on SELECT ON system.tables TO chlive_snap"
              sql "CREATE USER OR REPLACE dirigent_sink $on IDENTIFIED WITH sha256_password BY '$SINK'"
              sql "GRANT $on SELECT, INSERT, CREATE TABLE ON dirigent.* TO dirigent_sink"
              sql "GRANT $on SELECT ON system.tables TO dirigent_sink"
              sql "GRANT $on SELECT ON system.merge_tree_settings TO dirigent_sink"
              sql "CREATE USER OR REPLACE dirigent_pause $on IDENTIFIED WITH sha256_password BY '$PAUSE'"
              sql "GRANT $on SELECT ON dirigent.* TO dirigent_pause"
              sql "CREATE USER OR REPLACE dirigent_recovery $on IDENTIFIED WITH sha256_password BY '$RECOVERY'"
              sql "GRANT $on SELECT, INSERT, CREATE TABLE ON dirigent.* TO dirigent_recovery"
              sql "GRANT $on SELECT ON system.tables TO dirigent_recovery"
              echo "migrated; ClickHouse's users made"
          env:
            - name: PG_PASSWORD
              valueFrom: { secretKeyRef: { name: dirigent-postgres-superuser, key: password } }
            - name: CH_HOST
              value: {{ required "clickhouse.host" .Values.clickhouse.host | quote }}
            - name: CH_PORT
              value: {{ .Values.clickhouse.port | quote }}
            - name: CH_SECURE
              value: {{ .Values.clickhouse.secure | quote }}
            - name: CH_CLUSTER
              value: {{ .Values.clickhouse.cluster | quote }}
            - name: CH_USER
              valueFrom: { secretKeyRef: { name: {{ .Values.clickhouse.adminSecret }}, key: username } }
            - name: CH_PASSWORD
              valueFrom: { secretKeyRef: { name: {{ .Values.clickhouse.adminSecret }}, key: password } }
            {{- range $env, $user := dict "CHLIVE_RO" "chlive_ro" "CHLIVE_SNAP" "chlive_snap" "SINK" "dirigent_sink" "PAUSE" "dirigent_pause" "RECOVERY" "dirigent_recovery" }}
            - name: {{ $env }}
              valueFrom: { secretKeyRef: { name: dirigent-clickhouse, key: {{ $user }} } }
            {{- end }}
{{- end }}
