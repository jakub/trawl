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
{{- /* After every check above, so their failure texts win as before. */ -}}
{{- $_ := include "trawl.resolveUpstreamTrust" . -}}
{{- end -}}

{{/*
How the trawl-web sidecar reaches and verifies trawld (ADR-0048), as JSON.
Templates read it through trawl.upstreamTrust and never recompute a part.

The sidecar always dials trawld over the pod's loopback and always verifies
its certificate:

- auto: trawld writes {state_dir}/tls/cert.pem, for localhost and 127.0.0.1,
  where state_dir is the parent of config.data.path. The sidecar mounts
  that directory of the data volume read-only at the same path and pins
  the file.
  trawl-web derives its upstream, https://127.0.0.1:<port>, from [server].
- secret, certManager: the certificate names a DNS host, so trawl-web asks
  for https://<tls.upstreamServerName>:<port> and connects to
  127.0.0.1:<port> through upstream_connect_addr, verifying that name.
  tls.upstreamCa picks the trust anchor. It has no default because the
  chart cannot see inside the Secret to know whether a ca.crt is there.

Every failure names the value to set. Nothing is required while web.enabled
is false, except that auto refuses the values it would contradict.
*/}}
{{- define "trawl.resolveUpstreamTrust" -}}
{{- $tls := .Values.tls -}}
{{- $web := .Values.web -}}
{{- $trust := dict "enabled" false -}}
{{- if eq $tls.mode "auto" -}}
  {{- if $tls.upstreamServerName -}}
    {{- fail "tls.upstreamServerName is only supported when tls.mode is secret or certManager; tls.mode=auto pins trawld's generated certificate" -}}
  {{- end -}}
  {{- if $tls.upstreamCa -}}
    {{- fail "tls.upstreamCa is only supported when tls.mode is secret or certManager; tls.mode=auto pins trawld's generated certificate" -}}
  {{- end -}}
{{- end -}}
{{- if $web.enabled -}}
  {{- $_ := set $trust "enabled" true -}}
  {{- /* trawld's key.pem is 0600 and owned by trawld's uid. A sidecar with
       that uid could read it. In auto mode the key is in tls-key/, outside
       the tls/ directory the sidecar mounts, so the uid is a second
       barrier there. */ -}}
  {{- /* Kubernetes takes the container's runAsUser over the pod's when the
       key is set, 0 included, so resolve by key presence, not truthiness. */ -}}
  {{- $daemonUid := dig "runAsUser" nil (.Values.podSecurityContext | default dict) -}}
  {{- $container := .Values.securityContext | default dict -}}
  {{- if and (hasKey $container "runAsUser") (not (kindIs "invalid" $container.runAsUser)) -}}
    {{- $daemonUid = $container.runAsUser -}}
  {{- end -}}
  {{- if and (not (kindIs "invalid" $daemonUid)) (eq (int $daemonUid) (int $web.runAsUser)) -}}
    {{- fail (printf "web.runAsUser must differ from trawld's uid %d, so trawl-web cannot read trawld's private key" (int $daemonUid)) -}}
  {{- end -}}
  {{- if eq $tls.mode "auto" -}}
    {{- $data := include "trawl.dataMountPath" . -}}
    {{- $path := toString .Values.config.data.path -}}
    {{- /* trawld's state_dir is the lexical parent of [data] path, and
         clean resolves "..": /var/lib/trawl/nested/data/.. is
         /var/lib/trawl here and /var/lib/trawl/nested/data to trawld.
         Refuse dot components so the two cannot disagree. Repeated and
         trailing slashes collapse the same way in both. */ -}}
    {{- range splitList "/" $path -}}
      {{- if or (eq . ".") (eq . "..") -}}
        {{- fail (printf "config.data.path must be an absolute path without . or .. components when tls.mode=auto and web.enabled=true: trawld takes its parent without resolving .., so the chart could mount a certificate directory that trawld never writes; got %q" $path) -}}
      {{- end -}}
    {{- end -}}
    {{- $stateDir := dir (clean $path) -}}
    {{- if not (and (hasPrefix "/" $path) (or (eq $stateDir $data) (hasPrefix (printf "%s/" $data) $stateDir))) -}}
      {{- fail (printf "config.data.path must be an absolute path whose parent is %s or a directory on the data volume below it when tls.mode=auto and web.enabled=true: trawld generates its certificate in <parent>/tls, and trawl-web pins it from the data volume; got %q" $data $path) -}}
    {{- end -}}
    {{- /* Every other mount of the trawld container. One at, under, or
         above <state_dir>/tls would put the certificate trawld writes on
         another volume while trawl-web pins the data volume's copy. The
         key directory beside it, <state_dir>/tls-key, is never pinned, but
         trawld refuses a tls-key that is not a directory it owns, so no
         other volume may cover it either. */ -}}
    {{- $tlsDir := printf "%s/tls" $stateDir -}}
    {{- $keyDir := printf "%s/tls-key" $stateDir -}}
    {{- $guarded := list
          (dict "dir" $tlsDir "reason" (printf "trawld would write its certificate to that volume while trawl-web pins the data volume's %s" $tlsDir))
          (dict "dir" $keyDir "reason" "trawld keeps its generated key there, in a directory on the data volume that it owns") -}}
    {{- $mounts := list (dict "path" "/tmp" "field" "trawld's /tmp mount") (dict "path" "/etc/trawl/trawld.toml" "field" "trawld's config mount") -}}
    {{- if .Values.crashDump.enabled -}}
      {{- $mounts = append $mounts (dict "path" (toString .Values.crashDump.mountPath) "field" "crashDump.mountPath") -}}
    {{- end -}}
    {{- range $mounts -}}
      {{- $mount := clean .path -}}
      {{- $m := . -}}
      {{- range $guarded -}}
        {{- if or (eq $mount .dir) (hasPrefix (printf "%s/" $mount) .dir) (hasPrefix (printf "%s/" .dir) $mount) -}}
          {{- fail (printf "%s %q must not be at, under, or above %s when tls.mode=auto and web.enabled=true: %s" $m.field $m.path .dir .reason) -}}
        {{- end -}}
      {{- end -}}
    {{- end -}}
    {{- $subPath := "tls" -}}
    {{- if ne $stateDir $data -}}
      {{- $subPath = printf "%s/tls" (trimPrefix (printf "%s/" $data) $stateDir) -}}
    {{- end -}}
    {{- $_ := set $trust "dataSubPath" $subPath -}}
    {{- $_ := set $trust "dataMountPath" (printf "%s/tls" $stateDir) -}}
    {{- $_ := set $trust "caPath" (printf "%s/tls/cert.pem" $stateDir) -}}
  {{- else -}}
    {{- $name := toString (default "" $tls.upstreamServerName) -}}
    {{- $explicit := ne $name "" -}}
    {{- if and (not $explicit) (eq $tls.mode "certManager") -}}
      {{- range $tls.certManager.dnsNames -}}
        {{- if and (not $name) (not (hasPrefix "*." .)) -}}
          {{- $name = . -}}
        {{- end -}}
      {{- end -}}
      {{- if not $name -}}
        {{- fail "tls.upstreamServerName is required when every tls.certManager.dnsNames entry is a wildcard: name the one host trawl-web verifies in trawld's certificate" -}}
      {{- end -}}
    {{- end -}}
    {{- if not $name -}}
      {{- fail "tls.upstreamServerName is required when tls.mode=secret and web.enabled=true: the DNS name in trawld's certificate that trawl-web verifies while it connects over loopback" -}}
    {{- end -}}
    {{- if contains "*" $name -}}
      {{- fail (printf "tls.upstreamServerName must be one DNS name, not a wildcard; got %q" $name) -}}
    {{- end -}}
    {{- /* A URL host whose last label is a number parses as an IPv4
         address, and a colon means IPv6. trawl-web refuses either with
         upstream_connect_addr, so the chart names the value instead. */ -}}
    {{- if or (contains ":" $name) (regexMatch "(^|\\.)(0x[0-9a-f]*|[0-9]+)$" $name) -}}
      {{- fail (printf "tls.upstreamServerName must be a DNS name, not an IP address; got %q" $name) -}}
    {{- end -}}
    {{- if not (regexMatch "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$" $name) -}}
      {{- fail (printf "tls.upstreamServerName must be a DNS name of lowercase labels; got %q" $name) -}}
    {{- end -}}
    {{- if and $explicit (eq $tls.mode "certManager") (not (include "trawl.tlsCoversHost" (dict "host" $name "names" $tls.certManager.dnsNames))) -}}
      {{- fail (printf "tls.certManager.dnsNames must cover tls.upstreamServerName host %s" $name) -}}
    {{- end -}}
    {{- $ca := toString (default "" $tls.upstreamCa) -}}
    {{- if not $ca -}}
      {{- fail (printf "tls.upstreamCa is required when tls.mode=%s and web.enabled=true: set secret to pin ca.crt from the TLS Secret, system to use the platform roots for a publicly trusted issuer, or the absolute path of a CA file mounted into the sidecar with web.extraVolumes and web.extraVolumeMounts" $tls.mode) -}}
    {{- else if eq $ca "secret" -}}
      {{- $_ := set $trust "caSecretName" (include "trawl.tlsSecretName" .) -}}
      {{- $_ := set $trust "caMountPath" "/etc/trawl/upstream-ca" -}}
      {{- $_ := set $trust "caPath" "/etc/trawl/upstream-ca/ca.crt" -}}
    {{- else if hasPrefix "/" $ca -}}
      {{- $_ := set $trust "caPath" $ca -}}
    {{- else if ne $ca "system" -}}
      {{- fail (printf "tls.upstreamCa must be secret, system, or an absolute path; got %q" $ca) -}}
    {{- end -}}
    {{- $port := regexFind ":[0-9]+$" (toString .Values.config.server.httpAddr) -}}
    {{- if not $port -}}
      {{- fail (printf "config.server.httpAddr must end in :<port> when tls.mode=%s and web.enabled=true, because trawl-web connects to that port over loopback; got %q" $tls.mode (toString .Values.config.server.httpAddr)) -}}
    {{- end -}}
    {{- $_ := set $trust "url" (printf "https://%s%s" $name $port) -}}
    {{- $_ := set $trust "connectAddr" (printf "127.0.0.1%s" $port) -}}
  {{- end -}}
{{- end -}}
{{- toJson $trust -}}
{{- end -}}

{{/* Validated upstream trust for the templates: every TLS check runs first. */}}
{{- define "trawl.upstreamTrust" -}}
{{- include "trawl.validateTLS" . -}}
{{- include "trawl.resolveUpstreamTrust" . -}}
{{- end -}}
