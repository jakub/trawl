{{/*
Expand the name of the chart.
*/}}
{{- define "trawl.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "trawl.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "trawl.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels.
*/}}
{{- define "trawl.labels" -}}
helm.sh/chart: {{ include "trawl.chart" . }}
{{ include "trawl.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "trawl.selectorLabels" -}}
app.kubernetes.io/name: {{ include "trawl.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Service account name.
*/}}
{{- define "trawl.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "trawl.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Container image with tag defaulting to appVersion.
*/}}
{{- define "trawl.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag }}
{{- end }}

{{/*
trawld container securityContext: the shared securityContext, plus
CAP_SYS_PTRACE when crash-dump capture is enabled. The crash handler forks
and ptraces itself to write a minidump, which the node's yama/ptrace_scope=2
gates behind that capability. Scoped to trawld only — init-auth and trawl-web
keep the unmodified securityContext.
*/}}
{{- define "trawl.trawldSecurityContext" -}}
{{- $sc := deepCopy .Values.securityContext -}}
{{- if .Values.crashDump.enabled -}}
{{- $caps := default (dict) $sc.capabilities -}}
{{- $_ := set $caps "add" (append (default (list) $caps.add) "SYS_PTRACE" | uniq) -}}
{{- $_ := set $sc "capabilities" $caps -}}
{{- end -}}
{{- toYaml $sc -}}
{{- end }}
