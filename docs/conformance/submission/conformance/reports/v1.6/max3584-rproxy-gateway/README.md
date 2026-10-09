# rproxy-gateway

[rproxy-gateway](https://github.com/max3584/rproxy-gateway) is a Kubernetes controller, written in Rust, that implements the Gateway API on top of [rproxy](https://github.com/max3584/rproxy-api), a reverse proxy for HTTP, TLS, TCP and UDP. It turns Gateways and routes into rproxy rule sets and applies them through rproxy's control API.

## Table of Contents

| API channel  | Implementation version                                                         | Mode    | Report                                                       |
|--------------|--------------------------------------------------------------------------------|---------|--------------------------------------------------------------|
| experimental | [v0.4.5](https://github.com/max3584/rproxy-gateway/releases/tag/v0.4.5)        | default | [v0.4.5 report](./experimental-v0.4.5-default-report.yaml)   |

## Reproduce

The report comes from the released Helm chart and images (nothing is built): the chart
`oci://ghcr.io/max3584/charts/rproxy-gateway` 0.4.5 with its default images
`ghcr.io/max3584/rproxy-gateway:0.4.5` (the controller) and `ghcr.io/max3584/rproxy-gateway/rproxy:0.4.3`
(the data plane it provisions per Gateway). The supported features are not passed on the command line:
the suite reads them from the GatewayClass status that the controller writes.

You need a Linux host with Docker, [kind](https://kind.sigs.k8s.io) (v0.33.0 was used), kubectl, Helm 3.8+,
Go and git. The suite runs on the host and reaches each Gateway's address (the ClusterIP of the Service the
controller creates for it) through a route to the kind node.

1. Create a cluster and install the experimental-channel Gateway API v1.6.3 CRDs (some of the claimed
   features, `HTTPRouteRetry*`, use experimental fields):

   ```shell
   kind create cluster --name rproxy-gateway-conformance --wait 180s
   kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/experimental-install.yaml
   ```

2. Route the cluster's Service network through the kind node, so that the suite can reach the Gateways'
   ClusterIPs:

   ```shell
   node_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' rproxy-gateway-conformance-control-plane)
   svc_cidr=$(kubectl get svc kubernetes -o jsonpath='{.spec.clusterIP}' | awk -F. '{print $1"."$2".0.0/16"}')
   sudo ip route replace "$svc_cidr" via "$node_ip"
   ```

3. Install the released chart. kind has no load balancer, so the Gateways' Services are `ClusterIP`.
   `managed.addressCIDRs` allows static addresses in `192.0.2.0/24` (off by default), for the
   `GatewayStaticAddresses` test's usable address:

   ```shell
   helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway --version 0.4.5 \
     --namespace rproxy-gateway-system --create-namespace --wait \
     --set managed.serviceType=ClusterIP \
     --set 'managed.addressCIDRs={192.0.2.0/24}'
   kubectl wait --for=condition=Accepted gatewayclass/rproxy --timeout=120s
   ```

4. Run the suite of Gateway API v1.6.3 with all five profiles:

   ```shell
   git clone --depth 1 --branch v1.6.3 https://github.com/kubernetes-sigs/gateway-api.git
   cd gateway-api
   go test ./conformance -run TestConformance -count=1 -timeout 110m -v -args \
     --gateway-class=rproxy \
     --conformance-profiles=GATEWAY-HTTP,GATEWAY-GRPC,GATEWAY-TLS,GATEWAY-TCP,GATEWAY-UDP \
     --organization=max3584 \
     --project=rproxy-gateway \
     --url=https://github.com/max3584/rproxy-gateway \
     --version=v0.4.5 \
     --contact=https://github.com/max3584/rproxy-gateway/issues \
     --report-output="$PWD/experimental-v0.4.5-default-report.yaml" \
     --usable-address=192.0.2.10 \
     --unusable-address=0.0.0.0
   ```

The same steps are automated on the project's main branch: `scripts/conformance-report.sh`, run by hand in
GitHub Actions as `gh workflow run e2e.yml -f report_version=0.4.5` (the report is the run's artifact,
together with the suite's log and the image digests).
