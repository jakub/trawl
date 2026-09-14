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
Container image. Source checkouts require a tag; release packages supply it.
*/}}
{{- define "trawl.image" -}}
{{- $tag := required "image.tag is required: select an image built from this chart checkout" .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag }}
{{- end }}

{{/*
The trawld container's securityContext. It is the shared securityContext
plus CAP_SYS_PTRACE, added only when crash-dump capture is enabled.
trawld re-execs itself as a separate monitor process that ptraces the
crashed parent and writes the minidump, and a node at
yama/ptrace_scope=2 allows that ptrace only if the monitor holds
CAP_SYS_PTRACE effective. The runtime is what puts it there: containerd
hands an added capability to the container's init process as permitted
and effective, not bounding-only, so trawld already holds the bit before
it execs the monitor. The cap_sys_ptrace+p file capability the image
stamps on /usr/bin/trawld is what keeps the bit across that exec, and
no_new_privs does not object because nothing is gained: commoncap
downgrades only when the new permitted set is not a subset of the old
one. So allowPrivilegeEscalation stays false, which a kind run at
ptrace_scope=2 confirmed (34-thread dump, NoNewPrivs=1). Adding any
capability other than NET_BIND_SERVICE is what the Restricted Pod
Security profile refuses, so an enabled install is still incompatible
with Restricted. The chart documents that rather than enforcing it,
because admission policy is cluster state the chart cannot read
(ADR-0023 ruling 7, as amended). Only trawld is touched; init-auth and
trawl-web keep the unmodified securityContext.
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
