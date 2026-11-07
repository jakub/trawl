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
ptrace_scope=2 confirmed (34-thread dump, NoNewPrivs=1). Capture also
works under the pod's RuntimeDefault seccomp filter. Adding any
capability other than NET_BIND_SERVICE is what the Restricted Pod
Security profile refuses, and Baseline refuses SYS_PTRACE too, so an
enabled install needs a namespace that allows it, in practice
privileged. The chart documents that rather than enforcing it,
because admission policy is cluster state the chart cannot read
(ADR-0023 ruling 7, as amended). Only trawld is touched; init-auth,
init-tls-dir and trawl-web keep the unmodified securityContext.
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

{{/*
The trawl-web container's securityContext: the shared securityContext with
the sidecar's own uid, web.runAsUser (ADR-0048). In tls.mode=auto the
sidecar mounts trawld's tls directory to pin cert.pem. That directory holds
only the certificate: key.pem is in tls-key/, 0600 and owned by trawld's uid.
A separate uid keeps the key unreadable as well. The pod's runAsGroup and
fsGroup still apply, and those are what let the sidecar read its ConfigMap
and cookie Secret.
*/}}
{{- define "trawl.webSecurityContext" -}}
{{- $sc := deepCopy (.Values.securityContext | default dict) -}}
{{- $_ := set $sc "runAsUser" .Values.web.runAsUser -}}
{{- toYaml $sc -}}
{{- end }}

{{/*
Where trawld mounts its data volume. The web sidecar's certificate mount in
tls.mode=auto is a subPath of the same volume, so both derive from this.
*/}}
{{- define "trawl.dataMountPath" -}}/var/lib/trawl{{- end }}

{{/*
A whole number from 1 to .max, rendered as its exact decimal digits, or a
render failure. Values files give float64, --set gives int64, and
--set-string or a value past int64 gives a string; a cast would round,
wrap or turn a bool into 1, so normalize to digits and compare those.
Equal-length digit strings compare in numeric order.
*/}}
{{- define "trawl.wholeNumber" -}}
{{- $v := .value -}}
{{- $kind := kindOf $v -}}
{{- $s := "" -}}
{{- $hint := "" -}}
{{- if eq $kind "string" -}}
{{- $s = $v -}}
{{- else if regexMatch "^u?int" $kind -}}
{{- $s = toString $v -}}
{{- else if and (eq $kind "float64") (eq $v (floor $v)) -}}
{{- if lt $v 9007199254740992.0 -}}
{{- $s = printf "%.0f" $v -}}
{{- else -}}
{{- $hint = "; a value this large is not exact as a YAML number, so write it as a quoted string" -}}
{{- end -}}
{{- end -}}
{{- if not (and (regexMatch "^[1-9][0-9]*$" $s) (or (lt (len $s) (len .max)) (and (eq (len $s) (len .max)) (le $s .max)))) -}}
{{- fail (printf "%s (%s) must be a whole number from 1 to %s; got %v (%s)%s" .name .key .max $v $kind $hint) -}}
{{- end -}}
{{- $s -}}
{{- end }}
