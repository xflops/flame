{{/*
Expand the chart name.
*/}}
{{- define "flame.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "flame.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "flame.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.labels" -}}
helm.sh/chart: {{ include "flame.chart" . }}
app.kubernetes.io/name: {{ include "flame.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "flame.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flame.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "flame.sessionManager.name" -}}
{{- printf "%s-session-manager" (include "flame.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.objectCache.name" -}}
{{- printf "%s-object-cache" (include "flame.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.objectCache.headlessName" -}}
{{- printf "%s-headless" (include "flame.objectCache.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.executorManager.name" -}}
{{- printf "%s-executor-manager" (include "flame.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.configMapName" -}}
{{- printf "%s-config" (include "flame.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flame.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "flame.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "flame.image" -}}
{{- $root := index . "root" -}}
{{- $image := index . "image" -}}
{{- $tag := default $root.Values.global.imageTag $image.tag -}}
{{- if $root.Values.global.imageRegistry -}}
{{- printf "%s/%s:%s" $root.Values.global.imageRegistry $image.repository $tag -}}
{{- else -}}
{{- printf "%s:%s" $image.repository $tag -}}
{{- end -}}
{{- end -}}

{{- define "flame.clusterScheme" -}}
{{- if .Values.tls.enabled -}}https{{- else -}}http{{- end -}}
{{- end -}}

{{- define "flame.cacheScheme" -}}
{{- if .Values.tls.enabled -}}grpcs{{- else -}}grpc{{- end -}}
{{- end -}}

{{- define "flame.clusterEndpoint" -}}
{{- printf "%s://%s:%d" (include "flame.clusterScheme" .) (include "flame.sessionManager.name" .) (int .Values.sessionManager.service.frontendPort) -}}
{{- end -}}

{{- define "flame.cacheEndpoint" -}}
{{- printf "%s://%s:%d" (include "flame.cacheScheme" .) (include "flame.objectCache.name" .) (int .Values.objectCache.service.port) -}}
{{- end -}}

{{- define "flame.tlsClusterPath" -}}
{{- printf "%s/cluster" .Values.tls.mountPath -}}
{{- end -}}

{{- define "flame.tlsCachePath" -}}
{{- printf "%s/cache" .Values.tls.mountPath -}}
{{- end -}}

{{- define "flame.validate" -}}
{{- if hasKey .Values.cluster.limits "maxExecutors" -}}
{{- fail "cluster.limits.maxExecutors has moved to cluster.limits.node.maxExecutors; update your Helm values" -}}
{{- end -}}
{{- if ne .Values.cluster.executors.shim "host" -}}
{{- fail "Kubernetes executor managers require cluster.executors.shim=host" -}}
{{- end -}}
{{- if .Values.tls.enabled -}}
{{- if not .Values.tls.cluster.secretName -}}
{{- fail "tls.cluster.secretName is required when tls.enabled=true" -}}
{{- end -}}
{{- if not .Values.tls.cache.secretName -}}
{{- fail "tls.cache.secretName is required when tls.enabled=true" -}}
{{- end -}}
{{- end -}}
{{- if ne (int .Values.sessionManager.replicas) 1 -}}
{{- fail "sessionManager.replicas must be 1 for the static chart" -}}
{{- end -}}
{{- if and .Values.objectCache.enabled (lt (int .Values.objectCache.replicas) 1) -}}
{{- fail "objectCache.replicas must be at least 1 when objectCache.enabled=true" -}}
{{- end -}}
{{- $expectedBackendPort := add (int .Values.sessionManager.service.frontendPort) 1 -}}
{{- if ne (int .Values.sessionManager.service.backendPort) (int $expectedBackendPort) -}}
{{- fail (printf "sessionManager.service.backendPort must equal frontendPort + 1 (%d)" (int $expectedBackendPort)) -}}
{{- end -}}
{{- end -}}

{{- define "flame.runtimeEmptyDir" -}}
{{- if or .medium .sizeLimit -}}
{{- with .medium }}
medium: {{ . | quote }}
{{- end }}
{{- with .sizeLimit }}
sizeLimit: {{ . | quote }}
{{- end }}
{{- else -}}
{}
{{- end -}}
{{- end -}}
