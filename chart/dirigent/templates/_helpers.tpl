{{- define "dirigent.labels" -}}
app.kubernetes.io/name: dirigent
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end }}

{{- define "dirigent.selector" -}}
app.kubernetes.io/name: dirigent
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/* An image of the chart's: (list $ "control") */}}
{{- define "dirigent.image" -}}
{{- $root := index . 0 -}}
{{- printf "%s/dirigent-%s:%s" $root.Values.images.registry (index . 1) (default $root.Chart.AppVersion $root.Values.images.tag) -}}
{{- end }}

{{/* A password generated once, kept across upgrades: (list $ "secret-name" "key"); 64 hex digits,
as Garage's RPC secret must be */}}
{{- define "dirigent.password" -}}
{{- $root := index . 0 -}}
{{- $found := lookup "v1" "Secret" $root.Release.Namespace (index . 1) -}}
{{- if and $found (hasKey $found.data (index . 2)) -}}
{{- index $found.data (index . 2) | b64dec -}}
{{- else -}}
{{- randAlphaNum 32 | sha256sum -}}
{{- end -}}
{{- end }}

{{- define "dirigent.s3Endpoint" -}}
{{- if .Values.checkpoints.garage.enabled -}}
dirigent-garage:3900
{{- else -}}
{{- required "checkpoints.s3.endpoint (or checkpoints.garage.enabled)" .Values.checkpoints.s3.endpoint -}}
{{- end -}}
{{- end }}

{{- define "dirigent.clickhouseUrl" -}}
{{- printf "%s://%s:%v" (ternary "https" "http" .Values.clickhouse.secure) (required "clickhouse.host" .Values.clickhouse.host) .Values.clickhouse.port -}}
{{- end }}

{{- define "dirigent.pauseServerUrl" -}}
{{- if .Values.ingestion.enabled -}}
{{- printf "http://%s-pause-server:%v" .Values.ingestion.fullnameOverride (default 8080 .Values.ingestion.pauseServer.port) -}}
{{- else -}}
{{- required "ingestion.pauseServerUrl (ingestion disabled)" .Values.ingestion.pauseServerUrl -}}
{{- end -}}
{{- end }}

{{- define "dirigent.pullSecrets" -}}
{{- with .Values.images.pullSecrets }}
imagePullSecrets:
{{- range . }}
  - name: {{ . }}
{{- end }}
{{- end }}
{{- end }}
