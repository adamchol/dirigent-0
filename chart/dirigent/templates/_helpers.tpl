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

{{- define "dirigent.s3Endpoint" -}}
{{- if .Values.checkpoints.garage.enabled -}}
dirigent-garage:3900
{{- else -}}
{{- required "checkpoints.s3.endpoint (or checkpoints.garage.enabled)" .Values.checkpoints.s3.endpoint -}}
{{- end -}}
{{- end }}

{{- define "dirigent.s3Secret" -}}
{{- if .Values.checkpoints.garage.enabled -}}
dirigent-garage
{{- else -}}
{{- required "checkpoints.s3.existingSecret (or checkpoints.garage.enabled)" .Values.checkpoints.s3.existingSecret -}}
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
