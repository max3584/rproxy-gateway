English: [en/PLATFORM.md](en/PLATFORM.md)

# rproxy-gateway にアドレスを渡す（プラットフォームの書き方）

rproxy-gateway はアドレス（VIP）を自分では持たない。アドレスを用意してノードや Pod に届け、障害のときに移すのはプラットフォーム（MetalLB・kube-vip・Cilium・クラウドのロードバランサ・ノードの keepalived・自前の L4 ロードバランサ）の仕事で、rproxy-gateway は rproxy をそのアドレスで待ち受けさせるだけ。この文書は、よくある形ごとに、プラットフォームの側の書き方（そのまま使えるマニフェスト）と、受け入れテストで測った途切れを並べる。

- v0.4.4 の `fleet.vip`（組み込みの VIP のサイドカー）は v0.4.5 で外した（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 7.）。移り方は最後の「[v0.4.4 の `fleet.vip` から移る](#v044-の-fleetvip-から移る)」。
- 例のアドレスは文書用（`192.0.2.0/24`・`198.51.100.0/24`）。プラットフォームのマニフェストの API の版は、それぞれの版のドキュメントで確かめる。

## 選び方

| 形 | アドレスを用意するもの | Pod の入れ替え（削除・drain・rollout）の途切れ | ノードの喪失 | クライアントの IP |
|---|---|---|---|---|
| [managed + LoadBalancer（MetalLB L2）](#metallb-l2) | MetalLB（ARP / NDP） | 0.2〜1.2 秒（`Local`） | 5〜8 秒（memberlist） | 届く（`Local`） |
| [managed + LoadBalancer（MetalLB BGP + BFD）](#metallb-bgp--bfd) | MetalLB と上流のルータ（ECMP） | 2.3〜3.5 秒（`Local`）、0.2 秒（`Cluster`） | 3.3 秒（`Local`、BFD） | 届く（`Local`） |
| [managed + LoadBalancer（kube-vip）](#kube-vipservices-のモード) | kube-vip（ARP、Service ごとの Lease） | 測っていない | Lease の期限 | 届く（`Local`） |
| [managed + LoadBalancer（Cilium）](#cilium-の-lb-ipam--l2-announcements) | Cilium の LB IPAM・L2 announcements（または BGP） | 測っていない | Lease の期限 | `Cluster` を勧める（届かない） |
| [managed + LoadBalancer（クラウド）](#クラウドのロードバランサ) | クラウドのロードバランサ | 測っていない（ヘルスチェックの設定による） | ヘルスチェックの設定による | 種類による |
| [managed + NodePort + 自前の L4](#nodeport--自前の-l4-ロードバランサhaproxy) | HAProxy など | 0.1〜0.2 秒（`Cluster`）、2.2 秒（`Local`） | 6.4 秒〜（ヘルスチェック） | `Local` だけ |
| [fleet（hostNetwork）](#fleethostnetwork) | 自前の L4・keepalived・MetalLB / kube-vip / Cilium の Service | 測っていない（ヘルスチェック・VRRP の設定による） | 同左 | 届く（L4 の転送による） |
| [ClusterIP](#clusteripクラスタの中だけ) | Kubernetes の Service | Service の endpoint の切り替え | — | 届く |

受け入れテスト（kind 1+3 ノード、`managed.replicas=2`、HTTP・HTTPS・TCP を 100 ms ごとに新しい接続で）の、通らなかった最も長い間（秒）。README の「可用性」と同じ値：

| 形（`TOPOLOGY`） | Pod の削除 | 告知するノードの Pod の削除 | drain | rollout restart | ノードが止まる |
|---|---|---|---|---|---|
| MetalLB L2、`Local`（既定） | 0.2 | 0.3 | 1.2（drain 16.5 秒） | 0.2 | 7.8 |
| MetalLB L2、`Cluster` | 0.2 | 0.2 | 0.2 | 0.2 | 9.0（約 59 秒まで一部が落ちる） |
| MetalLB BGP + ECMP（BFD）、`Local` | 3.5 | — | 2.3 | 2.3 | 3.3 |
| MetalLB BGP + ECMP（BFD）、`Cluster` | 0.2 | — | 0.2 | 0.1 | 13.3（約 60 秒まで一部が落ちる） |
| NodePort + 自前の L4（HAProxy）、`Local` | 0.2 | — | 2.2 | 2.2 | 6.4 |
| NodePort + 自前の L4（HAProxy）、`Cluster` | 0.1 | — | 0.1 | 0.2 | 20.2（約 63 秒まで一部が落ちる） |

- rproxy-gateway が決めるのは Pod の入れ替えの途切れ（readiness gate、`/readyz` と `shutdown.delay`・`drain`、PDB）で、受け入れテストはこれを `GAP_LIMIT`（3 秒）で落とす。**ネットワークの側の時間**（BGP の経路の取り下げ・BFD、ARP の移動、ノードの死の見つけ方、クラウドのヘルスチェック）はプラットフォームの設定で決まるので、受け入れテストでも記録するだけ（`bgp` とノードの喪失）。表の値はその環境での一例。
- ノードが止まったとき、そのノードの **backend** の Pod も NotReady になるまで EndpointSlice に残る（どの形でも 40〜60 秒の一部の失敗が続くことがある）。backend には `RproxyPolicy` の `outlierDetection` を使う。
- どの形でも、rproxy が止まるときの `shutdown.delay`（managed 15 秒、fleet 5 秒）は、ロードバランサがその Pod・ノードを外すまでの時間（ヘルスチェックの間隔 × 回数、ARP・経路の移動）より長くする。

## managed + LoadBalancer

既定（`managed.serviceType: LoadBalancer`）では、Gateway ごとに `LoadBalancer` の Service（`rproxy-<id>`、Gateway の namespace）を作る。アドレスはクラスタのロードバランサの実装が Service に付け、Gateway の `status.addresses` に出る。`externalTrafficPolicy` は既定で `Local`（クライアントの IP が届き、ready な Pod のあるノードだけが受ける）。

アドレスを Gateway ごとに決める方法は 2 つ：

- **ロードバランサの注釈**（`metallb.io/loadBalancerIPs`・`kube-vip.io/loadbalancerIPs`・`lbipam.cilium.io/ips` など）を Gateway の `spec.infrastructure.annotations` に書く。アドレスを決める注釈は既定で Service に付けない（テナントがほかのアドレスを取れるため。[SECURITY.md](SECURITY.md)）。テナントを信頼するなら `managed.serviceAnnotationPrefixes` で開け、アドレスのプールを namespace ごとに分ける（下の MetalLB の `serviceAllocation`、Cilium の `serviceSelector`）。
- **Gateway の `spec.addresses`**：Service の `externalIPs` になる（`managed.addressCIDRs` の内だけ）。そのアドレスへの通信をノードに届けるのは別の仕組み（上流のルータの静的経路、Cilium の L2 announcements の `externalIPs: true`、ノードの keepalived）。MetalLB と kube-vip は `externalIPs` を告知しない。

### MetalLB L2

```yaml
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata: {name: gateways, namespace: metallb-system}
spec:
  addresses: [192.0.2.100-192.0.2.120]
  # namespace ごとに分けるなら（MetalLB 0.14 以降）
  # serviceAllocation: {priority: 50, namespaces: [team-a]}
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata: {name: gateways, namespace: metallb-system}
spec:
  ipAddressPools: [gateways]
```

```yaml
# rproxy-gateway の chart の値
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false   # MetalLB は node port を使わない
  # Gateway ごとにアドレスを選ばせるなら（テナントを信頼するときだけ）
  # serviceAnnotationPrefixes: [metallb.io/]
```

```yaml
# Gateway でアドレスを選ぶ（serviceAnnotationPrefixes で開けたとき）
spec:
  infrastructure:
    annotations: {metallb.io/loadBalancerIPs: 192.0.2.101}
```

- 告知するノードは Service ごとに 1 つ。`Local` では ready な rproxy の Pod のあるノードだけが告知する。告知するノードを移すとき、その瞬間の接続を 1 つ落とすことがある（drain の 1.2 秒）。
- ノードの死は memberlist で見つける（5〜8 秒）。速くするには MetalLB の memberlist の設定か BGP + BFD。

### MetalLB BGP + BFD

```yaml
apiVersion: metallb.io/v1beta1
kind: BFDProfile
metadata: {name: fast, namespace: metallb-system}
spec: {receiveInterval: 300, transmitInterval: 300, detectMultiplier: 3}
---
apiVersion: metallb.io/v1beta2
kind: BGPPeer
metadata: {name: tor, namespace: metallb-system}
spec:
  myASN: 64512
  peerASN: 64513
  peerAddress: 198.51.100.1
  bfdProfile: fast        # BFD は MetalLB の FRR モード（または frr-k8s）
---
apiVersion: metallb.io/v1beta1
kind: BGPAdvertisement
metadata: {name: gateways, namespace: metallb-system}
spec:
  ipAddressPools: [gateways]
```

- ルータは同じ経路を複数のノードへ配る（ECMP、FRR なら `maximum-paths`）。BFD でノードの死は 1 秒ほどで経路から外れる。
- `Local` では、MetalLB は終了中の Pod のあるノードの経路を Pod が消えてから取り下げる（FRR モードで 3〜4 秒）ので、その分が落ちる。計画した入れ替えをほぼ 0 にしたいなら `Cluster`（`managed.externalTrafficPolicy: Cluster`、Gateway ごとなら parameters の `service.externalTrafficPolicy`）。ただしクライアントの IP は届かず、ノードが止まるとそのノードの Pod が endpoint から外れるまで（40〜50 秒）一部が落ちる。
- 経路の収束（BFD・タイマー・ルータの ECMP の扱い）はネットワークの側の設定。

### kube-vip（services のモード）

kube-vip を DaemonSet で動かし、`LoadBalancer` の Service のアドレスを ARP で告知させる。アドレスの払い出しは kube-vip-cloud-provider。

```bash
kubectl apply -f https://kube-vip.io/manifests/rbac.yaml
kubectl apply -f https://raw.githubusercontent.com/kube-vip/kube-vip-cloud-provider/main/manifest/kube-vip-cloud-controller.yaml
kubectl -n kube-system create configmap kubevip --from-literal range-global=192.0.2.100-192.0.2.120
```

```yaml
# kube-vip manifest daemonset --services --inCluster --arp --servicesElection でも作れる
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: kube-vip-ds, namespace: kube-system}
spec:
  selector: {matchLabels: {app.kubernetes.io/name: kube-vip-ds}}
  template:
    metadata: {labels: {app.kubernetes.io/name: kube-vip-ds}}
    spec:
      serviceAccountName: kube-vip
      hostNetwork: true
      containers:
        - name: kube-vip
          image: ghcr.io/kube-vip/kube-vip:v1.0.1   # その時の版に
          args: [manager]
          env:
            - {name: vip_arp, value: "true"}
            - {name: svc_enable, value: "true"}
            - {name: svc_election, value: "true"}   # Service ごとに持ち主を選ぶ（ノードに散る）
            - {name: vip_leaseduration, value: "5"}
            - {name: vip_renewdeadline, value: "3"}
            - {name: vip_retryperiod, value: "1"}
          securityContext:
            capabilities: {add: [NET_ADMIN, NET_RAW]}
```

- アドレスを Gateway で選ぶ注釈は `kube-vip.io/loadbalancerIPs`（`managed.serviceAnnotationPrefixes: [kube-vip.io/]` で開ける）。
- `svc_election` では Service ごとの Lease で告知するノードが決まり、ノードの喪失は Lease の期限（上の例で 5 秒）まで。`Local` では ready な Pod のあるノードが選ばれる。rproxy-gateway の受け入れテストでは測っていない。

### Cilium の LB IPAM + L2 announcements

Cilium の helm の値で L2 announcements を有効にする（`kubeProxyReplacement: true` が要る）：

```yaml
kubeProxyReplacement: true
l2announcements:
  enabled: true
  leaseDuration: 3s
  leaseRenewDeadline: 1s
  leaseRetryPeriod: 200ms
externalIPs:
  enabled: true          # Gateway の spec.addresses（externalIPs）も告知するなら
k8sClientRateLimit: {qps: 50, burst: 100}   # Lease の更新が API サーバに届くように（Service の数に合わせる）
```

```yaml
apiVersion: cilium.io/v2alpha1          # Cilium の版により cilium.io/v2
kind: CiliumLoadBalancerIPPool
metadata: {name: gateways}
spec:
  blocks: [{start: 192.0.2.100, stop: 192.0.2.120}]
  # namespace ごとに分けるなら
  # serviceSelector: {matchLabels: {io.kubernetes.service.namespace: team-a}}
---
apiVersion: cilium.io/v2alpha1
kind: CiliumL2AnnouncementPolicy
metadata: {name: gateways}
spec:
  loadBalancerIPs: true
  externalIPs: true
  interfaces: ["^eth[0-9]+"]
  nodeSelector:
    matchExpressions: [{key: node-role.kubernetes.io/control-plane, operator: DoesNotExist}]
```

```yaml
# rproxy-gateway の chart の値
managed:
  replicas: 2
  externalTrafficPolicy: Cluster   # L2 announcements は告知するノードを Pod の場所と関係なく選ぶ
  allocateLoadBalancerNodePorts: false
```

- アドレスを Gateway で選ぶ注釈は `lbipam.cilium.io/ips`（`managed.serviceAnnotationPrefixes: [lbipam.cilium.io/]`）。
- 告知するノードは Service ごとの Lease で決まり、ノードの喪失は Lease の期限まで。BGP にするなら Cilium の BGP control plane（MetalLB の BGP と同じ見方）。rproxy-gateway の受け入れテストでは測っていない。

### クラウドのロードバランサ

クラウドのロードバランサの注釈（`service.beta.kubernetes.io/`・`cloud.google.com/` など）もテナントには既定で付けさせない。全 Gateway に同じものを付けるなら、管理者がクラスの既定の parameters（chart の `managed.parameters`）に書く（先に CRD を入れる。README の「入れ方」）。

```yaml
# AWS（AWS Load Balancer Controller の NLB、Pod の IP を直接ターゲットに）
managed:
  parameters:
    service:
      loadBalancerClass: service.k8s.aws/nlb
      annotations:
        service.beta.kubernetes.io/aws-load-balancer-nlb-target-type: ip
        service.beta.kubernetes.io/aws-load-balancer-scheme: internet-facing
```

```yaml
# GKE（バックエンド サービスのネットワーク ロードバランサ）
managed:
  parameters:
    service:
      annotations: {cloud.google.com/l4-rbs: enabled}
```

```yaml
# Azure（内部のロードバランサ）
managed:
  parameters:
    service:
      annotations: {service.beta.kubernetes.io/azure-load-balancer-internal: "true"}
```

- `Local` では、ロードバランサは Service の `healthCheckNodePort` でノードを見て、ready な Pod のないノードを外す。Pod の IP をターゲットにする形（AWS の `ip`）では、endpoint から外れた Pod をコントローラがターゲットから外す。
- `managed.shutdown.delay`（既定 15 秒）を、ロードバランサがターゲットを外すまで（ヘルスチェックの間隔 × 回数、登録解除の時間）より長くする。足りなければ parameters の `rproxy.shutdown.delay` で延ばす。
- 途切れはクラウドのヘルスチェック・登録解除の設定で決まる（rproxy-gateway の受け入れテストでは測っていない）。

## NodePort + 自前の L4 ロードバランサ（HAProxy）

```yaml
# rproxy-gateway の chart の値
managed:
  replicas: 2
  serviceType: NodePort   # externalTrafficPolicy は既定で Cluster
```

Gateway ごとの node port は Kubernetes が選ぶ：

```bash
kubectl -n default get svc -l gateway.networking.k8s.io/gateway-name=web \
  -o jsonpath='{range .items[0].spec.ports[*]}{.port} -> {.nodePort}{"\n"}{end}'
```

```haproxy
# 受け入れテスト（nodeport-lb）と同じ設定：500 ms ごとに確かめ、2 回で外す。
# つながらなかった接続はすぐそのノードを外して別のノードへ
defaults
  mode tcp
  timeout connect 200ms
  timeout client 30s
  timeout server 30s
  timeout check 500ms
  retries 3
  option redispatch 1
  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions observe layer4 error-limit 1 on-error mark-down

frontend https
  bind :443
  default_backend gw-https
backend gw-https
  balance roundrobin
  server node1 198.51.100.11:30443 check
  server node2 198.51.100.12:30443 check
  server node3 198.51.100.13:30443 check
```

- `Cluster`（既定）では、どのノードも ready な rproxy の Pod へ送るので、Pod の入れ替えはほぼ途切れない（0.1〜0.2 秒）。クライアントの IP は届かない。ノードが止まると、そのノードの Pod が endpoint から外れるまで（約 60 秒）一部が落ちる。
- `Local` にする（クライアントの IP を届ける）なら、NodePort には `healthCheckNodePort` がないので、ロードバランサは rproxy が止まってからでないとノードを外せない（2 秒ほど落ちる）。Service を `LoadBalancer` 型にすれば（ロードバランサの実装がなくても）`healthCheckNodePort` が付くので、そこを HTTP で確かめる：

```haproxy
backend gw-https
  balance roundrobin
  option httpchk GET /healthz
  # 32000 は Service の spec.healthCheckNodePort（kube-proxy が答える。ready な Pod がなければ 503）
  server node1 198.51.100.11:30443 check port 32000
  server node2 198.51.100.12:30443 check port 32000
  server node3 198.51.100.13:30443 check port 32000
```

## fleet（hostNetwork）

`fleet.enabled=true` では、chart の DaemonSet の rproxy がノードのネットワーク（`hostNetwork: true`）で、すべての Gateway のリスナーを `rproxy.listenAddresses`（既定 `0.0.0.0`。IPv6 には `::` を足す）で待ち受ける。ノードに届いた通信は、どのアドレス宛てでもポートが合えば受ける。Service はないので、アドレスはノードの IP か、ノードの前・ノードの上にプラットフォームが用意する。

- Gateway の `status.addresses` に出すアドレスは `fleet.addresses`（空ならノードの IP）。下の VIP やロードバランサのアドレスを書く。
- rproxy は止まるとき `fleet.shutdown.delay`（既定 5 秒）の間 `https://<ノードの IP>:9443/readyz` が 503（draining）を返しながら受け付けを続ける。ヘルスチェックをここに向け、外すまでの時間（間隔 × 回数）を delay より短くする。9443 は制御 API のポート（ノードの IP だけで待ち受ける）なので、外からはロードバランサだけが届くようにファイアウォールで絞る（[SECURITY.md](SECURITY.md) の「ネットワーク」）。
- ここの形は rproxy-gateway の受け入れテストでは測っていない（途切れはヘルスチェック・VRRP・ロードバランサの設定で決まる）。

### 自前の L4 ロードバランサ（HAProxy、`/readyz` で確かめる）

```haproxy
defaults
  mode tcp
  timeout connect 200ms
  timeout client 30s
  timeout server 30s
  timeout check 500ms
  retries 3
  option redispatch 1
  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions

frontend https
  bind :443
  default_backend fleet-https
backend fleet-https
  balance roundrobin
  # rproxy の /readyz（止まり始めると 503）。証明書はコントローラの CA なので verify none か ca-file
  option httpchk GET /readyz
  http-check expect status 200
  server node1 198.51.100.11:443 check port 9443 check-ssl verify none
  server node2 198.51.100.12:443 check port 9443 check-ssl verify none
  server node3 198.51.100.13:443 check port 9443 check-ssl verify none
```

```yaml
# rproxy-gateway の chart の値
fleet:
  enabled: true
  addresses: [192.0.2.10]   # HAProxy のアドレス
```

- 500 ms × 2 回で外れるので、既定の delay 5 秒で止まる前に外れる。クライアントの IP は届かない（PROXY protocol は使わない）。

### ノードの keepalived（VRRP）

ノードの OS に keepalived を入れ、VIP を VRRP で持たせる。rproxy が ready でないノードは `track_script` で外れる。

```text
# /etc/keepalived/keepalived.conf（ノードごと。NODE_IP はそのノードの IP、priority はノードごとに変える）
global_defs {
  enable_script_security
  script_user root
}
vrrp_script rproxy_ready {
  script "/usr/bin/curl -skf --max-time 1 https://NODE_IP:9443/readyz"
  interval 1
  fall 2
  rise 2
}
vrrp_instance gateway {
  state BACKUP
  interface eth0
  virtual_router_id 51
  priority 100
  advert_int 1
  nopreempt
  virtual_ipaddress {
    192.0.2.10/32 dev eth0
  }
  track_script {
    rproxy_ready
  }
}
```

```yaml
# rproxy-gateway の chart の値
fleet:
  enabled: true
  addresses: [192.0.2.10]
```

- 予定の移動（rollout・drain）は、`/readyz` が 503 になってから `fall` × `interval`（上で 2 秒）で VIP が移る（既定の delay 5 秒の内）。ノードの喪失は VRRP の master の死の見つけ方（`advert_int` の 3 倍ほど）。
- VIP を複数にしてノードに散らし（`vrrp_instance` を VIP ごとに、`priority` を変えて）、DNS で配ると active-active になる。
- UDP の応答は届いたアドレス（VIP）から返る（rproxy が `IP_PKTINFO` で）。

### MetalLB・kube-vip・Cilium（fleet の Pod を選ぶ Service）

ロードバランサの実装があるクラスタでは、fleet の Pod を選ぶ `LoadBalancer` の Service を管理者が 1 つ書けば、その実装が VIP を告知する。`externalTrafficPolicy: Local` で、ready な fleet の Pod のあるノードだけが受ける（終了中の Pod は endpoint で ready でなくなるので、rproxy が受け付けを続けている delay の間に告知が移る）。

```yaml
apiVersion: v1
kind: Service
metadata:
  name: rproxy-fleet
  namespace: rproxy-gateway-system
  annotations: {metallb.io/loadBalancerIPs: 192.0.2.10}   # kube-vip: kube-vip.io/loadbalancerIPs、Cilium: lbipam.cilium.io/ips
spec:
  type: LoadBalancer
  externalTrafficPolicy: Local
  selector: {app.kubernetes.io/name: rproxy, app.kubernetes.io/component: fleet}
  # Gateway のリスナーのポートをすべて書く（足したら Service も直す）
  ports:
    - {name: http, port: 80, protocol: TCP}
    - {name: https, port: 443, protocol: TCP}
    - {name: dns, port: 53, protocol: UDP}
```

```yaml
# rproxy-gateway の chart の値
fleet:
  enabled: true
  addresses: [192.0.2.10]
```

- 管理者の Service なので、アドレスの注釈はテナントの制限（`serviceAnnotationPrefixes`）と関係なく書ける。
- 切り替えの時間は上の managed の各形と同じ見方（MetalLB L2 の告知の移動・memberlist、BGP の取り下げ、kube-vip・Cilium の Lease）。Cilium の L2 announcements は告知するノードを Pod の場所と関係なく選ぶので `Cluster` にする（クライアントの IP は届かない）。
- リスナーのポートを Service に並べる手間がある。ポートが多い・よく変わるなら keepalived か自前の L4 の形のほうが楽。

### Gateway の `spec.addresses` と `addressCIDRs`

- `fleet.listen: wildcard`（既定）：fleet の Gateway の `spec.addresses` は `fleet.addresses` のどれかだけ（ほかは `Programmed: False`、`AddressNotUsable`）。rproxy は `0.0.0.0` で待ち受けるので、VIP を複数用意して `fleet.addresses` に並べても、同じポートを別の Gateway に使い分けることはできない（どの VIP に来てもポートが合えば受ける）。
- `fleet.listen: addresses`（v0.4.5、rproxy v0.4.3 の `listen_freebind`）：`spec.addresses` を持つ Gateway のルールは、そのアドレスでだけ待ち受ける（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 12.）。アドレスは `managed.addressCIDRs` の内だけ（chart が `--address-cidr` に渡す）。VIP がまだそのノードに来ていなくても、すべての fleet の Pod が待ち受けるので、上の keepalived・Service の VIP をそのまま Gateway ごとに使い分けられる（VIP が違えば同じポートを使える。あるアドレスに来たものはその Gateway にだけ届く）。`spec.addresses` のない Gateway は `0.0.0.0` のまま。同じアドレス（か `0.0.0.0`）の同じポートは古い Gateway が持ち、後の Gateway のリスナーは `Accepted: False`（`PortUnavailable`）。Gateway の `status.addresses` は `spec.addresses`。

```yaml
fleet:
  enabled: true
  listen: addresses
managed:
  addressCIDRs: [192.0.2.10/32, 192.0.2.11/32]   # keepalived・MetalLB などが置く VIP
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web-a, namespace: team-a}
spec:
  gatewayClassName: rproxy
  addresses: [{type: IPAddress, value: 192.0.2.10}]
  listeners: [{name: https, port: 443, protocol: HTTPS, tls: {certificateRefs: [{name: web-a}]}}]
```

## ClusterIP（クラスタの中だけ）

クラスタの外に出さない Gateway（クラスタの中のクライアント向けの入口）は `ClusterIP` にする。

```yaml
# 全 Gateway なら chart の値
managed:
  serviceType: ClusterIP
```

```yaml
# Gateway ごとなら RproxyGatewayParameters（service.type はクラスの policy で開ける）
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {name: internal, namespace: default}
spec:
  service: {type: ClusterIP}
```

- Gateway の `status.addresses` は Service の ClusterIP。名前で使うなら Service（`rproxy-<id>`）を `kubectl -n default get svc -l gateway.networking.k8s.io/gateway-name=web` で調べ、`<Service>.<namespace>.svc` を使う。
- 切り替えは Service の endpoint（kube-proxy・CNI）で、Pod の入れ替えは readiness gate と `/readyz` で外れる。

## v0.4.4 の `fleet.vip` から移る

`fleet.vip` は v0.4.4 だけにあった（既定で切っていた）。v0.4.5 の chart は、値に `fleet.vip` があると `helm upgrade` を止める（`fleet.vip was removed in rproxy-gateway 0.4.5 ...`）。

1. 代わりのアドレスを先に用意する（上の keepalived・自前の L4・fleet の Pod を選ぶ Service のどれか）。同じ VIP を使うなら、`fleet.vip` を外した後に新しい仕組みで持たせる。
2. 値から `fleet.vip` を消し、`fleet.addresses` に VIP を書いて `helm upgrade` する。chart が作っていたもの（ServiceAccount・Role・RoleBinding・ClusterRole・ClusterRoleBinding `rproxy-gateway-vip`、VIP ごとの Lease `rproxy-vip-<ハッシュ>`）は Helm が消す。DaemonSet の Pod が入れ替わるとき、`vip` のコンテナは SIGTERM で VIP をノードのインタフェースから外してから終わる。
3. Kustomize（`config/samples/fleet-vip`）で入れていたなら、overlay から外して手で消す：

```bash
kubectl -n rproxy-gateway-system delete lease -l app.kubernetes.io/component=vip
kubectl -n rproxy-gateway-system delete serviceaccount,role,rolebinding rproxy-gateway-vip
kubectl delete clusterrole,clusterrolebinding rproxy-gateway-vip
```

   ConfigMap `rproxy-gateway-config` に残った `RPROXY_GATEWAY_FLEET_VIPS` は v0.4.5 のコントローラが読まない（消してよい）。v0.4.5 のコントローラはこの Lease を読まず、作りもしない。

4. `vip` のコンテナが SIGTERM を受けずに消えた（ノードの喪失など）なら、VIP がノードに残っていないか確かめる：

```bash
ip -br addr | grep 192.0.2.10            # ノードで
ip addr del 192.0.2.10/32 dev eth0       # 残っていれば（IPv6 は /128）
```
