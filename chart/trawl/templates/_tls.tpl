{{/* The generated Certificate and the daemon volume use the same Secret. */}}
{{- define "trawl.tlsSecretName" -}}
{{- if eq .Values.tls.mode "secret" -}}
{{- .Values.tls.secretName -}}
{{- else -}}
{{- printf "%s-tls" (include "trawl.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* Exact DNS SANs or a wildcard covering exactly one leftmost label. */}}
{{- define "trawl.tlsCoversHost" -}}
{{- $covered := false -}}
{{- $host := .host -}}
{{- range .names -}}
  {{- if eq . $host -}}
    {{- $covered = true -}}
  {{- else if and (hasPrefix "*." .) (not (hasPrefix "*." $host)) -}}
    {{- $suffix := trimPrefix "*" . -}}
    {{- if and (hasSuffix $suffix $host) (not (contains "." (trimSuffix $suffix $host))) -}}
      {{- $covered = true -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- if $covered -}}true{{- end -}}
{{- end -}}

{{- define "trawl.validateTLS" -}}
{{- $tls := .Values.tls -}}
{{- $cm := $tls.certManager -}}
{{- if and .Values.config.raw (ne $tls.mode "auto") -}}
  {{- fail "config.raw requires tls.mode=auto; use structured config values for chart-managed TLS Secret mounts" -}}
{{- end -}}
{{- if eq $tls.mode "secret" -}}
  {{- $_ := required "tls.secretName is required when tls.mode=secret" $tls.secretName -}}
{{- else if $tls.secretName -}}
  {{- fail "tls.secretName is only supported when tls.mode=secret" -}}
{{- end -}}
{{- if ne $tls.mode "certManager" -}}
  {{- if or $cm.issuerRef.name $cm.dnsNames (ne $cm.issuerRef.kind "ClusterIssuer") (ne $cm.issuerRef.group "cert-manager.io") -}}
    {{- fail "tls.certManager settings require tls.mode=certManager" -}}
  {{- end -}}
{{- else -}}
  {{- $_ := required "tls.certManager.issuerRef.name is required when tls.mode=certManager" $cm.issuerRef.name -}}
  {{- if not $cm.dnsNames -}}
    {{- fail "tls.certManager.dnsNames must list the daemon API DNS names" -}}
  {{- end -}}
  {{- range $cm.dnsNames -}}
    {{- if regexMatch "^([0-9]+\\.){3}[0-9]+$" . -}}
      {{- fail "tls.certManager.dnsNames accepts DNS names, not IP addresses; use tls.mode=secret for an IP-address certificate" -}}
    {{- end -}}
  {{- end -}}
  {{- $secret := include "trawl.tlsSecretName" . -}}
  {{- if or (eq $secret .Values.auth.database.existingSecret) (eq $secret .Values.storage.database.existingSecret) -}}
    {{- fail "generated TLS Secret name conflicts with auth.database.existingSecret or storage.database.existingSecret" -}}
  {{- end -}}
  {{- if and .Values.web.enabled (eq $secret .Values.web.cookieSecret.existingSecret) -}}
    {{- fail "generated TLS Secret name conflicts with web.cookieSecret.existingSecret" -}}
  {{- end -}}
  {{- $hosts := list -}}
  {{- if and .Values.ingress.enabled (eq .Values.ingress.backend "trawld") -}}
    {{- range .Values.ingress.hosts -}}
      {{- $hosts = append $hosts (dict "name" .host "field" "ingress.hosts") -}}
    {{- end -}}
  {{- end -}}
  {{- if .Values.httpRoute.enabled -}}
    {{- range .Values.httpRoute.hostnames -}}
      {{- $hosts = append $hosts (dict "name" . "field" "httpRoute.hostnames") -}}
    {{- end -}}
  {{- end -}}
  {{- if .Values.ingress.enabled -}}
    {{- range .Values.ingress.tls -}}
      {{- if eq .secretName $secret -}}
        {{- if or (hasKey $.Values.ingress.annotations "cert-manager.io/issuer") (hasKey $.Values.ingress.annotations "cert-manager.io/cluster-issuer") (eq (toString (get $.Values.ingress.annotations "kubernetes.io/tls-acme")) "true") -}}
          {{- fail "ingress cert-manager annotations must not request the chart-managed TLS Secret; tls.mode=certManager already creates its Certificate" -}}
        {{- end -}}
        {{- range .hosts -}}
          {{- $hosts = append $hosts (dict "name" . "field" "ingress.tls.hosts") -}}
        {{- end -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
  {{- range $hosts -}}
    {{- if not (include "trawl.tlsCoversHost" (dict "host" .name "names" $cm.dnsNames)) -}}
      {{- fail (printf "tls.certManager.dnsNames must cover %s host %s" .field .name) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- end -}}
