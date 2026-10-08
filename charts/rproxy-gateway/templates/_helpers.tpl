{{- define "rproxy-gateway.labels" -}}
app.kubernetes.io/part-of: rproxy-gateway
{{- if not .Values.rendered }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}
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

{{/* a probe's timing as `key=value,...` (--readiness-probe, --liveness-probe) */}}
{{- define "rproxy-gateway.probe" -}}
{{- $out := list -}}
{{- range $k, $v := . -}}
{{- $out = append $out (printf "%s=%d" $k (int $v)) -}}
{{- end -}}
{{- join "," $out -}}
{{- end -}}

{{/* the controller's settings: the data of the ConfigMap rproxy-gateway-config (RPROXY_GATEWAY_* of
`rproxy-gateway controller`; what used to be its arguments) */}}
{{- define "rproxy-gateway.config" -}}
RPROXY_GATEWAY_LOG_FORMAT: {{ .Values.controller.logFormat | quote }}
RPROXY_GATEWAY_CONTROLLER_NAME: {{ .Values.controller.controllerName | quote }}
RPROXY_GATEWAY_RESYNC_SECS: {{ .Values.controller.resyncSeconds | quote }}
RPROXY_GATEWAY_LISTEN_ADDR: {{ join "," .Values.rproxy.listenAddresses | quote }}
RPROXY_GATEWAY_LEADER_ELECT: {{ .Values.controller.leaderElection | quote }}
RPROXY_GATEWAY_CROSS_NAMESPACE_SECRETS: {{ .Values.controller.crossNamespaceSecrets | quote }}
{{- if .Values.controller.allowExternalNameServices }}
RPROXY_GATEWAY_ALLOW_EXTERNAL_NAME_SERVICES: "true"
{{- end }}
{{- with .Values.controller.watchNamespaces }}
RPROXY_GATEWAY_WATCH_NAMESPACES: {{ join "," . | quote }}
{{- end }}
{{- if .Values.fleet.enabled }}
RPROXY_GATEWAY_MODE: "fleet"
RPROXY_GATEWAY_FLEET_SELECTOR: "app.kubernetes.io/name=rproxy,app.kubernetes.io/component=fleet"
{{- if .Values.fleet.rproxyRules }}
RPROXY_GATEWAY_FLEET_RPROXY_RULES: "true"
{{- end }}
{{- with .Values.fleet.addresses }}
RPROXY_GATEWAY_FLEET_ADDRESS: {{ join "," . | quote }}
{{- end }}
{{- else }}
RPROXY_GATEWAY_MODE: "managed"
RPROXY_GATEWAY_RPROXY_IMAGE: {{ include "rproxy-gateway.rproxyImage" . | quote }}
RPROXY_GATEWAY_IMAGE: {{ include "rproxy-gateway.controllerImage" . | quote }}
RPROXY_GATEWAY_IMAGE_PULL_POLICY: {{ .Values.rproxy.image.pullPolicy | quote }}
RPROXY_GATEWAY_REPLICAS: {{ .Values.managed.replicas | quote }}
RPROXY_GATEWAY_SERVICE_TYPE: {{ .Values.managed.serviceType | quote }}
RPROXY_GATEWAY_NETWORK_POLICY: {{ .Values.managed.networkPolicy | quote }}
{{- with .Values.managed.externalTrafficPolicy }}
RPROXY_GATEWAY_EXTERNAL_TRAFFIC_POLICY: {{ . | quote }}
{{- end }}
RPROXY_GATEWAY_ALLOCATE_LOAD_BALANCER_NODE_PORTS: {{ .Values.managed.allocateLoadBalancerNodePorts | quote }}
RPROXY_GATEWAY_PRE_STOP_SECS: {{ int .Values.managed.preStopSeconds | quote }}
RPROXY_GATEWAY_READINESS_PROBE: {{ include "rproxy-gateway.probe" .Values.managed.readinessProbe | quote }}
RPROXY_GATEWAY_LIVENESS_PROBE: {{ include "rproxy-gateway.probe" .Values.managed.livenessProbe | quote }}
{{- with .Values.managed.addressCIDRs }}
RPROXY_GATEWAY_ADDRESS_CIDR: {{ join "," . | quote }}
{{- end }}
{{- with .Values.managed.serviceAnnotationPrefixes }}
RPROXY_GATEWAY_SERVICE_ANNOTATION_PREFIX: {{ join "," . | quote }}
{{- end }}
{{- end }}
{{- if .Values.migration.migrateTo }}
RPROXY_GATEWAY_MIGRATE_TO: {{ .Values.migration.migrateTo | quote }}
RPROXY_GATEWAY_INGRESS_CLASS: {{ .Values.migration.ingressClass | quote }}
RPROXY_GATEWAY_TRAEFIK_ENTRYPOINTS: {{ join "," .Values.migration.traefikEntryPoints | quote }}
{{- if .Values.migration.allowCrossNamespace }}
RPROXY_GATEWAY_MIGRATION_ALLOW_CROSS_NAMESPACE: "true"
{{- end }}
{{- end }}
{{- end -}}
