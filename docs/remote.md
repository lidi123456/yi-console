# 远程连接与中继

Yi Console 可以连接三类主机：本机、SSH 服务器，以及经中继服务器连接的电脑。本机的用法见[使用手册](guide.md)。

English: [Remote access and relay](remote.en.md)

## SSH 主机

"主机管理"里添加 SSH 主机，也可以从 `~/.ssh/config` 导入。认证方式支持 ssh-agent、密钥文件、粘贴的私钥和密码，支持跳板机。

第一次连接时会弹出主机指纹让你确认。确认后，客户端检测远程的系统和架构，上传对应的 yi-agentd，再通过 `yi-agentd bridge` 通信。远程机器的 yi-agentd 不监听任何端口。

## 通过中继连电脑（手机或另一台电脑）

电脑在 NAT 后面、手机或另一台电脑直接连不到它时，用中继。下面把被连的电脑称为"目标电脑"。安全设计见[中继安全设计](relay-security.md)。

1. 在一台有公网地址的服务器上部署 `yi-relay`，见下文「部署中继」。中继运营者用 `yi-relay invite` 给每台目标电脑生成一条**中继接入链接**（一次性，默认 24 小时有效）。
2. 在目标电脑的桌面 App 里打开「主机」→ 点右上角「让这台电脑可被连接」，按卡片操作：
   - **第 1 步 连接中继服务器**：
     - 粘贴中继接入链接（`yiconsole://relay?...&invite=...`），点「开启」。电脑用链接里的邀请码在中继上登记自己的公钥，之后只靠自己的密钥连接，邀请码随即作废；
     - 这个 App 里已经添加过经中继连接的设备、且中继开启了 `--device-invites` 时，也可以直接点「使用 xxx」：由那台电脑向中继要一个一次性邀请；
     - 没有链接时点「没有链接？手动填写中继地址和邀请码」，填中继地址、邀请码和证书指纹。
     连不上时卡片上会直接说明原因（没登记、邀请码失效、被中继移除、指纹不对、地址或网络不通）。
   - **第 2 步 在另一台设备上添加这台电脑**：点「生成配对二维码」，得到一个**一次性**的二维码和配对链接，10 分钟内有效，只能配对一台设备。
   - **已配对的设备**：列出配对过的手机和电脑，可以改名，或者单独移除某一台（它会立刻断开，之后要重新配对）。
