# Tailscale 远程连接配置

办公笔记本 ←→ 家里 PC（`192.168.0.61`，局域网私有地址，办公网络无法直连）。
用 Tailscale 组 mesh VPN 打通两个网络，穿透 NAT/CGNAT，无需公网 IP、无需改路由器。

内核 `6.8.0-124-generic` 为 Ubuntu/Debian 系，以下以此为主，附 NixOS 写法。

## 家里 PC（192.168.0.61）

```sh
# 安装（官方一键脚本，Ubuntu/Debian/多数发行版通用）
curl -fsSL https://tailscale.com/install.sh | sh

# 启动并登录（会打印一个 URL，浏览器打开授权）
sudo tailscale up

# 可选：启用 Tailscale 内置 SSH，免维护 sshd 密钥
sudo tailscale up --ssh

# 查看分到的虚拟 IP（100.x.x.x）和设备名
tailscale ip -4
tailscale status
```

> 被连端建议在 admin 后台（login.tailscale.com）对这台机器点 **Disable key expiry**，
> 否则默认 ~6 个月 key 过期需重新登录。

## 办公笔记本

```sh
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up            # 同一账号登录，授权后两台互通
tailscale status             # 应能看到家里 PC 那一行
```

## 验证连通

```sh
# 笔记本上 ping 家里 PC 的 tailscale IP
tailscale ping <home-pc-tailscale-ip>

# 直接 ssh（用 tailscale IP，或开了 MagicDNS 后用设备名）
ssh user@<home-pc-tailscale-ip>
ssh user@<home-pc-hostname>
```

## 要点

- **同一账号/tailnet**：两端 `tailscale up` 必须登录同一账号，否则看不到彼此。
- **MagicDNS**：admin 后台（login.tailscale.com → DNS）打开后，可用机器名代替 IP。
- **家里 PC key 过期**：被连端建议禁用 key expiry，避免人在办公室时掉线。
- **`--ssh` 选项**：启用后 Tailscale 基于身份处理 SSH 认证，免维护 `authorized_keys`；
  想继续用现有 sshd + 密钥则不加。

## NixOS 版本（家里 PC 若为 NixOS）

```nix
# configuration.nix
services.tailscale.enable = true;
```

```sh
sudo nixos-rebuild switch
sudo tailscale up
```
