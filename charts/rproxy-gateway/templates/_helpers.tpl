{{- define "rproxy-gateway.labels" -}}
app.kubernetes.io/part-of: rproxy-gateway
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}

{{- define "rproxy-gateway.controllerImage" -}}
{{- if .Values.controller.image.digest -}}
{{ .Values.controller.image.repository }}@{{ .Values.controller.image.digest }}
{{- else -}}
{{ .Values.controller.image.repository }}:{{ .Values.controller.image.tag | default .Chart.AppVersion }}
{{- end }}
{{- end }}

{{- define "rproxy-gateway.rproxyImage" -}}
{{- if .Values.rproxy.image.digest -}}
{{ .Values.rproxy.image.repository }}@{{ .Values.rproxy.image.digest }}
{{- else -}}
{{ .Values.rproxy.image.repository }}:{{ .Values.rproxy.image.tag }}
{{- end }}
{{- end }}
