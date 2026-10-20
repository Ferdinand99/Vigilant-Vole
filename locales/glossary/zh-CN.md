# 简体中文术语表 (zh-CN)

Vigilant Vole 界面翻译所用术语。每个术语只定一种译法，全文统一。

| English | 简体中文 | Note |
|---|---|---|
| monitor | 监控项 | Same as Kuma. Counts: `{n} 个监控项` |
| heartbeat | 心跳 | Kuma uses it too; in our UI mostly "check" |
| check (noun) | 检查 | A single run; `{n} 次检查` |
| check now | 立即检查 | |
| uptime | 在线率 | Percentage; Kuma uses 在线时间 |
| downtime | 停机时长 | |
| response time | 响应时间 | |
| incident | 事件 | Public announcement on a status page |
| event (monitor event) | 事件 | Same word; context distinguishes |
| outage | 故障 | |
| maintenance window | 维护窗口 | |
| maintenance | 维护 | |
| status page | 状态页 | Kuma: 状态页面; shortened |
| notification channel | 通知渠道 | |
| notification | 通知 | |
| alert | 告警 | |
| escalation | 升级 | |
| flapping | 抖动 | Status switching up/down repeatedly |
| retry / retries | 重试 | |
| push monitor | 推送监控项 | |
| push URL | 推送 URL | |
| remote agent | 远程探针 | 探针 avoids clash with proxy (代理); Kuma has no agents |
| proxy | 代理 | HTTP proxy only; remote agents are 探针 |
| broker (MQTT/Kafka/RabbitMQ) | Broker | Kept in Latin to avoid clash with 代理 |
| group | 分组 | |
| tag | 标签 | |
| badge | 徽章 | |
| API token | API 令牌 | |
| two-factor authentication | 双重验证 | Short: 两步验证 not used |
| authenticator app | 验证器应用 | |
| recovery code | 恢复码 | |
| backup | 备份 | |
| restore | 恢复 | |
| dashboard | 仪表盘 | Same as Kuma |
| settings | 设置 | |
| user | 用户 | |
| role | 角色 | |
| viewer | 查看者 | |
| editor | 编辑者 | |
| admin | 管理员 | |
| certificate | 证书 | |
| expiry / expires | 到期 | |
| retention | 保留 | "Keep check history for" = 检查历史保留 |
| paused | 已暂停 | |
| up | 正常 | Same as Kuma |
| down | 故障 | Same as Kuma; "Down" label in events is 故障 |
| pending | 待定 | Kuma 重试中; ours covers retrying too |
| interval | 间隔 | |
| timeout | 超时 | |
| target | 目标 | |
| keyword | 关键字 | |
| webhook | Webhook | Kept in Latin |
| Apprise | Apprise | Product name |
| discovery | 发现 | "Docker discovery" = Docker 发现 |
| import / export | 导入 / 导出 | |
| config as code | 配置即代码 | |
| session | 会话 | |
| language | 语言 | |
| time zone | 时区 | |
| log in / log out | 登录 / 退出登录 | |
| sign in | 登录 | |
| password | 密码 | |
| username | 用户名 | |
| display name | 显示名称 | |
| secret | 密钥 | |
| private key | 私钥 | |
| public URL | 公开 URL | |
| operational | 运行正常 | |
| degraded | 性能下降 | |
| resolved | 已解决 | |
| reminder | 提醒 | |
| upside-down mode | 反转模式 | Same as Kuma |
| draft / published | 草稿 / 已发布 | |
| remote browser | 远程浏览器 | Headless Chromium used by real-browser monitors |
| real browser | 真实浏览器 | |
| headless | 无头 | |
| screenshot | 截图 | |
| game server | 游戏服务器 | |
| server reflection | 服务器反射 | gRPC |
| metadata | 元数据 | gRPC metadata |
| privacy password | 隐私密码 | SNMPv3; authentication password = 认证密码 |
| security level | 安全级别 | |
| engine ID | 引擎 ID | |
| unlock | 解锁 | |
| password-protected page | 受密码保护的页面 | Pill: 受密码保护 |
| page views | 页面浏览量 | Short: 浏览量 |
| unique visitors | 独立访客 | |
| referrer | 来源网站 | |
| Tailscale peer | Tailscale 节点 | |
| MOTD | MOTD | Kept in Latin |
| replica set | 副本集 | |
| analytics | 统计 | |
| logo / browser icon | Logo / 浏览器图标 | |

## Style

- 简体中文，使用中国大陆术语；中性、简洁，标签中不用“你/您”，句子中尽量省略主语，必要时用“你”。
- 全角标点：，。：；？！；引号用 “ ” 和 ‘ ’；省略号 `…` 保留；破折号 —。
- 中文与拉丁字母、数字之间不加空格（Apple 风格）；占位符与中文相邻也不加空格，除非为分隔点“ · ”。
- 产品名、协议和技术术语保持原文（HTTP、TLS、DNS、Docker、Prometheus 等）。
- 无复数形式，`{n}` 后接量词：个（监控项、渠道、标签、用户、令牌、探针）、次（检查）、天、条（事件）。
- 日期：`2026年10月20日`；星期短 `周一`，长 `星期一`；24 小时制。

| cron expression | Cron 表达式 | "cron" kept Latin |
