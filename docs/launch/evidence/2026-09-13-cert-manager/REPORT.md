# L06 disposable certificate issuance evidence

Verified 2026-09-14 UTC from chart commit 965d945d452e9e714984c31dfc5d2a0164809399, exported with git archive. No repository changes committed.

- kind 0.33.0; Kubernetes 1.36.4 node image kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed, from https://github.com/kubernetes-sigs/kind/releases/tag/v0.33.0.
- cert-manager 1.21.1 from https://github.com/cert-manager/cert-manager/releases/download/v1.21.1/cert-manager.yaml, referenced by https://cert-manager.io/docs/installation/kubectl/; downloaded manifest SHA256 5f6a499b8c1857d57f560f536e0dcc830914b45c420899fe7ad0692c8624e408.
- Owned cluster trawl-l06-cert-manager, context kind-trawl-l06-cert-manager, dedicated kubeconfig in this directory. Every kubectl operation specified both kubeconfig and context. No existing kubeconfig/context was read or selected.
- Full chart rendered with Helm, values.yaml retained. Only certificate.yaml applied from that render. Namespace l06-cert-manager and self-signed Issuer l06-selfsigned created separately. No Trawl application resources deployed.
- Certificate l06-trawl-tls Ready=True. Secret l06-trawl-tls type kubernetes.io/tls with tls.crt and tls.key. Public keys from certificate and private key compared equal in memory; no private key written to evidence.
- Issued certificate SANs exactly l06-trawl.l06-cert-manager.svc and trawl.l06.example.test. Public certificate and openssl report retained.
- Parsed chart render assertions: Certificate metadata.name = spec.secretName = StatefulSet tls volume secretName = l06-trawl-tls. trawld tls mount /etc/trawl/tls is readOnly. ConfigMap certificate/key paths are /etc/trawl/tls/tls.crt and /etc/trawl/tls/tls.key.
- cert-manager controller/cainjector/webhook deployments Available. Runtime image IDs are in controller-pods.json.
- Cleanup: kind delete cluster completed; owned node container absent; dedicated kubeconfig removed. No application deployments, existing databases, external CA accounts, DNS records, or production cluster touched.

One CLI invocation failed: kubectl rollout status deployment --all rejects --all. Replaced with kubectl wait --for=condition=Available deployment --all; all three passed. No failed issuance.

Limits: self-signed issuance proves controller admission/reconciliation and Secret material generation; it does not prove ACME/DNS/HTTP challenges, public trust, external issuer behavior, renewal or live daemon certificate reload. Secret mount/path consistency was checked in the rendered StatefulSet/ConfigMap, not a running application pod.
