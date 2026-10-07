日本語: [../MIGRATION.md](../MIGRATION.md)

# Migrating from Ingress and Traefik

With `--migrate-to <namespace>/<name>`, the controller reads Ingress and Traefik's CRDs and adds them to that Gateway's rule set (rproxy-api docs/en/DESIGN-v0.4.md 3.3; off by default). It is for running them on the same rproxy until they move to Gateway API.

- Nothing is written to the spec of Ingress or Traefik resources. As status, an Ingress gets the target Gateway's addresses (`status.addresses`) in `status.loadBalancer.ingress` (the ADDRESS of `kubectl get ingress`, read by external-dns and the like). Traefik's CRDs have no status, so nothing is written there.
- Traefik's CRDs may be installed after the controller started: it asks the API server again every 30 seconds and starts watching kinds that appeared (no restart needed). The same goes for Gateway API's CRDs.
- The mapping is the one of rproxy-api's `contrib/traefik2rproxy.py` (docs/en/MIGRATING-FROM-TRAEFIK.md). What cannot be converted is left out and reported in the controller's log (`migration: not converted`, once each time the notes change) and in the `notes` of `rproxy-gateway render`.
- When the target Gateway has a listener on the same port, the migrated routes are added to its rule (both `http`, with or without TLS alike). Otherwise the migrated part is left out.
- Migrated routes keep Traefik's precedence (`priority`, else the length of `match`). Gateway API routes are numbered from 1, so on a shared port the migrated routes are often tried first.

## Flags

| Flag | Default | Meaning |
|---|---|---|
| `--migrate-to` | none (nothing read) | The target Gateway (`namespace/name`) |
| `--ingress-class` | `rproxy` | The Ingress class read (`spec.ingressClassName`, else the annotation `kubernetes.io/ingress.class`) |
| `--traefik-entrypoint` | `web=80,websecure=443` | Traefik entry points (`name=port[/udp]`). Routes without `entryPoints` go to every entry point of their protocol |

## Mapping

| From | rproxy |
|---|---|
| IngressRoute `match` | as it is (rproxy's `match` is Traefik v3 syntax). v2 `Headers`, `HeadersRegexp`, `HostHeader`, `Query(a=b)` are rewritten. v2 placeholders (`{name:regex}`) and other matchers are left out |
| IngressRoute `services` (Services; port by number or name) | `servers` with the pod IPs of the EndpointSlices. `scheme` (else https for port 443 or a port name starting with https), `weight`, `passHostHeader: false`. `TraefikService` is left out |
| IngressRoute `tls.secretName` | certificate files (a mounted Secret) |
| `tls.certResolver` | `{acme: <resolver>, domains: [...]}` (`domains`, else the `Host()` names). rproxy's settings file needs that resolver in `global.acme` |
| `tls.options` (TLSOption) | `tls.options` (`minVersion`, `cipherSuites`), `client_auth` (`tls.ca` / `ca.crt` of the `clientAuth.secretNames` Secrets as a file), `alpn`. `maxVersion`, `curvePreferences`, `sniStrict` are left out |
| Middleware | rproxy middlewares (`redirectScheme`, `redirectRegex`, `stripPrefix`, `addPrefix`, `replacePath(Regex)`, `headers`, `rateLimit`, `inFlightReq`, `ipAllowList`, `basicAuth` (the Secret's `users` as an htpasswd file), `forwardAuth`, `compress`, `retry`, `circuitBreaker`, `errors`, `buffering`, the CrowdSec plugin; `chain` is expanded) |
| TLS and non-TLS routers on one port | only the TLS ones (an rproxy rule is either TLS or plain) |
| IngressRouteTCP `HostSNI(*)` | `targets` (pod IPs); `proxyProtocol` as `source_ip: proxy_v1/v2` |
| IngressRouteTCP `HostSNI(name)` / `HostSNIRegexp` (plain suffixes only) | `tls.routes` (destination: the Service's ClusterIP). `sni` with `tls.passthrough`, else `terminate`. On a port with HTTP routers, only named passthrough routers are added (`passthrough: true`) |
| IngressRouteUDP | `targets` of a udp rule (one per entry point) |
| Ingress (`networking.k8s.io/v1`) | `http` routes on the `web` port, and also on the `websecure` port when it has `tls` (certificates from `secretName`). `pathType` Prefix → matched at `/` boundaries, Exact → `Path`, ImplementationSpecific → `PathPrefix`. `defaultBackend` → `http.default` |