3. 在要发起连接的设备上，打开「主机」→「配对设备」：
   - 手机：点「扫码」扫目标电脑上的二维码。
   - 另一台电脑：把复制的配对链接发过来。打开「配对设备」时，剪贴板里有配对链接就会自动填上，并显示将要添加的设备名，点「添加并连接」即可。

   添加时这台设备会生成自己的密钥，用配对码和目标电脑完成配对；保存下来的是这台设备自己的凭据，配对链接用完就没用了。添加后目标电脑会出现在主机列表里，标为"经中继连接"，会话、历史、文件、终端都能用。目标电脑要保持 yi-agentd 在运行，想开机就能连，在目标电脑上注册自启服务（见[使用手册](guide.md#本机后台服务)）。

没有图形界面的电脑用命令行：
```bash
yi-agentd relay setup 'yiconsole://relay?...'                         # 用接入链接登记并开启
yi-agentd relay setup wss://relay.example.com --invite <邀请码> [--fp <证书指纹>]
yi-agentd relay pair                                                 # 终端里打印一次性配对二维码和链接
yi-agentd relay clients                                              # 已配对的设备
yi-agentd relay revoke <设备 id>                                      # 移除一台
yi-agentd relay reset                                                # 清空所有配对
```

电脑所在网络只能经代理出网时（公司内网常见），yi-agentd 会自动走代理。代理按以下顺序查找：`HTTPS_PROXY`（`wss://` 地址用）或 `HTTP_PROXY`（`ws://` 地址用），然后是 `ALL_PROXY`，Windows 上最后再看系统代理设置。`NO_PROXY` 里列出的主机直连。内网地址（10.x、172.16–31.x、192.168.x 等）也直连。只支持 HTTP 代理。

桌面 / 手机客户端连 SSH 主机和中继时同样默认走代理，规则同上。可以在「系统设置」页的「网络代理」卡片里改成手动指定或不使用代理，设置保存在客户端数据目录的 `client-settings.json`。SSH 经代理建隧道失败时（公司代理常只放行 443 端口）会退回直连。

安全要点：
- 每台电脑、每台客户端都有自己的密钥，私钥只留在自己机器上。中继只知道公钥，看不到内容，也冒充不了任何一方。
- 端到端用 Noise IK 握手（X25519 + ChaCha20-Poly1305），有前向保密：日后密钥泄露，之前录下的流量也解不开。
- 配对二维码里只有电脑的公钥和一次性配对码，用过或过期后泄露也没关系。怀疑某台设备丢失时在「已配对的设备」里移除它；全部作废用「更多 → 重置所有配对」。

## 部署中继

有域名时，建议放在 Caddy 或 Nginx 后面，由它们负责 TLS：

```bash
yi-relay --listen 127.0.0.1:7878 --data /var/lib/yi-relay \
          --public-url wss://relay.example.com --trusted-proxy 127.0.0.1
```

```
relay.example.com {
    reverse_proxy 127.0.0.1:7878
}
```

Nginx：

```nginx
location /v1/ {
    proxy_pass http://127.0.0.1:7878;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    proxy_buffering off;
}
```

`--trusted-proxy` 填反向代理的地址（可写多次，支持网段）。只有来自这些地址的连接，中继才读 `X-Forwarded-For` 取真实客户端 IP；不配的话，经代理进来的连接都算成代理的 IP，单 IP 限流和拉黑会误伤所有人。中继地址可以带路径前缀（如 `wss://example.com/hub`），代理把前缀去掉再转给中继即可。

没有域名时，用 `--self-signed`：

```bash
yi-relay --listen 0.0.0.0:7878 --data /var/lib/yi-relay --self-signed --public-url wss://<IP>:7878
```

首次启动会在数据目录生成一张自签名证书，日志里打印它的 SHA-256 指纹。指纹会自动写进邀请链接和配对二维码，客户端连接时只认这张证书。证书文件不删，指纹就不会变。

自签名证书只适合客户端直连中继，或者代理只做四层透传（TCP 转发、不解 TLS）。代理终结 TLS 时客户端看到的是代理的证书，指纹一定对不上：这时代理要配正规证书，中继不用 `--self-signed`。同时配了 `--self-signed` 和 `--trusted-proxy` 的，中继启动时会警告，并且不把指纹写进链接。四层透传拿不到 `X-Forwarded-For`，所有连接都算作代理的 IP，人多时要调大 `--max-per-ip`。

也可以由 yi-relay 自己加载正式证书：`--tls-cert cert.pem --tls-key key.pem`。

管理电脑（在中继服务器上执行，直接读写数据目录，运行中的中继 5 秒内生效）：

```bash
yi-relay invite  --data /var/lib/yi-relay [--ttl 24h] [--uses 1] [--note 备注]   # 生成接入链接
yi-relay invites --data /var/lib/yi-relay [--revoke <邀请 id>]                    # 列出 / 作废邀请
yi-relay devices --data /var/lib/yi-relay                                         # 已登记的电脑
yi-relay revoke  <设备 id> [--cascade] --data /var/lib/yi-relay                   # 移除电脑，在线的立刻断开
```

中继第一次启动、还没有任何电脑时，会自动生成一个 24 小时有效的邀请，打印在启动日志里。

安全相关的设置：

- 准入：电脑必须凭邀请码登记（一次性、会过期，中继只存哈希），之后用自己的 Ed25519 密钥签名证明身份；手机等客户端不需要中继的任何凭据，中继只放行目标电脑授权过的客户端。URL 里不带任何密钥。
- `--device-invites`：允许已登记的电脑代发一次性、10 分钟有效的邀请（App 里的「使用 xxx」一键复用）。默认关闭；多人共用中继时建议保持关闭。代发记录可以在 `yi-relay devices` 里看到，`revoke --cascade` 会连同它邀请的电脑一起移除。
- `--open`：任何人都能直接登记电脑，不需要邀请，只适合公共中继。默认关闭。
- 防止连接耗尽：升级和鉴权各须在 10 秒内完成；默认全局最多 1000 个连接（`--max-conns`），每个 IP 最多 32 个（`--max-per-ip`）；同一 IP 在 10 分钟内出现 10 次可疑失败（签名错、格式错、超时、猜邀请码或配对码），会被拉黑 10 分钟。已被移除的设备自动重连被拒绝不计入，免得同一 NAT 后面的其他设备被误伤。
