{{- define "rproxy-gateway.labels" -}}
app.kubernetes.io/part-of: rproxy-gateway
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}

{{- define "rproxy-gateway.controllerImage" -}}
{{ .Values.controller.image.repository }}:{{ .Values.controller.image.tag | default .Chart.AppVersion }}
{{- end }}

{{- define "rproxy-gateway.rproxyImage" -}}
{{ .Values.rproxy.image.repository }}:{{ .Values.rproxy.image.tag }}
{{- end }}
